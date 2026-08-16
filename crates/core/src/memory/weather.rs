//! 天气上下文预喂（对齐 `src/weather.js` 的 wttr.in 抓取 / 缓存 / 格式化段）。
//!
//! 触发关键词（`WEATHER_KEYWORD_RE`）命中且能从消息或用户配置解析出位置时，
//! 拉取 wttr.in 实时天气并把 `## Weather Reference` 块交给注入器渲染；
//! 数据仅作背景参考，不要求模型主动播报。
//!
//! 对齐说明：
//! - 触发词 / 周模式词 / 位置别名 / 中文城市表对齐 Node 的
//!   `WEATHER_KEYWORD_RE` / `WEATHER_WEEK_RE` / `WEATHER_LOCATION_ALIASES`，
//!   城市表采用高频子集（北上广深杭成武南京苏…），拼音直接送 wttr.in 由其对地理编码；
//! - 缓存 TTL 30 分钟，`std::sync::Mutex` 只在命中检查 / 写入时短持锁；
//! - 与 Node 的差异：Node 的 prefeed 无显式关键词门（靠位置解析兜底），
//!   本实现先过 `is_weather_query` 再抓取，避免每轮无谓注入；
//! - 未迁：并发 in-flight 去重（Node `inflightMap`；当前单消费点无需）、
//!   天气卡片 props / `ui_set`（随 ui 工具里程碑）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use crate::db::repositories::config::{get_config, set_config};
use crate::db::Db;
use crate::error::Result;

pub const CACHE_TTL_MS: u64 = 30 * 60 * 1000; // 30 分钟（对齐 Node CACHE_TTL_MS）
const FETCH_TIMEOUT_MS: u64 = 6000;
/// L4（审计修复）：天气缓存条数上限，防不同城市/配置变更导致无界增长。
const WEATHER_CACHE_MAX: usize = 64;

/// 天气注入触发关键词（对齐 capability-registry.js `WEATHER_KEYWORD_RE`）。
fn weather_keyword_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"天气|温度|气温|下雨|降雨|下雪|台风|雾霾|阴天|晴天|多云|wttr|weather")
            .expect("static regex")
    })
}

/// 周模式关键词（对齐 Node `WEATHER_WEEK_RE`）。
fn weather_week_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"一周|一星期|7\s*天|七天|未来一周|接下来一周|下周|week|weekly|7-?day")
            .expect("static regex")
    })
}

/// 消息是否命中天气触发（对齐 Node prefeed 的 gate / detect 正则）。
pub fn is_weather_query(text: &str) -> bool {
    weather_keyword_re().is_match(text)
}

/// 消息是否命中周预报模式。
pub fn is_week_query(text: &str) -> bool {
    weather_week_re().is_match(text)
}

// ── 位置解析（对齐 getLocationFromMessage / normalizeWeatherLocation） ──────

/// 显式别名表 `(正则, wttr.in 查询值, 展示标签)`；优先级高于城市表。
const LOCATION_ALIASES: &[(&str, &str, &str)] = &[
    (r"陆丰", "22.945,115.644", "汕尾陆丰"),
    (r"浦东", "31.2304,121.5440", "上海浦东"),
];

/// 高频中文城市表（对齐 Node `WEATHER_ZH_CITIES` 高频子集；拼音直接送 wttr.in）。
const ZH_CITIES: &[(&str, &str)] = &[
    ("上海", "Shanghai China"),
    ("北京", "Beijing China"),
    ("广州", "Guangzhou Guangdong China"),
    ("深圳", "Shenzhen Guangdong China"),
    ("杭州", "Hangzhou Zhejiang China"),
    ("成都", "Chengdu Sichuan China"),
    ("武汉", "Wuhan Hubei China"),
    ("南京", "Nanjing Jiangsu China"),
    ("苏州", "Suzhou Jiangsu China"),
    ("天津", "Tianjin China"),
    ("重庆", "Chongqing China"),
    ("西安", "Xi'an Shaanxi China"),
    ("长沙", "Changsha Hunan China"),
    ("郑州", "Zhengzhou Henan China"),
    ("青岛", "Qingdao Shandong China"),
    ("厦门", "Xiamen Fujian China"),
    ("哈尔滨", "Harbin Heilongjiang China"),
    ("大连", "Dalian Liaoning China"),
    ("昆明", "Kunming Yunnan China"),
    ("合肥", "Hefei Anhui China"),
];

