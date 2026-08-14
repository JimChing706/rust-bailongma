//! 网络类工具（对齐 Node `capabilities/tools/web/*` 与 `shell.js` 的 download_file）：
//!
//! | 工具 | 说明 |
//! |---|---|
//! | `web_search` | 多引擎搜索：第一梯队带 key JSON API（serper/brave/tavily/searxng，串行），第二梯队无 key 爬虫兜底（bing/jina/ddg，并行抢答）；10 分钟 LRU 缓存（200 条） |
//! | `web_read` | 读取网页为可读文本：直连 HTTP → Jina Reader 兜底；长文落盘 `sandbox/articles/{YYYY-MM}/`；URL 分级缓存 |
//! | `fetch_url` | 抓取 URL 原始响应（status/headers/body 原文，body 截断 64KB） |
//! | `download_file` | 下载 URL 到沙箱内文件（流式写临时文件后原子重命名，超时默认 120s） |
//!
//! 同步 executor 内通过模块级 [`RT`]（current_thread tokio runtime）跑 async reqwest，
//! 与 Node 的 fetch 行为对齐（UA/头、重定向、内容类型过滤、低价值页面检测）。
//!
//! SSRF 防护（对齐 Node `assertBrowserUrlAllowed`）：web_read / fetch_url / download_file
//! 请求前与每次重定向都校验目标 URL——拒绝非 http/https 协议、带凭据 URL、localhost 与
//! 私网/本机/云元数据地址（169.254.169.254 等），域名则解析后检查防 DNS rebinding；
//! `allow_lan_access=true`（对齐 Node config.network.allowLanAccess）时放行本机/私网。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use futures_util::StreamExt;
use regex::Regex;
use scraper::{Html, Node, Selector};
use serde_json::{json, Value};
use std::sync::LazyLock;

use super::{resolve_under_root, NativeToolExecutor};
use crate::error::{CoreError, Result};
use crate::llm::tools::{boolean_param, enum_param, integer_param, string_param, ToolSchema};
use base64::Engine;

// ─────────────────────────────────────────────────────────────
// 常量
// ─────────────────────────────────────────────────────────────

/// 浏览器 UA（对齐 Node WEB_HEADERS）
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
/// web_search 缓存 TTL（对齐 Node SEARCH_CACHE_TTL_MS）
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(600);
/// web_search 缓存条数上限（对齐 Node SEARCH_CACHE_MAX）
const SEARCH_CACHE_MAX: usize = 200;
/// 搜索结果字段截断上限（对齐 Node SEARCH_TITLE_MAX / SEARCH_SNIPPET_MAX）
const SEARCH_TITLE_MAX: usize = 200;
const SEARCH_SNIPPET_MAX: usize = 300;
/// 搜索 limit 默认/上限（对齐 Node schema：默认 5，最大 8）
const SEARCH_LIMIT_DEFAULT: u64 = 5;
const SEARCH_LIMIT_MAX: u64 = 8;
/// 长文阈值/摘要长度（对齐 Node util.js）
const ARTICLE_LENGTH_THRESHOLD: usize = 2000;
const ARTICLE_SUMMARY_EXCERPT: usize = 800;
/// web_read max_chars 默认/上限（对齐 Node schema：1000-20000，默认 5000）
const DEFAULT_MAX_CHARS: u64 = 5000;
const MAX_CHARS_LIMIT: u64 = 20000;
/// web_read timeout_ms 默认/上限（对齐 Node schema：1000-45000，默认 20000）
const DEFAULT_TIMEOUT_MS: u64 = 20000;
const MAX_TIMEOUT_MS: u64 = 45000;
/// fetch_url 默认超时（毫秒）
const FETCH_URL_TIMEOUT_MS: u64 = 12000;
/// fetch_url body 截断（字节）
const FETCH_URL_BODY_CAP: usize = 64 * 1024;
/// download_file 超时（秒，对齐 Node schema：默认 120，最大 120）
const DEFAULT_DL_TIMEOUT_SEC: u64 = 120;
const MAX_DL_TIMEOUT_SEC: u64 = 120;
/// web_read 长文落盘目录（沙箱内）
const ARTICLES_DIR: &str = "articles";

// ─────────────────────────────────────────────────────────────
// 共享运行时 / HTTP client
// ─────────────────────────────────────────────────────────────

/// 同步 executor 内跑 async reqwest 的 current_thread runtime。
static RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build web io runtime")
});

fn block_on<F: Future>(fut: F) -> F::Output {
    RT.block_on(fut)
}

/// 共享 HTTP client（浏览器头 + 连接超时 + 有限重定向），供不校验用户 URL 的
/// 爬虫/搜索请求使用（URL 均为代码内构造的公网地址）。
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(WEB_USER_AGENT)
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(6))
        .build()
        .expect("failed to build web http client")
});

/// 构建带 SSRF 重定向策略的 client（公共配置与 [`HTTP`] 一致）。
/// reqwest 0.12 移除了 `RequestBuilder::redirect`，重定向策略只能在
/// client 构建时注入，故按 `allow_lan` 固定两个实例。
fn http_client(allow_lan: bool) -> &'static reqwest::Client {
    static HTTP_SAFE: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent(WEB_USER_AGENT)
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(30))
            .redirect(redirect_policy(false))
            .build()
            .expect("failed to build ssrf-safe http client")
    });
    static HTTP_LAN: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent(WEB_USER_AGENT)
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(30))
            .redirect(redirect_policy(true))
            .build()
            .expect("failed to build lan-allowed http client")
    });
    if allow_lan {
        &HTTP_LAN
    } else {
        &HTTP_SAFE
    }
}

// ─────────────────────────────────────────────────────────────
// 缓存（模块级 static，进程内共享；对齐 Node Map LRU）
// ─────────────────────────────────────────────────────────────

struct SearchCacheEntry {
    payload: Value,
    fetched_at: Instant,
}

static SEARCH_CACHE: LazyLock<Mutex<HashMap<String, SearchCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn search_cache_get(key: &str) -> Option<Value> {
    let mut cache = SEARCH_CACHE.lock().expect("search cache poisoned");
    let entry = cache.get(key)?;
    if entry.fetched_at.elapsed() >= SEARCH_CACHE_TTL {
        cache.remove(key);
        return None;
    }
    let payload = entry.payload.clone();
    // 删除重插以刷新“最近访问”（近似 LRU 序）
    let fetched_at = entry.fetched_at;
    cache.remove(key);
    cache.insert(
        key.to_string(),
        SearchCacheEntry { payload: payload.clone(), fetched_at },
    );
    Some(payload)
}

fn search_cache_set(key: &str, payload: Value) {
    let mut cache = SEARCH_CACHE.lock().expect("search cache poisoned");
    cache.remove(key);
    cache.insert(
        key.to_string(),
        SearchCacheEntry { payload, fetched_at: Instant::now() },
    );
    // 超量直接清空（HashMap 无法按序淘汰；200 条内极少触发，代价可接受）
    if cache.len() > SEARCH_CACHE_MAX {
        cache.clear();
    }
}

/// web_read URL 缓存：key → (payload, fetched_at)
static READ_CACHE: LazyLock<Mutex<HashMap<String, (Value, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn url_ttl(url: &str) -> Duration {
    let v = url.to_lowercase();
    if v.contains("wttr.in") || v.contains("weather") || v.contains("openweather") || v.contains("tianqi")
    {
        Duration::from_secs(600)
    } else if v.contains("news") || v.contains("rss") || v.contains("feed") {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(3600)
    }
}

// ─────────────────────────────────────────────────────────────
// 正则（LazyLock 编译一次）
// ─────────────────────────────────────────────────────────────

static SCRIPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<script[\s\S]*?</script>").unwrap());
static STYLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<style[\s\S]*?</style>").unwrap());
static NOSCRIPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<noscript[\s\S]*?</noscript>").unwrap());
static BR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").unwrap());
static BLOCK_END_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</(p|div|section|article|li|h[1-6])>").unwrap());
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]{2,}").unwrap());
static NEWLINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());
static NUM_DEC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#(\d+);").unwrap());
static NUM_HEX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)&#x([0-9a-f]+);").unwrap());
static TITLE_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<title[^>]*>([\s\S]*?)</title>").unwrap());
static LOW_VALUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(please wait|just a moment|checking your browser|enable javascript|access denied|forbidden|captcha|安全验证|请稍候|请稍等|正在验证|访问受限)",
    )
    .unwrap()
});
static CT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)text|html|xml|json").unwrap());
static API_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\.json(?:\?|#|$)|[?&](format|output|alt)=json\b|/api/|/(rest|graphql)/)").unwrap()
});
/// Bing 结果块：`<li class="b_algo">...<h2><a href="URL">Title</a>...<p...>snippet</p>`
static BING_ITEM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)<li class="b_algo"[\s\S]*?<h2[^>]*>\s*<a[^>]+href="(https?://[^"]+)"[^>]*>([\s\S]*?)</a>[\s\S]*?(?:<p[^>]*class="[^"]*b_lineclamp[^"]*"[^>]*>([\s\S]*?)</p>|<p[^>]*>([\s\S]{30,}?)</p>)?"#,
    )
    .unwrap()
});
/// DDG 结果：`<a class="result__a" href="URL">Title</a>`，其后块内找 `class="result__snippet"`
static DDG_ITEM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<a[^>]+class="result__a"[^>]+href="([^"]+)"[^>]*>([\s\S]*?)</a>"#).unwrap()
});
static DDG_SNIPPET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)class="result__snippet"[^>]*>([\s\S]*?)</a>|class="result__snippet"[^>]*>([\s\S]*?)</div>"#,
    )
    .unwrap()
});
static NEXT_DDG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<a[^>]+class="result__a""#).unwrap());
/// Jina Search 结果块：`[1] 标题 / URL: ... / Description: ...`
static JINA_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[\d+\]\s*(.+)").unwrap());
static JINA_URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^URL:\s*(\S+)").unwrap());
static JINA_DESC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Description:\s*(.+)").unwrap());
/// Bing 反爬/验证页检测
static BING_BLOCKED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)sorry|captcha|verify|访问被拒绝").unwrap());
/// Bing ck/a 中转链接检测
static BING_CK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)bing\.com/ck/a").unwrap());

