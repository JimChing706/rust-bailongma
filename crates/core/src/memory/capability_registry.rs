//! 能力注册表（对齐 `src/capabilities/capability-registry.js`，v1 范围：已迁能力的
//! 工作流块 + 触发判定半）。
//!
//! 本模块是「能力机制」声明式单元的唯一真相源：每个能力把关键词 / 工具 / 工作流块 /
//! 数据预喂收敛在一处，由情境触发、打包注入。已移植子集（对齐 system_prompt.rs TODO）：
//!
//! - `weather`          天气：`### Weather Surface Rules`（仅 wttr.in 取数）
//! - `hotspot`          热点面板：`### Hotspot Panel`（hotspot_mode）
//! - `worldcup`         世界杯面板：`### World Cup Panel`（worldcup_mode）
//! - `software-install` 安装软件：`## Software Install Workflow`（install_software）
//!
//! 尚未移植（后续里程碑）：
//! - `interactive-browser` / `web` / `typhoon` 三个能力的块与意图正则；
//! - 运行时数据预喂（weather.js / hotspots.js / worldcup.js / typhoon.js 的
//!   `buildXxxRuntimeContext`）与 `api-slots.js` 的 `listApiSlotCapabilities`；
//! - find_tool 发现侧（`findCapabilitiesByQuery` / `listCapabilities`）随 tool-router 里程碑。

use std::sync::OnceLock;

use regex::Regex;

use crate::memory::software_install_intent::is_software_install_request;

// ── 已迁能力的工具名数组（本模块为唯一定义处；tool-router 里程碑从这里 import） ──

/// 无状态读网页工具（weather 能力固定用 web_read 抓 wttr.in）。
pub const WEB_READ_TOOLS: [&str; 1] = ["web_read"];
/// 热点面板工具（不自动注入，Agent 判断后经 find_tool 装载）。
pub const HOTSPOT_TOOLS: [&str; 1] = ["hotspot_mode"];
/// 世界杯面板工具：开面板即可看赛况；追问细节要联网 → 带无状态搜索。
pub const WORLDCUP_TOOLS: [&str; 2] = ["worldcup_mode", "web_search"];
/// 安装软件工具组。
pub const SOFTWARE_INSTALL_TOOLS: [&str; 2] = ["install_software", "list_processes"];

// ── 工作流块（prompt 注入用；文本逐字保留自 capability-registry.js） ─────────

/// `### Weather Surface Rules`（对齐 WEATHER_CONTEXT_BLOCK）。
pub const WEATHER_CONTEXT_BLOCK: &str = r#"### Weather Surface Rules
- The data source must be wttr.in only. Do not use search engines or other weather sites. Use this fixed call:
  web_read({ url: "https://wttr.in/{city-English-name}?format=j1&lang=zh", fresh: true, render: "http" })
- Map the following fields the weather kind actually renders. Only fill a field that is actually present in the JSON; leave a missing field empty rather than supplying a typical value or a guess:
  - city       <- nearest_area[0].areaName[0].value, any language is fine; if missing, use the city the user asked about.
  - temp       <- current_condition[0].temp_C, number
  - condition  <- current_condition[0].lang_zh[0].value or weatherDesc[0].value
  - variant    <- "compact" for a 3-day card, or "week" when the user asks for one week / seven days.
  - forecast   <- compact: three items from weather[0..2]; week: seven items if available. Each item is { day, low, high, condition }.
- Call: ui_set({ id: "weather-<city>", kind: "weather", data: { variant, city, temp, condition, forecast }, intent: "ambient" })
- If a matching weather surface is already listed in Supplemental Context, do not call ui_set again unless the user asks to refresh or the surface data is clearly missing.
- To refresh, call ui_set again with the same id."#;

/// `### Hotspot Panel`（对齐 HOTSPOT_CONTEXT_BLOCK）。
pub const HOTSPOT_CONTEXT_BLOCK: &str = r#"### Hotspot Panel
- You have a hotspot_mode tool that opens a visual hotspot / trending-topics panel. It is NOT pre-loaded each turn — if it is not in your current tool list, call find_tool("热点 面板 hotspot") first to load it, then call it.
- Open it (action="show") only when the user actually wants to browse trending topics, or a demo/scene needs it; close it (action="hide") when asked. Do not open it for ordinary Q&A.
- While the panel is open, current hotspot data is injected into your context automatically — answer from that rather than guessing."#;

/// `### World Cup Panel`（对齐 WORLDCUP_CONTEXT_BLOCK）。
pub const WORLDCUP_CONTEXT_BLOCK: &str = r#"### World Cup Panel
- You have a worldcup_mode tool that opens a panel with live scores, schedule and group standings (FIFA World Cup, Beijing time). It is NOT pre-loaded each turn — if it is not in your current tool list, call find_tool("世界杯 比分 worldcup") first to load it, then call it.
- Open it (action="show") when the user asks about World Cup matches, scores or schedule and a visual panel helps; close it (action="hide") when asked.
- While the panel is open, current match data is injected into your context automatically; for deeper details (lineups, scorers) use web tools."#;