/// 从消息提取位置并规范化为 wttr.in 能用的查询串。
/// 顺序：显式别名 → 中文城市表（消息包含城市名）→ 用户配置城市 → 空串。
pub fn get_location_from_message(text: &str, user_city: &str) -> String {
    for (re, value, _label) in LOCATION_ALIASES {
        if let Ok(re) = Regex::new(re) {
            if re.is_match(text) {
                return value.to_string();
            }
        }
    }
    for (zh, pinyin) in ZH_CITIES {
        if text.contains(zh) {
            return pinyin.to_string();
        }
    }
    if !user_city.trim().is_empty() {
        // 用户配置城市过一遍城市表（对齐 normalizeWeatherLocation 的精确键查找），
        // 未命中则原样直送 wttr.in（由其地理编码）。
        let city = user_city.trim();
        for (zh, pinyin) in ZH_CITIES {
            if *zh == city {
                return pinyin.to_string();
            }
        }
        return city.to_string();
    }
    String::new()
}

// ── 数据模型 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct CurrentWeather {
    pub temp: Option<f64>,
    pub condition: String,
    pub feels_like: Option<f64>,
    pub humidity: Option<f64>,
    pub wind_kph: Option<f64>,
    pub uv: Option<f64>,
    pub sunrise: String,
    pub sunset: String,
}

#[derive(Debug, Clone, Default)]
pub struct ForecastDay {
    pub date: String,
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub condition: String,
}

#[derive(Debug, Clone, Default)]
pub struct WeatherData {
    pub city: String,
    pub region: String,
    pub current: CurrentWeather,
    pub forecast: Vec<ForecastDay>,
}

// ── 抓取 + 缓存 ─────────────────────────────────────────────────────────────

struct CacheEntry {
    data: WeatherData,
    fetched_at: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// URL 编码（对齐 `encodeURIComponent` 的 UTF-8 字节级转义；仅用于 wttr.in 路径段）。
pub fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 拉取并解析 wttr.in（对齐 `fetchWeatherData`；预报取 7 天，格式化时按模式裁剪）。
async fn fetch_weather_data(location: &str) -> Result<WeatherData> {
    let url = format!("https://wttr.in/{}?format=j1", url_encode(location));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(FETCH_TIMEOUT_MS))
        .build()?;
    let resp = client.get(&url).send().await?;
    let resp = resp.error_for_status()?;
    let data: Value = resp.json().await?;

    let current_cond = data["current_condition"]
        .get(0)
        .cloned()
        .unwrap_or_default();
    let nearest = data["nearest_area"].get(0).cloned().unwrap_or_default();
    let city = nearest["areaName"][0]["value"]
        .as_str()
        .unwrap_or("Unknown")
        .to_string();
    let region = [
        nearest["region"][0]["value"].as_str().unwrap_or(""),
        nearest["country"][0]["value"].as_str().unwrap_or(""),
    ]
    .iter()
    .filter(|s| !s.is_empty())
    .cloned()
    .collect::<Vec<_>>()
    .join(", ");

    let condition = current_cond["lang_zh"][0]["value"]
        .as_str()
        .or_else(|| current_cond["weatherDesc"][0]["value"].as_str())
        .unwrap_or("")
        .to_string();

    let mut forecast = Vec::new();
    if let Some(days) = data["weather"].as_array() {
        for day in days.iter().take(7) {
            let cond = day["hourly"][0]["lang_zh"][0]["value"]
                .as_str()
                .or_else(|| day["hourly"][0]["weatherDesc"][0]["value"].as_str())
                .or_else(|| day["weatherDesc"][0]["value"].as_str())
                .unwrap_or("")
                .to_string();
            forecast.push(ForecastDay {
                date: day["date"].as_str().unwrap_or("").to_string(),
                low: as_f64(&day["mintempC"]),
                high: as_f64(&day["maxtempC"]),
                condition: cond,
            });
        }
    }