// ─────────────────────────────────────────────────────────────
// 工具入口（与 sys_tools 同签名）
// ─────────────────────────────────────────────────────────────

/// `web_search(query, limit?)`：多引擎搜索。
pub fn web_search_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Tool("web_search 缺 query".into()))?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(SEARCH_LIMIT_DEFAULT)
        .clamp(1, SEARCH_LIMIT_MAX) as usize;

    let cache_key = format!("{query}::{limit}");
    if let Some(cached) = search_cache_get(&cache_key) {
        return Ok(cached);
    }

    let keys = ex.web_keys.clone().unwrap_or_default();
    let mut failures: Vec<Value> = Vec::new();

    // 第一梯队：带 key JSON API，串行（未配置瞬间跳过，不拖慢）
    let mut tier1_result: Option<(String, Vec<Value>)> = None;
    if let Some(k) = keys.serper_key.clone() {
        match block_on(search_serper(&k, query, limit)) {
            Ok(items) if !items.is_empty() => {
                tier1_result = Some(("serper".into(), normalize_results(items, limit)))
            }
            Ok(_) => failures.push(json!({ "engine": "serper", "reason": "empty results" })),
            Err(e) => failures.push(json!({ "engine": "serper", "reason": e })),
        }
    }
    if tier1_result.is_none() {
        if let Some(k) = keys.brave_key.clone() {
            match block_on(search_brave(&k, query, limit)) {
                Ok(items) if !items.is_empty() => {
                    tier1_result = Some(("brave".into(), normalize_results(items, limit)))
                }
                Ok(_) => failures.push(json!({ "engine": "brave", "reason": "empty results" })),
                Err(e) => failures.push(json!({ "engine": "brave", "reason": e })),
            }
        }
    }
    if tier1_result.is_none() {
        if let Some(k) = keys.tavily_key.clone() {
            match block_on(search_tavily(&k, query, limit)) {
                Ok(items) if !items.is_empty() => {
                    tier1_result = Some(("tavily".into(), normalize_results(items, limit)))
                }
                Ok(_) => failures.push(json!({ "engine": "tavily", "reason": "empty results" })),
                Err(e) => failures.push(json!({ "engine": "tavily", "reason": e })),
            }
        }
    }
    if tier1_result.is_none() {
        if let Some(base) = keys.searxng_url.clone() {
            match block_on(search_searxng(&base, query, limit)) {
                Ok(items) if !items.is_empty() => {
                    tier1_result = Some(("searxng".into(), normalize_results(items, limit)))
                }
                Ok(_) => failures.push(json!({ "engine": "searxng", "reason": "empty results" })),
                Err(e) => failures.push(json!({ "engine": "searxng", "reason": e })),
            }
        }
    }
    if let Some((source, results)) = tier1_result {
        let payload = build_search_payload(query, &source, results);
        search_cache_set(&cache_key, payload.clone());
        return Ok(payload);
    }

    // 第二梯队：无 key 爬虫兜底，并行抢答（最坏耗时压成单引擎超时 ~18s）
    let futs: Vec<SearchFut<'_>> = vec![
        Box::pin(search_bing(query, limit)),
        Box::pin(search_jina(query, limit)),
        Box::pin(search_ddg(query, limit)),
    ];
    let raced = block_on(join_all(futs));
    for (name, res) in ["bing", "jina", "ddg"].iter().zip(raced) {
        match res {
            Ok(items) if !items.is_empty() => {
                let payload = build_search_payload(query, name, normalize_results(items, limit));
                search_cache_set(&cache_key, payload.clone());
                return Ok(payload);
            }
            Ok(_) => failures.push(json!({ "engine": name, "reason": "empty results" })),
            Err(e) => failures.push(json!({ "engine": name, "reason": e })),
        }
    }

    let summary = if failures.is_empty() {
        "no engine configured".to_string()
    } else {
        failures
            .iter()
            .map(|f| {
                format!(
                    "{}: {}",
                    f["engine"].as_str().unwrap_or("?"),
                    f["reason"].as_str().unwrap_or("?")
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    Ok(json!({
        "ok": false, "tool": "web_search", "query": query,
        "error": format!("all search engines failed ({summary})"),
        "failures": failures,
        "hint": "所有搜索引擎都失败了。可以尝试用已知 URL 直接 web_read，或在设置中配置 SERPER_API_KEY / BRAVE_API_KEY 以获得稳定搜索。",
    }))
}

fn build_search_payload(query: &str, source: &str, results: Vec<Value>) -> Value {
    json!({
        "ok": true, "tool": "web_search", "query": query,
        "source": source,
        "results": results,
        "hint": "用 web_read 打开 1-3 个可靠的结果 URL，再回答用户。",
    })
}

/// `web_read(url, render?, fresh?, remote_fallback?, timeout_ms?, max_chars?)`。
pub fn web_read_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let raw_url = args
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Tool("web_read 缺 url".into()))?;
    let url = normalize_url(raw_url);

    let render = args.get("render").and_then(Value::as_str).unwrap_or("auto");
    if !["auto", "http", "browser"].contains(&render) {
        return Err(CoreError::Tool(format!("invalid render mode: {render}")));
    }
    let remote_fallback = args
        .get("remote_fallback")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1_000, MAX_TIMEOUT_MS);
    let max_chars = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1_000, MAX_CHARS_LIMIT) as usize;
    let fresh = args.get("fresh").and_then(Value::as_bool).unwrap_or(false);

    let cache_key = format!("{url}::{render}::{remote_fallback}");
    if !fresh {
        if let Some((payload, fetched_at)) = READ_CACHE
            .lock()
            .expect("read cache poisoned")
            .get(&cache_key)
            .cloned()
        {
            if fetched_at.elapsed() < url_ttl(&url) {
                let mut payload = payload;
                payload["cached"] = json!(true);
                payload["cache_age_minutes"] = json!(fetched_at.elapsed().as_secs() / 60);
                return Ok(payload);
            }
        }
    }

    let jina_key = ex.web_keys.as_ref().and_then(|k| k.jina_key.clone());
    let mut failures: Vec<Value> = Vec::new();

    // 策略零：本地浏览器渲染（仅 render=browser 时启用；对齐 Node 历史 browser_read 路径）
    if render == "browser" {
        let br = render_with_browser(&url, timeout_ms, max_chars);
        if br.ok && !is_low_value_page_text(&br.text) {
            let outcome = ReadOutcome {
                url: &url,
                final_url: &br.final_url,
                status: None,
                source: "browser",
                title: &br.title,
                text: &br.text,
                is_json: false,
            };
            let payload = build_read_payload(ex, &outcome, max_chars);
            READ_CACHE
                .lock()
                .expect("read cache poisoned")
                .insert(cache_key.clone(), (payload.clone(), Instant::now()));
            return Ok(payload);
        }
        failures.push(json!({
            "strategy": "browser",
            "code": br.error.as_ref().map(|e| if e.contains("未找到") { "NO_BROWSER" } else { "BROWSER_FAILED" }),
            "error": br.error,
            "low_value": br.ok && is_low_value_page_text(&br.text),
        }));
    }

    // 策略一：受保护直连 HTTP
    let allow_lan = ex.allow_lan_access;
    let direct = block_on(fetch_via_direct(&url, timeout_ms, is_likely_api_url(&url), allow_lan));
    if direct.ok {
        let outcome = ReadOutcome {
            url: &url,
            final_url: &direct.final_url,
            status: direct.status,
            source: "http",
            title: &direct.title,
            text: &direct.body,
            is_json: direct.is_json,
        };
        let payload = build_read_payload(ex, &outcome, max_chars);
        READ_CACHE
            .lock()
            .expect("read cache poisoned")
            .insert(cache_key.clone(), (payload.clone(), Instant::now()));
        return Ok(payload);
    }
    failures.push(json!({
        "strategy": "http",
        "status": direct.status,
        "content_type": direct.content_type,
        "code": direct.code,
        "error": direct.error,
        "low_value": direct.low_value,
    }));
    if render == "http" {
        let error = direct.error.unwrap_or_else(|| "HTTP read failed".into());
        return Ok(json!({
            "ok": false, "tool": "web_read", "url": url,
            "failures": failures,
            "error": error,
        }));
    }

    // 策略二：Jina Reader 兜底
    if remote_fallback {
        match block_on(fetch_via_jina(&url, timeout_ms, jina_key.as_deref())) {
            Ok(jr) => {
                let outcome = ReadOutcome {
                    url: &url,
                    final_url: &jr.final_url,
                    status: None,
                    source: "jina",
                    title: &jr.title,
                    text: &jr.body,
                    is_json: false,
                };
                let mut payload = build_read_payload(ex, &outcome, max_chars);
                payload["remote_fallback"] = json!(true);
                READ_CACHE
                    .lock()
                    .expect("read cache poisoned")
                    .insert(cache_key.clone(), (payload.clone(), Instant::now()));
                return Ok(payload);
            }
            Err(e) => failures.push(json!({ "strategy": "jina", "error": e })),
        }
    }

    Ok(json!({
        "ok": false, "tool": "web_read", "url": url,
        "error": "all enabled read strategies failed",
        "failures": failures,
        "hint": "从 web_search 结果中换一个可靠的 URL 再试。",
    }))
}