/// `## Software Install Workflow`（对齐 SOFTWARE_INSTALL_CONTEXT_BLOCK）。
pub const SOFTWARE_INSTALL_CONTEXT_BLOCK: &str = r#"## Software Install Workflow
- First use injected installed-software context to see whether the app is already installed. If installation is still needed, call install_software first. install_software starts a background job and normally returns immediately with status="started" and job_id; this only means the job began, not that the app is installed. After a started result, tell the user briefly that installation is running in the background and stop the round. Do not call install_software again for the same app, do not poll repeatedly, and do not claim success until a later background APP_SIGNAL/list_processes result says succeeded/already installed/current. Do not run raw winget commands with exec_command, do not browse vendor pages, and do not enumerate download URLs before install_software has returned a terminal structured failure. On Windows this tool owns the winget path, including candidate selection and stale-manifest fallback such as Tencent.QQ.NT before Tencent.QQ for QQ. Installs run silently by default (no installer-wizard clicks); pass silent=false only if the user wants to watch or click the installer UI. If the final job result reports all winget candidates failed or no candidates, explain that concrete result and only then use find_tool to load web/download tools for a targeted official fallback if the user still wants it."#;

// ── 触发正则（对齐 capability-registry.js 各 KEYWORD_RE；detect 用 raw_text） ──

fn weather_keyword_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"天气|温度|气温|下雨|降雨|下雪|台风|雾霾|阴天|晴天|多云|wttr|weather")
            .expect("static regex")
    })
}

fn hotspot_keyword_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"热点|热搜|热门|新闻|今日|趋势|榜单|头条|热议|微博热搜|trending|headline")
            .expect("static regex")
    })
}

fn worldcup_keyword_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"世界杯|赛况|比分|赛程|对阵|积分榜|小组赛|淘汰赛|揭幕战|进球|几比几|world ?cup|worldcup|fifa")
            .expect("static regex")
    })
}

// ── 能力 ctx / 能力定义 ────────────────────────────────────────────────────

/// 能力判定上下文（对齐 Node ctx 形状的可用子集）：
/// `text` 为小写正文（触发词字面包含用），`raw_text` 为原文（正则判定用）。
#[derive(Debug, Clone, Default)]
pub struct CapabilityCtx {
    pub text: String,
    pub raw_text: String,
}

/// 声明式能力单元（对齐 CAPABILITIES 条目；工具自动注入门独立于 detect）。
pub struct Capability {
    pub id: &'static str,
    pub label: &'static str,
    pub summary: &'static str,
    /// 触发词（find_tool 发现 + 按需激活用；本里程碑仅持有）。
    pub triggers: &'static [&'static str],
    pub tools: &'static [&'static str],
    /// 工作流块（detect 命中且存在时注入 prompt）。
    pub context: Option<&'static str>,
    /// 工具自动注入门；None → 回落 detect。`toolWhen` 独立于 `detect`，
    /// 保留 tools / context 分面解耦（如面板工具不随关键词自动加载）。
    pub tool_when: Option<fn(&CapabilityCtx) -> bool>,
    /// 领域相关信号 → 控 context 注入 +（默认）tool 注入门。
    pub detect: fn(&CapabilityCtx) -> bool,
}

/// 通用辅助：`text` 已小写，triggers 字面包含（对齐 `hits`）。
/// 当前已迁能力的 detect 均用正则；interactive-browser / web 的 triggers 字面包含场景
/// 随后续里程碑使用。
#[allow(dead_code)]
fn hits(text: &str, triggers: &[&str]) -> bool {
    triggers.iter().any(|t| text.contains(t))
}