    Ok(WeatherData {
        city,
        region,
        current: CurrentWeather {
            temp: as_f64(&current_cond["temp_C"]),
            condition,
            feels_like: as_f64(&current_cond["FeelsLikeC"]),
            humidity: as_f64(&current_cond["humidity"]),
            wind_kph: as_f64(&current_cond["windspeedKmph"]),
            uv: as_f64(&current_cond["uvIndex"]),
            sunrise: current_cond["sunrise"].as_str().unwrap_or("").to_string(),
            sunset: current_cond["sunset"].as_str().unwrap_or("").to_string(),
        },
        forecast,
    })
}

/// 缓存优先的天气拉取（对齐 `fetchAndCacheWeather`；30 分钟内直接返回缓存）。
async fn fetch_and_cache_weather(location: &str) -> Option<WeatherData> {
    let key = location.to_string();
    {
        let guard = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = guard.get(&key) {
            if entry.fetched_at.elapsed().as_millis() < CACHE_TTL_MS as u128 {
                return Some(entry.data.clone());
            }
        }
    }
    match fetch_weather_data(location).await {
        Ok(data) => {
            if let Ok(mut guard) = cache().lock() {
                guard.insert(
                    key,
                    CacheEntry {
                        data: data.clone(),
                        fetched_at: Instant::now(),
                    },
                );
                // L4（审计修复）：超限先淘汰过期项，仍超限则清空（城市数有限，代价可接受）。
                if guard.len() > WEATHER_CACHE_MAX {
                    let now = Instant::now();
                    guard.retain(|_, e| {
                        now.duration_since(e.fetched_at).as_millis() < CACHE_TTL_MS as u128
                    });
                    if guard.len() > WEATHER_CACHE_MAX {
                        guard.clear();
                    }
                }
            }
            Some(data)
        }
        Err(err) => {
            tracing::warn!(%err, "weather prefeed fetch failed, skipping injection");
            None
        }
    }
}

// ── 格式化（对齐 weatherToRuntimeContext 的 `## Weather Reference` 块） ─────

/// 渲染 `## Weather Reference` 上下文块；current 模式取前 3 天，week 模式取 7 天。
pub fn format_weather_reference(data: &WeatherData, mode: &str) -> String {
    let c = &data.current;
    let mut lines = Vec::new();
    lines.push(format!(
        "当前城市：{}{}",
        data.city,
        if data.region.is_empty() {
            String::new()
        } else {
            format!("（{}）", data.region)
        }
    ));
    let temp = c
        .temp
        .map(|t| format!("当前温度：{t}°C"))
        .unwrap_or_else(|| "当前温度：未知".to_string());
    lines.push(temp);
    if !c.condition.is_empty() {
        lines.push(format!("天气：{}", c.condition));
    }
    if let Some(f) = c.feels_like {
        lines.push(format!("体感温度：{f}°C"));
    }
    if let Some(h) = c.humidity {
        lines.push(format!("湿度：{h}%"));
    }
    if let Some(w) = c.wind_kph {
        lines.push(format!("风速：{w} km/h"));
    }
    if let Some(u) = c.uv {
        lines.push(format!("紫外线：{u}"));
    }
    if !c.sunrise.is_empty() || !c.sunset.is_empty() {
        lines.push(format!("日出：{}  日落：{}", c.sunrise, c.sunset));
    }

    let days = if mode == "week" { 7 } else { 3 };
    if !data.forecast.is_empty() {
        lines.push(format!("未来 {} 天预报：", data.forecast.len().min(days)));
        for day in data.forecast.iter().take(days) {
            let low = day
                .low
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".to_string());
            let high = day
                .high
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".to_string());
            let cond = if day.condition.is_empty() {
                String::new()
            } else {
                format!(" {}", day.condition)
            };
            lines.push(format!("{}：{}~{}°C{}", day.date, low, high, cond));
        }
    }

    format!(
        "## Weather Reference\n\n数据源：wttr.in（仅作背景参考；除非用户主动要求，否则不要主动播报或总结天气。）\n\n{}",
        lines.join("\n")
    )
}

// ── 能力预喂入口（对齐 buildWeatherRuntimeContext） ─────────────────────────