/// `fetch_url(url, timeout_ms?)`：抓取 URL 原始响应（status/headers/body 原文）。
pub fn fetch_url_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let raw_url = args
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Tool("fetch_url 缺 url".into()))?;
    let url = normalize_url(raw_url);
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(FETCH_URL_TIMEOUT_MS)
        .clamp(1_000, MAX_TIMEOUT_MS);
    let allow_lan = ex.allow_lan_access;

    block_on(async move {
        // SSRF 防护：请求前校验 + 每跳重定向校验
        let parsed = reqwest::Url::parse(&url)
            .map_err(|e| CoreError::Tool(format!("URL 无效: {e}")))?;
        check_url_ssrf(&parsed, allow_lan).map_err(CoreError::Tool)?;
        let resp = http_client(allow_lan)
            .get(&url)
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .await
            .map_err(|e| CoreError::Tool(format!("fetch_url 网络错误: {e}")))?;
        let status = resp.status().as_u16();
        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let headers: HashMap<String, String> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string())))
            .collect();
        let headers = serde_json::to_value(headers).unwrap_or(Value::Null);
        let raw = resp
            .text()
            .await
            .map_err(|e| CoreError::Tool(format!("fetch_url 读取响应失败: {e}")))?;
        let bytes = raw.len();
        let truncated = bytes > FETCH_URL_BODY_CAP;
        let body = truncate_chars(&raw, FETCH_URL_BODY_CAP);
        Ok(json!({
            "ok": true, "tool": "fetch_url", "url": url,
            "final_url": final_url, "status": status,
            "content_type": content_type, "headers": headers,
            "body": body, "bytes": bytes, "truncated": truncated,
        }))
    })
}

/// `download_file(url, output_path, timeout?)`：下载到沙箱内文件。
pub fn download_file_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .ok_or_else(|| CoreError::Tool("download_file: url 必须以 http:// 或 https:// 开头".into()))?;
    let output_raw = args
        .get("output_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Tool("download_file 缺 output_path".into()))?;
    if output_raw.contains("://") {
        return Err(CoreError::Tool(
            "output_path 必须是本地文件路径，不能是 URL".into(),
        ));
    }
    let output_path = resolve_under_root(&ex.root, Path::new(output_raw))?;
    let timeout_sec = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_DL_TIMEOUT_SEC)
        .clamp(1, MAX_DL_TIMEOUT_SEC);

    let temp_path = output_path.with_extension(format!(
        "download-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let temp_for_io = temp_path.clone();
    let started = Instant::now();
    let allow_lan = ex.allow_lan_access;

    let result = block_on(async move {
        // SSRF 防护：请求前校验 + 每跳重定向校验
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| format!("URL 无效: {e}"))?;
        check_url_ssrf(&parsed, allow_lan)?;
        let resp = http_client(allow_lan)
            .get(url)
            .timeout(Duration::from_secs(timeout_sec))
            .send()
            .await
            .map_err(|e| format!("网络错误: {e}"))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(format!("download failed with HTTP {status}"));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if let Some(parent) = temp_for_io.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::File::create(&temp_for_io).map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream();
        let mut bytes = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            bytes += chunk.len() as u64;
        }
        file.sync_all().map_err(|e| e.to_string())?;
        Ok((status, content_type, bytes))
    });

    match result {
        Ok((status, content_type, bytes)) => {
            if output_path.exists() {
                std::fs::remove_file(&output_path)
                    .map_err(|e| CoreError::Tool(format!("移除旧文件失败: {e}")))?;
            }
            std::fs::rename(&temp_path, &output_path)
                .map_err(|e| CoreError::Tool(format!("写入失败: {e}")))?;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(json!({
                "ok": true, "tool": "download_file",
                "command_profile": "download",
                "url": url,
                "output_path": output_path.to_string_lossy().to_string(),
                "status": status,
                "bytes": bytes,
                "bytes_human": format_bytes(bytes),
                "content_type": content_type,
                "elapsed_ms": elapsed_ms,
                "hint": "下载完成，输出文件已存在。",
            }))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(CoreError::Tool(format!("download_file 失败: {e}")))
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 搜索引擎（第一梯队：带 key JSON API）
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SearchResultItem {
    title: String,
    url: String,
    snippet: String,
}

/// 第二梯队并行搜索的未来类型（type_complexity 提示后显式命名）。
type SearchFut<'a> = Pin<Box<dyn Future<Output = std::result::Result<Vec<SearchResultItem>, String>> + Send + 'a>>;

async fn search_serper(key: &str, query: &str, limit: usize) -> std::result::Result<Vec<SearchResultItem>, String> {
    let (hl, gl) = if has_cjk(query) { ("zh-cn", "cn") } else { ("en", "us") };
    let resp = HTTP
        .post("https://google.serper.dev/search")
        .header("X-API-KEY", key)
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(12))
        .json(&json!({ "q": query, "num": limit, "hl": hl, "gl": gl }))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let hint = if status.as_u16() == 401 || status.as_u16() == 403 { " (check SERPER_API_KEY)" } else { "" };
        return Err(format!("http {status}{hint}"));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let mut out = Vec::new();
    if let Some(organic) = body.get("organic").and_then(Value::as_array) {
        for item in organic {
            let title = item.get("title").and_then(Value::as_str).unwrap_or("").to_string();
            let url = item.get("link").and_then(Value::as_str).unwrap_or("").to_string();
            let snippet = item.get("snippet").and_then(Value::as_str).unwrap_or("").to_string();
            if url.is_empty() { continue; }
            out.push(SearchResultItem { title, url, snippet });
            if out.len() >= limit { break; }
        }
    }
    Ok(out)
}

async fn search_brave(key: &str, query: &str, limit: usize) -> std::result::Result<Vec<SearchResultItem>, String> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencode(query),
        limit
    );
    let resp = HTTP
        .get(&url)
        .header("Accept", "application/json")
        .header("X-Subscription-Token", key)
        .timeout(Duration::from_secs(12))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let hint = if status.as_u16() == 401 || status.as_u16() == 403 { " (check brave_api_key)" } else { "" };
        return Err(format!("http {status}{hint}"));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let mut out = Vec::new();
    if let Some(results) = body.get("web").and_then(|w| w.get("results")).and_then(Value::as_array) {
        for item in results {
            let title = item.get("title").and_then(Value::as_str).unwrap_or("").to_string();
            let url = item.get("url").and_then(Value::as_str).unwrap_or("").to_string();
            let snippet = item.get("description").and_then(Value::as_str).unwrap_or("").to_string();
            if url.is_empty() { continue; }
            out.push(SearchResultItem { title, url, snippet });
            if out.len() >= limit { break; }
        }
    }
    Ok(out)
}

async fn search_tavily(key: &str, query: &str, limit: usize) -> std::result::Result<Vec<SearchResultItem>, String> {
    let resp = HTTP
        .post("https://api.tavily.com/search")
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(12))
        .json(&json!({
            "api_key": key,
            "query": query,
            "max_results": limit,
            "search_depth": "basic",
        }))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let hint = if status.as_u16() == 401 || status.as_u16() == 403 { " (check tavily_api_key)" } else { "" };
        return Err(format!("http {status}{hint}"));
    }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let mut out = Vec::new();
    if let Some(results) = body.get("results").and_then(Value::as_array) {
        for item in results {
            let title = item.get("title").and_then(Value::as_str).unwrap_or("").to_string();
            let url = item.get("url").and_then(Value::as_str).unwrap_or("").to_string();
            let snippet = item.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            if url.is_empty() { continue; }
            out.push(SearchResultItem { title, url, snippet });
            if out.len() >= limit { break; }
        }
    }
    Ok(out)
}

async fn search_searxng(base: &str, query: &str, limit: usize) -> std::result::Result<Vec<SearchResultItem>, String> {
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err("SEARXNG_URL must start with http:// or https://".into());
    }
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/search").unwrap_or(base).trim_end_matches('/');
    let url = format!("{base}/search?q={}&format=json&pageno=1", urlencode(query));
    let resp = HTTP
        .get(&url)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(12))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    if !status.is_success() { return Err(format!("http {status}")); }
    let body: Value = resp.json().await.map_err(|e| format!("json: {e}"))?;
    let mut out = Vec::new();
    if let Some(results) = body.get("results").and_then(Value::as_array) {
        for item in results {
            let title = item.get("title").and_then(Value::as_str).unwrap_or("").to_string();
            let url = item.get("url").and_then(Value::as_str).unwrap_or("").to_string();
            let snippet = item.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            if url.is_empty() { continue; }
            out.push(SearchResultItem { title, url, snippet });
            if out.len() >= limit { break; }
        }
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────
// 搜索引擎（第二梯队：无 key 爬虫兜底，并行抢答）
// ─────────────────────────────────────────────────────────────

async fn search_bing(query: &str, limit: usize) -> std::result::Result<Vec<SearchResultItem>, String> {
    let url = format!("https://cn.bing.com/search?q={}&setlang=zh-CN", urlencode(query));
    let resp = HTTP
        .get(&url)
        .header("Accept-Language", "zh-CN,zh;q=0.9")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
    }
    let html = resp.text().await.map_err(|e| format!("read: {e}"))?;
    let mut out = Vec::new();
    for caps in BING_ITEM_RE.captures_iter(&html) {
        let url = unwrap_redirect_url(&caps[1]);
        let title = html_to_text(&caps[2]);
        let snippet = caps
            .get(3)
            .or_else(|| caps.get(4))
            .map(|m| html_to_text(m.as_str()))
            .unwrap_or_default();
        if title.is_empty() || url.is_empty() { continue; }
        out.push(SearchResultItem { title, url, snippet });
        if out.len() >= limit { break; }
    }
    if out.is_empty() {
        let head = &html[..html.len().min(4000)];
        if BING_BLOCKED_RE.is_match(head) {
            return Err("blocked or captcha".into());
        }
        return Err("no b_algo found (layout may have changed)".into());
    }
    Ok(out)
}