/// 已迁能力清单（v1：weather / hotspot / worldcup / software-install）。
/// 顺序随 Node CAPABILITIES 数组的已迁子集（weather → hotspot → worldcup → software-install）。
pub const CAPABILITIES: [Capability; 4] = [
    Capability {
        id: "weather",
        label: "天气",
        summary: "查实时天气（仅 wttr.in 取数）并以 weather 卡片投影；含地理实况预喂。",
        triggers: &[
            "天气", "温度", "气温", "下雨", "下雪", "台风", "weather", "wttr",
        ],
        // 天气固定用 web_read 抓 wttr.in，不注入搜索或浏览器工具。
        tools: &WEB_READ_TOOLS,
        context: Some(WEATHER_CONTEXT_BLOCK),
        tool_when: None,
        detect: |ctx| weather_keyword_re().is_match(&ctx.raw_text),
    },
    Capability {
        id: "hotspot",
        label: "热点面板",
        summary: "打开热搜/趋势可视化面板（hotspot_mode）；面板开启时实时热点数据自动预喂。",
        triggers: &[
            "热点",
            "热搜",
            "热门",
            "新闻",
            "今日",
            "趋势",
            "榜单",
            "头条",
            "trending",
            "news",
            "hot ",
            "top ",
            "微博热搜",
            "热议",
        ],
        tools: &HOTSPOT_TOOLS,
        context: Some(HOTSPOT_CONTEXT_BLOCK),
        // 面板工具不自动注入；无论用户轮还是 Tick，Agent 判断需要后经 find_tool 装载。
        tool_when: Some(|_| false),
        detect: |ctx| hotspot_keyword_re().is_match(&ctx.raw_text),
    },
    Capability {
        id: "worldcup",
        label: "世界杯面板",
        summary: "打开世界杯比分/赛程/积分榜面板（worldcup_mode）；面板开启时赛况自动预喂。",
        triggers: &[
            "世界杯",
            "赛况",
            "比分",
            "赛程",
            "对阵",
            "积分榜",
            "小组赛",
            "淘汰赛",
            "谁赢",
            "进球",
            "几比几",
            "揭幕战",
            "球赛",
            "足球赛",
            "world cup",
            "worldcup",
            "fifa",
        ],
        tools: &WORLDCUP_TOOLS,
        context: Some(WORLDCUP_CONTEXT_BLOCK),
        // 工具不自动注入（schema 较大且拖 WEB_TOOLS）；只递规则块，Agent 想用时 find_tool 装载。
        tool_when: Some(|_| false),
        detect: |ctx| worldcup_keyword_re().is_match(&ctx.raw_text),
    },
    Capability {
        id: "software-install",
        label: "安装软件",
        summary: "用 winget 静默安装 Windows 软件，后台 job 进度以 progress 卡实时投影。",
        triggers: &[],
        tools: &SOFTWARE_INSTALL_TOOLS,
        context: Some(SOFTWARE_INSTALL_CONTEXT_BLOCK),
        tool_when: None,
        detect: |ctx| is_software_install_request(&ctx.raw_text),
    },
];

// ── 消费端 helpers（对齐 capability-registry.js） ───────────────────────────

/// 领域相关的能力（detect 命中）——用于 context 注入与自感知「现在哪些能力在场」。
pub fn select_active_capabilities(ctx: &CapabilityCtx) -> Vec<&'static Capability> {
    CAPABILITIES.iter().filter(|c| (c.detect)(ctx)).collect()
}

/// 本轮要注入的工作流块（detect 命中且能力有 context）；顺序随 CAPABILITIES 数组。
pub fn capability_context_blocks(ctx: &CapabilityCtx) -> Vec<String> {
    select_active_capabilities(ctx)
        .iter()
        .filter_map(|c| c.context.map(|s| s.to_string()))
        .collect()
}

/// 本轮要自动注入的工具名（去重）。每能力用 tool_when（缺省回落 detect）单独判断，
/// 保留 tools / context 解耦。
pub fn capability_tools_for(ctx: &CapabilityCtx) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in &CAPABILITIES {
        let gate = c.tool_when.unwrap_or(c.detect);
        if gate(ctx) {
            for name in c.tools {
                if !out.iter().any(|n| n == name) {
                    out.push((*name).to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(raw: &str) -> CapabilityCtx {
        CapabilityCtx {
            text: raw.to_lowercase(),
            raw_text: raw.to_string(),
        }
    }

    #[test]
    fn weather_keyword_injects_weather_block_only() {
        let blocks = capability_context_blocks(&ctx("今天上海天气怎么样"));
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].starts_with("### Weather Surface Rules"));
        // 工具自动注入门（无 toolWhen → 回落 detect）：web_read
        let tools = capability_tools_for(&ctx("今天上海天气怎么样"));
        assert_eq!(tools, vec!["web_read"]);
    }

    #[test]
    fn hotspot_keyword_injects_block_but_never_tools() {
        let blocks = capability_context_blocks(&ctx("看看今天微博热搜"));
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("hotspot_mode"));
        // toolWhen = () => false → 工具永不自动注入
        assert!(capability_tools_for(&ctx("看看今天微博热搜")).is_empty());
    }

    #[test]
    fn worldcup_keyword_injects_block_but_never_tools() {
        let blocks = capability_context_blocks(&ctx("世界杯决赛比分多少"));
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("worldcup_mode"));
        assert!(capability_tools_for(&ctx("世界杯决赛比分多少")).is_empty());
    }

    #[test]
    fn software_install_request_injects_install_block() {
        let blocks = capability_context_blocks(&ctx("帮我安装微信"));
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].starts_with("## Software Install Workflow"));
        let tools = capability_tools_for(&ctx("帮我安装微信"));
        assert!(tools.contains(&"install_software".to_string()));
        assert!(tools.contains(&"list_processes".to_string()));
    }

    #[test]
    fn multi_hit_keeps_capabilities_order() {
        // 同时命中天气 + 热搜 → 块顺序随 CAPABILITIES：weather → hotspot
        let blocks = capability_context_blocks(&ctx("今天天气和微博热搜都看看"));
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with("### Weather Surface Rules"));
        assert!(blocks[1].starts_with("### Hotspot Panel"));
    }

    #[test]
    fn unrelated_message_injects_nothing() {
        assert!(capability_context_blocks(&ctx("帮我把前端轮子重做一遍")).is_empty());
        assert!(capability_tools_for(&ctx("帮我把前端轮子重做一遍")).is_empty());
    }
}