/// 用户常驻城市（config 表 `user_location`；对齐 Node profile.city 的兜底角色）。
pub fn set_user_location(db: &Db, location: &str) -> Result<()> {
    set_config(db, "user_location", location)
}

/// 构建天气预喂文本：未命中天气关键词 / 无法解析位置 / 抓取失败 → 空串。
pub async fn build_weather_runtime_context(text: &str, db: &Db) -> String {
    if !is_weather_query(text) {
        return String::new();
    }
    let user_city = match get_config(db, "user_location") {
        Ok(Some(v)) => v,
        _ => String::new(),
    };
    let location = get_location_from_message(text, &user_city);
    if location.is_empty() {
        return String::new();
    }
    let mode = if is_week_query(text) {
        "week"
    } else {
        "current"
    };
    match fetch_and_cache_weather(&location).await {
        Some(data) => format_weather_reference(&data, mode),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_detection() {
        assert!(is_weather_query("今天上海天气怎么样"));
        assert!(is_weather_query("明天温度多少"));
        assert!(is_weather_query("What's the weather in Beijing"));
        assert!(!is_weather_query("帮我写一段 Rust 代码"));
    }

    #[test]
    fn week_mode_detection() {
        assert!(is_week_query("未来一周天气"));
        assert!(is_week_query("下周天气"));
        assert!(is_week_query("7天天气预报"));
        assert!(!is_week_query("今天天气"));
    }

    #[test]
    fn location_from_message() {
        // 中文城市表命中
        assert_eq!(
            get_location_from_message("上海今天天气", ""),
            "Shanghai China"
        );
        // 别名优先（陆丰 → 经纬度）
        assert_eq!(get_location_from_message("陆丰天气", ""), "22.945,115.644");
        // 消息无城市 → 用户配置兜底
        assert_eq!(
            get_location_from_message("今天天气", "杭州"),
            "Hangzhou Zhejiang China"
        );
        // 无城市无配置 → 空（不注入）
        assert_eq!(get_location_from_message("今天天气", ""), "");
        // 未命中天气但含城市（调用方已 gate，此处仅验证提取）
        assert_eq!(get_location_from_message("北京好玩吗", ""), "Beijing China");
    }

    #[test]
    fn url_encoding() {
        assert_eq!(url_encode("Shanghai China"), "Shanghai%20China");
        assert_eq!(url_encode("abc-._~"), "abc-._~");
    }

    #[test]
    fn format_reference_contains_anchor() {
        let data = WeatherData {
            city: "Shanghai".to_string(),
            region: "Shanghai, China".to_string(),
            current: CurrentWeather {
                temp: Some(26.0),
                condition: "晴".to_string(),
                feels_like: Some(27.5),
                humidity: Some(60.0),
                wind_kph: Some(12.0),
                uv: Some(5.0),
                sunrise: "05:12 AM".to_string(),
                sunset: "06:44 PM".to_string(),
            },
            forecast: vec![ForecastDay {
                date: "2026-08-09".to_string(),
                low: Some(22.0),
                high: Some(30.0),
                condition: "晴".to_string(),
            }],
        };
        let text = format_weather_reference(&data, "current");
        assert!(text.starts_with("## Weather Reference"));
        assert!(text.contains("数据源：wttr.in"));
        assert!(text.contains("当前温度：26°C"));
        assert!(text.contains("体感温度：27.5°C"));
        assert!(text.contains("2026-08-09：22~30°C 晴"));
        assert!(text.contains("日出：05:12 AM"));
    }

    #[test]
    fn unknown_temp_fallback() {
        let data = WeatherData {
            city: "X".to_string(),
            ..Default::default()
        };
        let text = format_weather_reference(&data, "current");
        assert!(text.contains("当前温度：未知"));
    }

    /// 联网集成测试（wttr.in）；默认不跑：`cargo test -- --ignored`。
    #[tokio::test]
    #[ignore]
    async fn fetch_live_weather() {
        let data = fetch_weather_data("Shanghai China")
            .await
            .expect("wttr.in reachable");
        assert!(!data.city.is_empty());
        assert!(!data.forecast.is_empty());
        assert!(data.current.temp.is_some());
    }
}