async fn search_jina(query: &str, _limit: usize) -> std::result::Result<Vec<SearchResultItem>, String> {
    let url = format!("https://s.jina.ai/{}", urlencode(query));
    let resp = HTTP
        .get(&url)
        .header("Accept", "text/plain")
        .header("X-Respond-With", "no-references")
        .timeout(Duration::from_secs(18))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => " (jina 现在需要 API key，请在设置 → 上网中配置 jina_api_key)".to_string(),
            429 => " (rate-limited)".to_string(),
            _ => String::new(),
        };
        return Err(format!("http {status}{hint}"));
    }
    let text = resp.text().await.map_err(|e| format!("read: {e}"))?.trim().to_string();
    if text.len() < 50 { return Err(format!("short body ({} chars, likely rate-limited)", text.len())); }
    let mut out = Vec::new();
    for block in text.split("\n[") {
        // split 后除第一个元素外丢失了 "["
        let block = if block.starts_with('[') {
            block.to_string()
        } else {
            format!("[{block}")
        };
        let title = JINA_TITLE_RE.captures(&block).and_then(|c| c.get(1));
        let url = JINA_URL_RE.captures(&block).and_then(|c| c.get(1));
        let (Some(title), Some(url)) = (title, url) else { continue };
        let snippet = JINA_DESC_RE
            .captures(&block)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        out.push(SearchResultItem {
            title: html_to_text(title.as_str()),
            url: url.as_str().to_string(),
            snippet: html_to_text(&snippet),
        });
    }
    if out.is_empty() { return Err("parsed 0 results (format may have changed)".into()); }
    Ok(out)
}

async fn search_ddg(query: &str, limit: usize) -> std::result::Result<Vec<SearchResultItem>, String> {
    let url = format!("https://duckduckgo.com/html/?q={}", urlencode(query));
    let resp = HTTP
        .get(&url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() { return Err(format!("http {}", resp.status())); }
    let html = resp.text().await.map_err(|e| format!("read: {e}"))?;
    if !html.contains("result__a") { return Err("blocked or captcha (no result__a)".into()); }
    let mut out = Vec::new();
    for caps in DDG_ITEM_RE.captures_iter(&html) {
        let m = caps.get(0).unwrap();
        let url = unwrap_redirect_url(&caps[1]);
        let title = html_to_text(&caps[2]);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        // 取该结果块到下一个 result__a 之间的 snippet
        let slice = &html[m.end()..];
        let snippet = NEXT_DDG_RE
            .find(slice)
            .and_then(|n| DDG_SNIPPET_RE.captures(&slice[..n.start()]))
            .or_else(|| DDG_SNIPPET_RE.captures(&slice[..slice.len().min(2000)]))
            .and_then(|c| c.get(1).or_else(|| c.get(2)))
            .map(|s| html_to_text(s.as_str()))
            .unwrap_or_default();
        out.push(SearchResultItem { title, url, snippet });
        if out.len() >= limit { break; }
    }
    if out.is_empty() { return Err("parsed 0 results".into()); }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────
// web_read / fetch_url 底层
// ─────────────────────────────────────────────────────────────

struct DirectResult {
    ok: bool,
    status: Option<u16>,
    content_type: Option<String>,
    title: String,
    body: String,
    is_json: bool,
    final_url: String,
    code: Option<String>,
    error: Option<String>,
    low_value: bool,
}

async fn fetch_via_direct(
    url: &str,
    timeout_ms: u64,
    expect_json: bool,
    allow_lan: bool,
) -> DirectResult {
    let parsed = match reqwest::Url::parse(url) {
        Ok(p) => p,
        Err(e) => {
            return DirectResult {
                ok: false, status: None, content_type: None,
                title: String::new(), body: String::new(), is_json: false,
                final_url: url.to_string(), code: Some("SSRF".into()),
                error: Some(format!("URL 无效: {e}")), low_value: false,
            };
        }
    };
    // SSRF 防护：请求前校验目标 URL（协议/凭据/本机/私网/云元数据，防 DNS rebinding）
    if let Err(e) = check_url_ssrf(&parsed, allow_lan) {
        return DirectResult {
            ok: false, status: None, content_type: None,
            title: String::new(), body: String::new(), is_json: false,
            final_url: url.to_string(), code: Some("SSRF".into()),
            error: Some(e), low_value: false,
        };
    }
    let resp = match http_client(allow_lan)
        .get(url)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.7",
        )
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .timeout(Duration::from_millis(timeout_ms))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return DirectResult {
                ok: false, status: None, content_type: None,
                title: String::new(), body: String::new(), is_json: false,
                final_url: url.to_string(), code: Some("NETWORK".into()),
                error: Some(e.to_string()), low_value: false,
            };
        }
    };
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if !resp.status().is_success() {
        return DirectResult {
            ok: false, status: Some(status), content_type: Some(content_type.clone()),
            title: String::new(), body: String::new(), is_json: false,
            final_url, code: Some("HTTP".into()),
            error: Some(format!("HTTP {status}")), low_value: false,
        };
    }
    if !content_type.is_empty() && !CT_RE.is_match(&content_type) {
        return DirectResult {
            ok: false, status: Some(status), content_type: Some(content_type.clone()),
            title: String::new(), body: String::new(), is_json: false,
            final_url, code: Some("CONTENT_TYPE".into()),
            error: Some(format!("不支持的 content-type: {content_type}")), low_value: false,
        };
    }
    let raw = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            return DirectResult {
                ok: false, status: Some(status), content_type: Some(content_type.clone()),
                title: String::new(), body: String::new(), is_json: false,
                final_url, code: Some("DECODE".into()), error: Some(e.to_string()),
                low_value: false,
            };
        }
    };

    let looks_json = (expect_json || content_type.to_lowercase().contains("json"))
        && (raw.trim_start().starts_with('{') || raw.trim_start().starts_with('['));
    if looks_json {
        let body = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| raw.clone());
        return DirectResult {
            ok: true, status: Some(status), content_type: Some(content_type.clone()),
            title: String::new(), body, is_json: true,
            final_url, code: None, error: None, low_value: false,
        };
    }

    let text = html_to_text(&raw);
    let title = extract_title(&raw);
    if is_low_value_page_text(&text) {
        return DirectResult {
            ok: false, status: Some(status), content_type: Some(content_type.clone()),
            title, body: String::new(), is_json: false,
            final_url, code: Some("LOW_VALUE".into()),
            error: Some("页面无可读内容".into()), low_value: true,
        };
    }
    DirectResult {
        ok: true, status: Some(status), content_type: Some(content_type),
        title, body: text, is_json: false,
        final_url, code: None, error: None, low_value: false,
    }
}

struct JinaResult {
    title: String,
    body: String,
    final_url: String,
}

async fn fetch_via_jina(
    url: &str,
    timeout_ms: u64,
    jina_key: Option<&str>,
) -> std::result::Result<JinaResult, String> {
    let mut req = HTTP
        .get(format!("https://r.jina.ai/{url}"))
        .header("Accept", "text/plain")
        .header("X-Return-Format", "markdown")
        .header("X-Timeout", format!("{}", (timeout_ms / 1000).max(5)))
        .timeout(Duration::from_millis(timeout_ms.min(20_000)));
    if let Some(k) = jina_key {
        req = req.header("Authorization", format!("Bearer {k}"));
    }
    let resp = req.send().await.map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Jina HTTP {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| format!("read: {e}"))?.trim().to_string();
    if text.is_empty() || is_low_value_page_text(&text) {
        return Err("Jina returned no readable content".into());
    }
    let title = text
        .lines()
        .find_map(|l| l.strip_prefix("Title:"))
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let body = text
        .replacen("URL Source:", "", 1)
        .replacen("Markdown Content:", "", 1)
        .trim()
        .to_string();
    Ok(JinaResult { title, body, final_url: url.to_string() })
}

/// web_read 一次读取的产物（聚合 `build_read_payload` 入参，避免过多参数）。
struct ReadOutcome<'a> {
    url: &'a str,
    final_url: &'a str,
    status: Option<u16>,
    source: &'a str,
    title: &'a str,
    text: &'a str,
    is_json: bool,
}

/// 组装 web_read 返回（对齐 Node buildPayload）：长文落盘 + 摘要。
fn build_read_payload(ex: &NativeToolExecutor, outcome: &ReadOutcome<'_>, max_chars: usize) -> Value {
    let ReadOutcome {
        url,
        final_url,
        status,
        source,
        title,
        text,
        is_json,
    } = outcome;
    let is_long = !*is_json && text.chars().count() >= ARTICLE_LENGTH_THRESHOLD;
    let mut body_path = None;
    let mut body_bytes = None;
    if is_long {
        match save_long_article(&ex.root, url, final_url, title, text, source) {
            Ok(saved) => {
                body_path = Some(saved.path);
                body_bytes = Some(saved.bytes);
            }
            Err(e) => tracing::warn!("[web_read] 长文落盘失败: {e}"),
        }
    }
    let inline_limit = max_chars.clamp(1_000, MAX_CHARS_LIMIT as usize);
    let text_len = text.chars().count();
    let content = if is_long {
        format!("{}\n\n...", truncate_chars(text, ARTICLE_SUMMARY_EXCERPT))
    } else if text_len > inline_limit {
        format!("{}\n\n...", truncate_chars(text, inline_limit))
    } else {
        text.to_string()
    };

    let mut payload = json!({
        "ok": true, "tool": "web_read", "url": url,
        "final_url": final_url,
        "status": status,
        "read_source": source,
        "title": title,
        "content": content,
        "truncated": is_long || text_len > inline_limit,
        "content_length": text_len,
        "hint": "可结合其他来源使用本页内容，然后回答用户。",
    });
    if *is_json {
        payload["is_json"] = json!(true);
    }
    if let Some(path) = body_path {
        payload["body_path"] = json!(path);
        payload["hint"] = json!(format!(
            "长文已落盘，完整内容在沙箱路径: {path}，可用 read_file 打开。"
        ));
    }
    if let Some(bytes) = body_bytes {
        payload["body_bytes"] = json!(bytes);
    }
    payload
}

struct SavedArticle {
    path: String,
    bytes: usize,
}

/// 把长文写入 `sandbox/articles/{YYYY-MM}/{date}_{titleSlug}_{hash8}.md`
/// （对齐 Node util.js saveLongArticle；同 URL 当天再次抓取直接复用已有文件）。
fn save_long_article(
    root: &Path,
    url: &str,
    final_url: &str,
    title: &str,
    body: &str,
    source: &str,
) -> std::io::Result<SavedArticle> {
    let now = chrono::Local::now();
    let yyyy_mm = now.format("%Y-%m").to_string();
    let date = now.format("%Y-%m-%d").to_string();
    let hash = url_hash8(if final_url.is_empty() { url } else { final_url });
    let title_slug = sanitize_slug(title);
    let base_name = if title_slug.is_empty() {
        format!("{date}_{hash}.md")
    } else {
        format!("{date}_{title_slug}_{hash}.md")
    };
    let abs_path = root.join(ARTICLES_DIR).join(&yyyy_mm).join(&base_name);
    let rel_path = format!("{ARTICLES_DIR}/{yyyy_mm}/{base_name}");
    if abs_path.exists() {
        return Ok(SavedArticle {
            path: rel_path,
            bytes: std::fs::metadata(&abs_path)?.len() as usize,
        });
    }
    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let frontmatter = format!(
        "---\ntitle: {}\nsource_url: {}\nfinal_url: {}\nsource_tool: {}\nfetched_at: {}\n---\n\n",
        json_quote(title),
        url,
        if final_url.is_empty() { url } else { final_url },
        source,
        now.to_rfc3339()
    );
    let heading = if title.is_empty() {
        String::new()
    } else {
        format!("# {title}\n\n")
    };
    let content = format!("{frontmatter}{heading}{body}");
    std::fs::write(&abs_path, &content)?;
    Ok(SavedArticle {
        path: rel_path,
        bytes: content.len(),
    })
}

// ─────────────────────────────────────────────────────────────
// 共享工具函数
// ─────────────────────────────────────────────────────────────

fn json_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn normalize_url(raw: &str) -> String {
    let v = raw.trim();
    if v.starts_with("http://") || v.starts_with("https://") {
        v.to_string()
    } else {
        format!("https://{v}")
    }
}

/// SSRF 防护：IPv4/IPv6 私网判定（对齐 Node `isPrivateAddress`）。
fn is_private_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 10
                || o[0] == 127
                || o[0] == 0
                || (o[0] == 169 && o[1] == 254)
                || (o[0] == 172 && o[1] >= 16 && o[1] <= 31)
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 100 && o[1] >= 64 && o[1] <= 127)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || o[0] >= 224
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            // v4-mapped 地址（::ffff:a.b.c.d）递归判定
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_private_ip(std::net::IpAddr::V4(mapped));
            }
            let segs = v6.segments();
            // fc00::/7 unique-local、fe80::/10 link-local、ff00::/8 multicast
            (segs[0] & 0xfe00) == 0xfc00
                || (segs[0] & 0xffc0) == 0xfe80
                || (segs[0] & 0xff00) == 0xff00
        }
    }
}

/// SSRF 防护：URL 校验（协议/凭据/主机名/IP 字面量/域名解析），对齐 Node
/// `assertBrowserUrlAllowed`。`allow_lan=true` 时放行本机/私网地址。
/// 注意：域名解析用同步 std `ToSocketAddrs`（重定向策略回调与请求前均同步调用，
/// 仅在直连工具里使用，频率低可接受阻塞）。
/// 返回 `std::result::Result` 以避免与 crate 的 `Result<T, CoreError>` 别名冲突。
fn check_url_ssrf(url: &reqwest::Url, allow_lan: bool) -> std::result::Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("仅支持 http/https 协议: {url}"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL 含凭据（username/password），已拒绝".into());
    }
    if allow_lan {
        return Ok(());
    }
    let host = url
        .host_str()
        .unwrap_or("")
        .trim_matches(|c| c == '[' || c == ']')
        .to_lowercase();
    if host.is_empty() {
        return Err("URL 无有效主机名".into());
    }
    if host == "localhost" || host.ends_with(".localhost") {
        return Err(format!("禁止访问本机/私网地址: {host}"));
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_private_ip(ip) {
            return Err(format!("禁止访问本机/私网地址: {host}"));
        }
        return Ok(());
    }
    // 域名：解析后任一地址为私网 → 拒绝（防 DNS rebinding）
    let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), 80))
        .map_err(|e| format!("域名解析失败: {host} ({e})"))?;
    let mut any = false;
    for sa in addrs {
        any = true;
        if is_private_ip(sa.ip()) {
            return Err(format!("域名 {host} 解析到本机/私网地址: {}", sa.ip()));
        }
    }
    if !any {
        return Err(format!("域名 {host} 无解析结果"));
    }
    Ok(())
}

/// SSRF 防护：reqwest 重定向策略——每跳重定向都过 [`check_url_ssrf`]；
/// 被拒目标通过 `Attempt::error` 使整个请求失败（对齐 Node 的 redirect 检查中止）。
fn redirect_policy(allow_lan: bool) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| match check_url_ssrf(attempt.url(), allow_lan) {
        Ok(()) => attempt.follow(),
        Err(e) => attempt.error(format!("SSRF 拒绝重定向目标: {e}")),
    })
}

// ─────────────────────────────────────────────────────────────
// 本地浏览器渲染（对齐 Node browser_read 的 Playwright 路径）
// ─────────────────────────────────────────────────────────────

/// 可读文本候选选择器（对齐 Node snapshot.js `extractReadablePage`）。
const READABLE_SELECTOR: &str = "article, main, [role=\"main\"], .article, .post, .content, .entry-content, #content, #main";
/// 提取文本上限（对齐 Node readOnce `extract_max_chars=500_000`）。
const BROWSER_EXTRACT_MAX: usize = 500_000;
/// dump-dom 输出读取上限（防止巨页撑爆内存）。
const BROWSER_DOM_MAX: usize = 3_000_000;

/// 浏览器渲染结果（对齐 Node readOnce 返回的字段）。
struct BrowserRender {
    ok: bool,
    title: String,
    text: String,
    final_url: String,
    error: Option<String>,
}

/// 浏览器可执行文件候选（env 优先，随后常见安装路径）。
fn browser_candidates() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    for key in ["CHROME_PATH", "EDGE_PATH", "BROWSER_PATH"] {
        if let Some(p) = std::env::var_os(key) {
            v.push(PathBuf::from(p));
        }
    }
    for p in [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Users\ADMIN\AppData\Local\Google\Chrome\Application\chrome.exe",
        r"C:\Users\ADMIN\AppData\Local\Microsoft\Edge\Application\msedge.exe",
    ] {
        v.push(PathBuf::from(p));
    }
    v.into_iter().filter(|p| p.exists()).collect()
}

fn find_browser_exe() -> Option<PathBuf> {
    browser_candidates().into_iter().next()
}

/// 文本清洗（对齐 Node `clean`：压缩水平空白、合并多空行）。
fn clean_text(s: &str) -> String {
    let mid = Regex::new(r"[ \t]+").unwrap().replace_all(s, " ");
    Regex::new(r"\n{3,}").unwrap().replace_all(&mid, "\n\n").trim().to_string()
}

/// 近似 innerText：收集元素后代文本，跳过 script/style/noscript/template 等嵌入文本。
fn element_text(el: &scraper::ElementRef) -> String {
    el.descendants()
        .filter_map(|node| match node.value() {
            Node::Text(t) => {
                let parent_tag = node
                    .parent()
                    .and_then(|p| p.value().as_element())
                    .map(|e| e.name().to_string());
                if matches!(parent_tag.as_deref(), Some("script" | "style" | "noscript" | "template")) {
                    None
                } else {
                    Some(t.text.to_string())
                }
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 从渲染 DOM 提取可读文本（对齐 Node snapshot.js `extractReadablePage`：
/// 候选容器中 innerText 最长者，>300 字符取用，否则回落 body 文本）。
fn extract_readable_text(dom: &str) -> (String, String) {
    let doc = Html::parse_document(dom);
    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| doc.select(&sel).next())
        .map(|e| element_text(&e).trim().to_string())
        .unwrap_or_default();
    let sel = Selector::parse(READABLE_SELECTOR).unwrap();
    let mut best: Option<String> = None;
    for el in doc.select(&sel) {
        let text = clean_text(&element_text(&el));
        if best.as_ref().is_none_or(|b| text.len() > b.len()) {
            best = Some(text);
        }
    }
    let body = doc
        .select(&Selector::parse("body").unwrap())
        .next()
        .map(|e| clean_text(&element_text(&e)))
        .unwrap_or_default();
    let text = match best {
        Some(b) if b.len() > 300 => b,
        _ => body,
    };
    (title, text.chars().take(BROWSER_EXTRACT_MAX).collect())
}

/// 用本机 Chrome/Edge headless `--dump-dom` 渲染页面（对齐 Node Playwright
/// `readOnce` 的文本提取语义；不做滚动懒加载，final_url 回落请求 URL）。
fn render_with_browser(url: &str, timeout_ms: u64, _extract_max: usize) -> BrowserRender {
    let Some(exe) = find_browser_exe() else {
        return BrowserRender {
            ok: false, title: String::new(), text: String::new(),
            final_url: url.to_string(),
            error: Some(
                "未找到本机 Chrome/Edge，浏览器渲染不可用（可用 CHROME_PATH/EDGE_PATH 环境变量指定）".into(),
            ),
        };
    };
    // 独立 user-data-dir，避免与用户浏览器实例冲突
    let profile = std::env::temp_dir().join(format!(
        "bailongma-headless-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let profile_str = profile.to_string_lossy().to_string();
    let mut child = match Command::new(&exe)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-features=TranslateUI")
        .arg(format!("--user-data-dir={profile_str}"))
        .arg(format!("--virtual-time-budget={timeout_ms}"))
        .arg("--dump-dom")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&profile);
            return BrowserRender {
                ok: false, title: String::new(), text: String::new(),
                final_url: url.to_string(),
                error: Some(format!("启动浏览器失败: {e}")),
            };
        }
    };

    // 收集 stdout（上限 BROWSER_DOM_MAX），同时轮询子进程退出，超时则 kill
    let mut buf: Vec<u8> = Vec::new();
    let mut exited = false;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms + 8_000);
    let mut stdout = child.stdout.take();
    while Instant::now() < deadline && !exited {
        match child.try_wait() {
            Ok(Some(_)) => exited = true,
            Ok(None) => {}
            Err(_) => {
                exited = true;
                break;
            }
        }
        if let Some(s) = stdout.as_mut() {
            let mut chunk = [0u8; 65536];
            match s.read(&mut chunk) {
                Ok(0) => {}
                Ok(n) => {
                    if buf.len() < BROWSER_DOM_MAX {
                        let take = n.min(BROWSER_DOM_MAX - buf.len());
                        buf.extend_from_slice(&chunk[..take]);
                    }
                }
                Err(_) => break,
            }
        }
        if !exited {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&profile);

    if buf.is_empty() {
        return BrowserRender {
            ok: false, title: String::new(), text: String::new(),
            final_url: url.to_string(),
            error: Some("浏览器未返回渲染 DOM（可能超时或页面无输出）".into()),
        };
    }
    let dom = String::from_utf8_lossy(&buf);
    let (title, text) = extract_readable_text(&dom);
    BrowserRender {
        ok: true, title, text,
        final_url: url.to_string(),
        error: None,
    }
}

fn has_cjk(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'))
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 简单 percent-decode（Bing/DDG 中转链接参数用）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        if b[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 解包搜索中转链接（Bing ck/a base64、DDG uddg 参数）。
fn unwrap_redirect_url(raw: &str) -> String {
    let decoded = decode_html_entities(raw);
    // DDG: //duckduckgo.com/l/?uddg={urlencoded}&rut=...
    if let Some(pos) = decoded.find("uddg=") {
        let rest = &decoded[pos + 5..];
        let end = rest.find(['&', '#']).unwrap_or(rest.len());
        let dec = percent_decode(&rest[..end]);
        if dec.starts_with("http://") || dec.starts_with("https://") {
            return dec;
        }
    }
    // Bing: bing.com/ck/a?...&u=a1<base64url>
    if BING_CK_RE.is_match(&decoded) {
        if let Some(pos) = decoded.find("u=") {
            let rest = &decoded[pos + 2..];
            let end = rest.find(['&', '#']).unwrap_or(rest.len());
            let mut encoded = &rest[..end];
            if let Some(stripped) = encoded.strip_prefix("a1") {
                encoded = stripped;
            }
            let mut b64 = encoded.replace('-', "+").replace('_', "/");
            while !b64.len().is_multiple_of(4) {
                b64.push('=');
            }
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                if let Ok(dec) = String::from_utf8(bytes) {
                    if dec.starts_with("http://") || dec.starts_with("https://") {
                        return dec;
                    }
                }
            }
        }
    }
    if let Some(stripped) = decoded.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    decoded
}

/// 结果归一化：截断字段、丢弃空 url/title、按 host+path（忽略 query/fragment）去重。
fn normalize_results(raw: Vec<SearchResultItem>, limit: usize) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for item in raw {
        let url = item.url.trim().to_string();
        let title = item.title.trim().to_string();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let dedup_key = host_plus_path(&url);
        if !seen.insert(dedup_key) {
            continue;
        }
        out.push(json!({
            "title": truncate_chars(&title, SEARCH_TITLE_MAX),
            "url": url,
            "snippet": truncate_chars(item.snippet.trim(), SEARCH_SNIPPET_MAX),
        }));
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// 提取 host + path（小写，去尾斜杠，忽略 query/fragment），用于 URL 去重。
fn host_plus_path(url: &str) -> String {
    let after = url.split("://").nth(1).unwrap_or(url);
    let end = after.find(['?', '#']).unwrap_or(after.len());
    after[..end].trim_end_matches('/').to_lowercase()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

fn url_hash8(url: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    format!("{:08x}", h.finish())
}

fn sanitize_slug(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        if c.is_ascii_alphanumeric() || ('\u{4e00}'..='\u{9fa5}').contains(&c) {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    while out.starts_with('_') {
        out.remove(0);
    }
    while out.ends_with('_') {
        out.pop();
    }
    out.chars().take(40).collect()
}

fn decode_html_entities(value: &str) -> String {
    let mut s = NUM_DEC_RE
        .replace_all(value, |caps: &regex::Captures| {
            caps[1]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_default()
        })
        .into_owned();
    s = NUM_HEX_RE
        .replace_all(&s, |caps: &regex::Captures| {
            u32::from_str_radix(&caps[1], 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_default()
        })
        .into_owned();
    s = s
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    s
}

/// HTML → 可读文本（对齐 Node util.js htmlToText）。
fn html_to_text(html: &str) -> String {
    let s = decode_html_entities(html);
    let s = SCRIPT_RE.replace_all(&s, " ");
    let s = STYLE_RE.replace_all(&s, " ");
    let s = NOSCRIPT_RE.replace_all(&s, " ");
    let s = BR_RE.replace_all(&s, "\n");
    let s = BLOCK_END_RE.replace_all(&s, "\n");
    let s = TAG_RE.replace_all(&s, " ");
    let s = SPACE_RE.replace_all(&s, " ");
    let s = NEWLINE_RE.replace_all(&s, "\n\n");
    s.trim().to_string()
}

fn extract_title(html: &str) -> String {
    TITLE_TAG_RE
        .captures(html)
        .map(|caps| truncate_chars(&html_to_text(&caps[1]), 200))
        .unwrap_or_default()
}

/// 低价值页面检测（对齐 Node isLowValuePageText）。
fn is_low_value_page_text(text: &str) -> bool {
    let compact: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let compact = compact.trim();
    if compact.chars().count() < 80 {
        return true;
    }
    LOW_VALUE_RE.is_match(compact)
}

fn is_likely_api_url(url: &str) -> bool {
    API_URL_RE.is_match(&url.to_lowercase())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ─────────────────────────────────────────────────────────────
// schema 注册
// ─────────────────────────────────────────────────────────────

/// 本批工具的 OpenAI schema（由 [`super::all_tool_schemas`] 追加）。
pub fn web_tool_schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchema::new(
            "web_search",
            "搜索网页（多引擎：serper/brave/tavily/searxng 带 key JSON API，未配置时自动回退 bing/jina/ddg 无 key 兜底；结果 10 分钟缓存）。返回结构化结果列表，配合 web_read 读取页面内容。",
        )
        .required("query", string_param("搜索关键词"))
        .param("limit", integer_param("返回条数，默认 5，最大 8")),
        ToolSchema::new(
            "web_read",
            "读取网页为可读文本：直连 HTTP 抓取（自动处理 HTML/JSON，长文落盘到 sandbox/articles/ 并返回摘要 + body_path），失败时可回退 Jina Reader。返回 ok/title/content/content_length/body_path 等。",
        )
        .required("url", string_param("要读取的网页 URL"))
        .param("render", enum_param("渲染方式：auto（直连优先，失败回退 Jina）、http（仅直连）、browser（Rust 版未接线）", &["auto", "http", "browser"]))
        .param("fresh", boolean_param("是否跳过缓存强制重新抓取"))
        .param("remote_fallback", boolean_param("直连失败时是否回退 Jina Reader，默认 true"))
        .param("timeout_ms", integer_param("直连超时毫秒，默认 20000，范围 1000-45000"))
        .param("max_chars", integer_param("内联返回最大字符数，默认 5000，范围 1000-20000；超过部分截断，长文自动落盘")),
        ToolSchema::new(
            "fetch_url",
            "抓取 URL 的原始 HTTP 响应（status/headers/body 原文，body 截断 64KB）。适合取 JSON API 或需要保留原始格式的场景；读网页正文请用 web_read。",
        )
        .required("url", string_param("要抓取的 HTTP/HTTPS URL"))
        .param("timeout_ms", integer_param("超时毫秒，默认 12000，范围 1000-45000")),
        ToolSchema::new(
            "download_file",
            "下载 URL 到本地文件（沙箱内路径约束；超时/重定向/父目录创建由运行时处理，返回字节数与耗时）。",
        )
        .required("url", string_param("要下载的 HTTP/HTTPS URL"))
        .required("output_path", string_param("目标文件路径（相对沙箱根或绝对路径）"))
        .param("timeout", integer_param("超时秒数，默认 120，最大 120")),
    ]
}

// ─────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::tool_loop::ToolExecutor;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn test_executor(root: &Path) -> NativeToolExecutor {
        // 本地 mock server 均在 127.0.0.1：放行私网以测试直连抓取路径；
        // SSRF 拦截行为由 ssrf_* 测试用默认（allow_lan=false）executor 覆盖。
        NativeToolExecutor::new(root.to_path_buf()).with_allow_lan_access(true)
    }

    /// 起一个一次性 mock HTTP server，返回基础 URL。
    fn mock_server(
        handler: impl Fn(&str) -> (u16, &'static str, Vec<u8>) + Send + Sync + 'static,
    ) -> String {
        mock_server_with_headers(move |req| {
            let (status, ctype, body) = handler(req);
            (status, ctype, body, Vec::new())
        })
    }

    /// 支持自定义响应头的 mock server（重定向测试用）。循环 accept 处理多次连接
    /// （浏览器渲染 + 直连回落可能各请求一次）。
    fn mock_server_with_headers(
        handler: impl Fn(&str) -> (u16, &'static str, Vec<u8>, Vec<(&'static str, &'static str)>) + Send + Sync + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = std::sync::Arc::new(handler);
        thread::spawn(move || {
            loop {
                let Ok((stream, _)) = listener.accept() else { break };
                let h = handler.clone();
                thread::spawn(move || {
                    let mut stream = stream;
                    let mut buf = vec![0u8; 65536];
                    let _ = stream.read(&mut buf);
                    let req = String::from_utf8_lossy(&buf).to_string();
                    let (status, ctype, body, headers) = h(&req);
                    let reason = match status {
                        200 => "OK",
                        301 => "Moved Permanently",
                        302 => "Found",
                        307 => "Temporary Redirect",
                        308 => "Permanent Redirect",
                        404 => "Not Found",
                        403 => "Forbidden",
                        503 => "Service Unavailable",
                        _ => "Custom",
                    };
                    let mut head = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n",
                        body.len()
                    );
                    for (k, v) in headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("Connection: close\r\n\r\n");
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                });
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn html_to_text_strips_markup_and_entities() {
        let html = r#"<html><head><title>测试页</title>
            <script>var x = 1;</script><style>.a{color:red}</style></head>
            <body><h1>标题</h1><p>正文 &amp; 实体</p><br><div>第二段</div></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("标题"), "text={text}");
        assert!(text.contains("正文 & 实体"), "text={text}");
        assert!(text.contains("第二段"), "text={text}");
        assert!(!text.contains("var x"), "script 应被剔除");
        assert!(!text.contains("color:red"), "style 应被剔除");
        assert_eq!(extract_title(html), "测试页");
    }

    #[test]
    fn is_low_value_detects_captcha_and_short_pages() {
        assert!(is_low_value_page_text("short"));
        assert!(is_low_value_page_text("Checking your browser before accessing..."));
        assert!(is_low_value_page_text("请稍候，正在安全验证，请勿关闭页面"));
        assert!(!is_low_value_page_text(
            "这是一篇足够长的正文内容，其长度远超八十个字符阈值。它包含多个完整的句子，用于验证低价值页面检测逻辑不会误杀正常的文章正文文本内容，确保网页抓取后正文能够得到保留。"
        ));
    }

    #[test]
    fn unwrap_redirect_urls_decodes_uddg_and_bing_base64() {
        // DDG uddg 参数
        let uddg = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpath%3Fq%3D1&rut=abc";
        assert_eq!(unwrap_redirect_url(uddg), "https://example.com/path?q=1");
        // Bing ck/a base64url（a1 前缀 + URL-safe）
        let target = "https://example.com/page?a=1&b=2";
        let encoded = base64::engine::general_purpose::URL_SAFE.encode(target);
        let bing = format!("https://www.bing.com/ck/a?u=a1{encoded}&ntb=1");
        assert_eq!(unwrap_redirect_url(&bing), target);
        // 普通链接原样返回
        assert_eq!(unwrap_redirect_url("https://plain.example/a"), "https://plain.example/a");
        // 协议相对链接补 https
        assert_eq!(unwrap_redirect_url("//cdn.example/x"), "https://cdn.example/x");
    }

    #[test]
    fn normalize_results_dedups_and_truncates() {
        let raw = vec![
            SearchResultItem {
                title: "标题".repeat(300),
                url: "https://example.com/a?utm=1".into(),
                snippet: "摘要".repeat(200),
            },
            SearchResultItem {
                title: "重复".into(),
                url: "https://example.com/a#frag".into(),
                snippet: "".into(),
            },
            SearchResultItem {
                title: "空url".into(),
                url: "".into(),
                snippet: "x".into(),
            },
            SearchResultItem {
                title: "第二个".into(),
                url: "https://other.example/b".into(),
                snippet: "s".into(),
            },
        ];
        let out = normalize_results(raw, 10);
        assert_eq!(out.len(), 2, "URL 去重 + 空 url 剔除 => {out:?}");
        assert!(out[0]["title"].as_str().unwrap().chars().count() <= SEARCH_TITLE_MAX);
        assert!(out[0]["snippet"].as_str().unwrap().chars().count() <= SEARCH_SNIPPET_MAX);
    }

    #[test]
    fn save_long_article_writes_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let saved = save_long_article(
            dir.path(),
            "https://example.com/post",
            "https://example.com/post",
            "我的文章标题",
            "这是正文内容……",
            "web_read",
        )
        .unwrap();
        assert!(saved.path.starts_with("articles/"), "path={}", saved.path);
        let abs = dir.path().join(&saved.path);
        assert!(abs.exists());
        let content = std::fs::read_to_string(&abs).unwrap();
        assert!(content.contains("title: \"我的文章标题\""));
        assert!(content.contains("source_url: https://example.com/post"));
        assert!(content.contains("这是正文内容"));
        // 同 URL 再次调用复用
        let again = save_long_article(
            dir.path(),
            "https://example.com/post",
            "https://example.com/post",
            "我的文章标题",
            "不同内容",
            "web_read",
        )
        .unwrap();
        assert_eq!(again.path, saved.path, "同 URL 应复用文件");
    }

    #[test]
    fn schemas_registered() {
        let schemas = super::super::all_tool_schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        for name in ["web_search", "web_read", "fetch_url", "download_file"] {
            assert!(names.contains(&name), "schema 缺失 {name}");
        }
    }

    #[test]
    fn web_read_local_http_server() {
        let body = r#"<html><head><title>本地测试页</title></head><body><h1>你好</h1><p>这是本地 mock 服务器的正文内容，用于验证 web_read 的直连抓取路径。这段文字故意写得很长，需要超过八十个字符的低价值页面判定阈值，这样整段正文才会被识别为正常可读内容，而不会被 is_low_value_page_text 误判并丢弃。</p></body></html>"#;
        let url = mock_server(move |_req| (200, "text/html; charset=utf-8", body.as_bytes().to_vec()));
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex
            .execute("web_read", &json!({ "url": url, "render": "http" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"].as_bool(), Some(true), "r={r}");
        assert_eq!(v["read_source"].as_str(), Some("http"));
        assert!(v["content"].as_str().unwrap().contains("本地 mock 服务器"), "r={r}");
        assert_eq!(v["title"].as_str(), Some("本地测试页"));
    }

    #[test]
    fn web_read_json_pretty() {
        let url = mock_server(|_req| {
            (
                200,
                "application/json",
                br#"{"name":"bailongma","tools":["web_search","web_read"]}"#.to_vec(),
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex
            .execute("web_read", &json!({ "url": url, "render": "http" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"].as_bool(), Some(true), "r={r}");
        assert_eq!(v["is_json"].as_bool(), Some(true));
        let content = v["content"].as_str().unwrap();
        assert!(content.contains("\"name\""), "content={content}");
    }

    #[test]
    fn web_read_long_article_lands_to_sandbox() {
        let body = format!(
            "<html><head><title>长文</title></head><body>{}</body></html>",
            "<p>这一段内容足够长，用于触发落盘阈值。</p>".repeat(120)
        );
        let url = mock_server(move |_req| (200, "text/html; charset=utf-8", body.clone().into_bytes()));
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex
            .execute("web_read", &json!({ "url": url, "render": "http" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"].as_bool(), Some(true), "r={r}");
        let body_path = v["body_path"].as_str().expect("长文应落盘");
        assert!(body_path.starts_with("articles/"), "body_path={body_path}");
        let abs = dir.path().join(body_path);
        assert!(abs.exists(), "落盘文件应存在");
        assert_eq!(v["truncated"].as_bool(), Some(true));
    }

    #[test]
    fn web_read_missing_url_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex.execute("web_read", &json!({ "render": "http" }));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("缺少必填参数"));
    }

    #[test]
    fn web_read_browser_render_falls_back_to_http() {
        // render=browser：浏览器可用则渲染提取，否则回落 http 直连；均应产出可读内容
        // 注意：页面需带 charset 声明，否则 headless Chrome 无 charset 时按系统
        // 默认编码猜测导致 dump-dom 输出乱码（真实站点普遍有 meta charset）
        let body = r#"<!doctype html><html><head><meta charset="utf-8"><title>Test</title></head><body><article><p>这是浏览器渲染的可读正文内容示例。长度足够绕过低价值判定阈值，用于验证 render=browser 策略的完整执行链路，包括页面导航、等待渲染完成、DOM 文本提取以及最终的回落行为。该段落还包含足够的字符数量，确保渲染结果不会被误判为低价值页面。</p></article></body></html>"#;
        let payload = body.as_bytes().to_vec();
        let url = mock_server(move |_req| (200, "text/html", payload.clone()));
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex
            .execute("web_read", &json!({ "url": url, "render": "browser" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"].as_bool(), Some(true), "r={r}");
        assert!(
            v["content"].as_str().unwrap().contains("可读正文"),
            "r={r}"
        );
    }

    #[test]
    fn extract_readable_text_skips_scripts_and_falls_back_to_body() {
        let dom = r#"<!doctype html><html><head><title>Sample Title</title></head><body>
            <script>var x = "noise text";</script>
            <style>.a{display:none}</style>
            <div id="nav">导航杂讯</div>
            <main><article><h1>正文标题</h1><p>正文内容填充，足够长度验证提取与回落的段落文本。</p></article></main>
            <div class="content">short</div>
        </body></html>"#;
        let (title, text) = extract_readable_text(dom);
        assert_eq!(title, "Sample Title");
        assert!(text.contains("正文标题"), "text={text}");
        assert!(!text.contains("noise"), "script 文本应跳过");
        assert!(!text.contains(".a{"), "style 文本应跳过");
        assert!(text.len() > 20);
    }

    #[test]
    fn extract_readable_text_prefers_long_article() {
        let long = "正文".repeat(400);
        let dom = format!(
            r#"<html><head><title>Long</title></head><body><div id="nav">nav</div><article>{long}</article><div>short</div></body></html>"#
        );
        let (title, text) = extract_readable_text(&dom);
        assert_eq!(title, "Long");
        assert!(text.starts_with("正文"), "应取 article 文本: len={}", text.len());
        assert!(!text.contains("nav"), "短导航不应混入");
    }

    #[test]
    fn web_read_all_strategies_failed() {
        // 直连 404 + remote_fallback=false → 失败聚合
        let url = mock_server(|_req| (404, "text/plain", b"nope".to_vec()));
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex
            .execute(
                "web_read",
                &json!({ "url": url, "render": "http", "remote_fallback": false }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"].as_bool(), Some(false), "r={r}");
        assert!(v["failures"].as_array().is_some());
    }

    #[test]
    fn fetch_url_returns_raw_response() {
        let body = "hello raw body with enough length to be meaningful for the test";
        let url = mock_server(move |_req| (200, "text/plain", body.as_bytes().to_vec()));
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex.execute("fetch_url", &json!({ "url": url })).unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"].as_bool(), Some(true), "r={r}");
        assert_eq!(v["status"].as_u64(), Some(200));
        assert_eq!(v["content_type"].as_str(), Some("text/plain"));
        assert!(v["body"].as_str().unwrap().contains("hello raw body"));
        assert_eq!(v["bytes"].as_u64(), Some(body.len() as u64));
    }

    #[test]
    fn download_file_saves_to_sandbox() {
        let payload = b"BINARY\x00\x01\x02\x03DATA".to_vec();
        let payload_len = payload.len();
        let server_payload = payload.clone();
        let url = mock_server(move |_req| (200, "application/octet-stream", server_payload.clone()));
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex
            .execute(
                "download_file",
                &json!({ "url": url, "output_path": "downloads/out.bin" }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"].as_bool(), Some(true), "r={r}");
        assert_eq!(v["bytes"].as_u64(), Some(payload_len as u64), "r={r}");
        let abs = dir.path().join("downloads/out.bin");
        assert!(abs.exists());
        let got = std::fs::read(&abs).unwrap();
        assert_eq!(got, payload);
        // 临时文件应清理
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("downloads"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件应清理: {leftovers:?}");
    }

    #[test]
    fn download_file_rejects_outside_sandbox() {
        let url = mock_server(|_req| (200, "text/plain", b"x".to_vec()));
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let outside = dir
            .path()
            .parent()
            .unwrap()
            .join("escape.txt")
            .to_string_lossy()
            .to_string();
        let r = ex.execute(
            "download_file",
            &json!({ "url": url, "output_path": outside }),
        );
        assert!(r.is_err(), "越界路径应被拒绝");
    }

    #[test]
    fn download_file_requires_http_url() {
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex.execute(
            "download_file",
            &json!({ "url": "file:///etc/passwd", "output_path": "out" }),
        );
        assert!(r.is_err());
    }

    #[test]
    fn web_search_missing_query_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ex = test_executor(dir.path());
        let r = ex.execute("web_search", &json!({ "limit": 3 }));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("缺少必填参数"));
    }

    #[test]
    fn url_ttl_varies_by_kind() {
        assert_eq!(url_ttl("https://wttr.in/beijing"), Duration::from_secs(600));
        assert_eq!(url_ttl("https://example.com/news/rss"), Duration::from_secs(300));
        assert_eq!(url_ttl("https://example.com/"), Duration::from_secs(3600));
    }

    #[test]
    fn format_bytes_human() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert!(format_bytes(2048).starts_with("2.0 KB"));
    }

    #[test]
    fn ssrf_private_ip_table_matches_node() {
        use std::net::IpAddr;
        for (ip, expect_private) in [
            ("10.0.0.5", true),
            ("127.0.0.1", true),
            ("0.0.0.0", true),
            ("169.254.169.254", true),
            ("172.16.0.1", true),
            ("172.31.255.255", true),
            ("172.32.0.1", false),
            ("192.168.1.10", true),
            ("100.64.0.1", true),
            ("100.127.255.255", true),
            ("100.128.0.1", false),
            ("198.18.0.1", true),
            ("198.19.255.255", true),
            ("223.255.255.255", false),
            ("224.0.0.1", true),
            ("8.8.8.8", false),
            ("::1", true),
            ("::", true),
            ("fc00::1", true),
            ("fd12:3456::1", true),
            ("fe80::1", true),
            ("2001:4860:4860::8888", false),
            ("::ffff:127.0.0.1", true),
            ("::ffff:169.254.169.254", true),
            ("::ffff:8.8.8.8", false),
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert_eq!(is_private_ip(ip), expect_private, "ip={ip}");
        }
    }

    #[test]
    fn ssrf_rejects_private_and_metadata_urls() {
        let dir = tempfile::tempdir().unwrap();
        // 默认 allow_lan=false：本机/私网/云元数据/带凭据 URL 一律拒绝
        let ex = NativeToolExecutor::new(dir.path().to_path_buf());
        for u in [
            "http://127.0.0.1:8080/status",
            "http://localhost:3000/",
            "http://169.254.169.254/latest/meta-data/",
            "http://192.168.1.10:8000/api",
            "http://10.0.0.5/",
            "http://user:pass@example.com/",
            "file:///etc/passwd",
        ] {
            let r = ex.execute("fetch_url", &json!({ "url": u }));
            assert!(r.is_err(), "url={u} 应被 SSRF 拒绝");
        }
        // download_file 同样拒绝私网
        let r = ex.execute(
            "download_file",
            &json!({ "url": "http://127.0.0.1:1/x", "output_path": "x" }),
        );
        assert!(r.is_err());
        // web_read 直连同样拒绝（fetch_via_direct SSRF 前置）：失败折叠进 JSON
        let r = ex
            .execute(
                "web_read",
                &json!({ "url": "http://169.254.169.254/latest/meta-data/", "render": "http" }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"].as_bool(), Some(false), "r={r}");
        let fails = v["failures"].as_array().unwrap();
        assert!(
            fails.iter().any(|f| f["code"].as_str() == Some("SSRF")),
            "r={r}"
        );
    }

    #[test]
    fn ssrf_redirect_to_private_is_rejected() {
        // redirect_policy 与首跳校验共用 check_url_ssrf：直接验证判定函数
        let evil = reqwest::Url::parse("http://169.254.169.254/latest/meta-data/").unwrap();
        assert!(check_url_ssrf(&evil, false).is_err());
        assert!(check_url_ssrf(&evil, true).is_ok(), "allow_lan=true 放行");
        let ftp = reqwest::Url::parse("ftp://example.com/x").unwrap();
        assert!(check_url_ssrf(&ftp, true).is_err(), "非 http/https 协议拒绝");
        let cred = reqwest::Url::parse("http://u:p@example.com/").unwrap();
        assert!(check_url_ssrf(&cred, true).is_err(), "凭据 URL 拒绝");

        // 端到端：mock server 首跳即私网（127.0.0.1）→ no-lan executor 请求前拒绝
        let url = mock_server_with_headers(|_req| {
            (302, "text/html", Vec::new(), vec![("Location", "http://169.254.169.254/")])
        });
        let dir = tempfile::tempdir().unwrap();
        let ex = NativeToolExecutor::new(dir.path().to_path_buf()); // allow_lan=false
        let e = ex
            .execute("fetch_url", &json!({ "url": url }))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("SSRF") || e.contains("禁止") || e.contains("拒绝"),
            "e={e}"
        );
    }
}
