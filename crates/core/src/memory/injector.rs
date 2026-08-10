//! 注入器 · 编排层（对齐 `src/memory/injector.js`）。
//!
//! runInjector 把 M4 记忆系统的各原语串成一次注入回合：
//! 消息解析 → 说话人上下文 / 时间词轮廓召回 → 焦点与上下文文本组装 →
//! 置信度缩放后的检索限额 → FTS5+向量相关召回 → 主动召回（RECALL）→
//! 「少即是强」选择 → 浏览器/API 配置方向提示 → UI 信号摘要 → 组装输出。
//!
//! 依赖边界（与 Node 版的对应关系）：
//! - 已移植：`retrieval`（检索原语）、`keywords`、`temporal`、`threads`、
//!   `db::repositories::{memories, conversations, ui_signals}`
//! - 尚未移植的外部子系统（tool-router / self-perception / active-policies /
//!   self-evolution / browser-tools / person-memory 等）以 [`InjectorContext`] 输入注入，
//!   本模块只透传并组装，不替它们做决策——后续里程碑各自落地后改为直连。
//!
//! 纯函数部分（限额计算、文本组装、方向判定、UI 摘要）全部可单测，不拉 DB。

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::db::models::{Conversation, Memory};
use crate::db::repositories::conversations::{recent_by_party, recent_timeline};
use crate::db::repositories::memories::{get_by_entity, search, ScoredMemory};
use crate::db::repositories::ui_signals::{get_unconsumed_ui_signals, mark_ui_signals_consumed};
use crate::db::Db;
use crate::embedding::Embedder;
use crate::memory::retrieval::{
    deduplicate_memories, gather_temporal_recall, parse_message_input, search_relevant_memories,
    select_context_memories, SearchOptions, SelectOptions, TemporalBucket,
};

use super::injector_format::summarize_ui_signals;
use super::keywords::extract_keywords;
use super::temporal::strip_temporal_words;

// ── 常量（对齐 injector.js） ───────────────────────────────────────────────

/// TICK 场景的对话时间窗：7 天
const L2_CONTEXT_HOURS: i64 = 24 * 7;
/// 召回上限：有对话历史放宽到 30，否则 12
const MERGE_CAP_WITH_HISTORY: usize = 30;
const MERGE_CAP_NO_HISTORY: usize = 12;
/// hint 截断 / 对话文本截断 / RECALL 主查条数 / RECALL 每关键词补漏条数
const HINT_MAX_CHARS: usize = 800;
const CONVERSATION_MAX_CHARS: usize = 4000;
const RECALL_LIMIT: u32 = 5;
const RECALL_PER_KEYWORD: u32 = 3;
/// UI 信号窗口（毫秒）
const UI_SIGNAL_WINDOW_MS: i64 = 60_000;
/// 默认 agent 名（getConfig('agent_name') 缺失时）
const DEFAULT_AGENT_NAME: &str = "小白龙";

// ── 正则（对齐 injector.js 词表） ─────────────────────────────────────────

static SELF_EVOLUTION_CONTEXT_RE: OnceLock<Regex> = OnceLock::new();
static API_KEY_RE: OnceLock<Regex> = OnceLock::new();
static API_DOCS_RE: OnceLock<Regex> = OnceLock::new();
static API_CONFIG_CONFIRM_RE: OnceLock<Regex> = OnceLock::new();
static API_SETUP_NEED_RE: OnceLock<Regex> = OnceLock::new();
static THINK_TAG_RE: OnceLock<Regex> = OnceLock::new();

fn self_evolution_context_re() -> &'static Regex {
    SELF_EVOLUTION_CONTEXT_RE.get_or_init(|| {
        Regex::new(r"(?i)self[-\s]?evol|evolv|self[-\s]?improv|improve yourself|learn(?:ed|ing)?\s+(?:from|that|this)|lesson|policy|procedure|constraint|failure|feedback|自进化|进化|自学习|学到了|改进|教训|经验|规则|策略|反思")
            .expect("static regex")
    })
}

fn api_key_re() -> &'static Regex {
    API_KEY_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:sk|ak|rk|pk|ark)-[A-Za-z0-9_\-.]{12,180}\b").expect("static regex")
    })
}

fn api_docs_re() -> &'static Regex {
    API_DOCS_RE.get_or_init(|| {
        Regex::new(r"(?i)https?://|api|docs?|platform|capability|endpoint|base[-_\s]?url|model|auth|文档|接口|配置|能力")
            .expect("static regex")
    })
}

fn api_config_confirm_re() -> &'static Regex {
    API_CONFIG_CONFIRM_RE.get_or_init(|| {
        Regex::new(r"^(?:yes|yep|ok|okay|sure|do it|go ahead|是|是的|可以|好|好的|对|行|配置|配上|设置|设成)$")
            .expect("static regex")
    })
}

fn api_setup_need_re() -> &'static Regex {
    API_SETUP_NEED_RE.get_or_init(|| {
        Regex::new(r"(?i)not_configured|slot_not_found|credential_not_configured|api_key required|configure|capability")
            .expect("static regex")
    })
}

fn think_tag_re() -> &'static Regex {
    THINK_TAG_RE.get_or_init(|| Regex::new(r"(?is)<think>.*?</think>").expect("static regex"))
}

// ── 纯函数：自我进化上下文 / API 配置方向 ─────────────────────────────────

/// 是否注入自我进化上下文（TICK 恒注入；普通消息看进化相关词，对齐 shouldInjectSelfEvolutionContext）。
pub fn should_inject_self_evolution_context(message_body: &str, is_tick: bool) -> bool {
    if is_tick {
        return true;
    }
    self_evolution_context_re().is_match(message_body)
}

/// 一条行动日志（action_logs 行的最小投影；tool-router 里程碑后改为直连仓库）。
#[derive(Debug, Clone, Default)]
pub struct ActionLogEntry {
    pub tool: String,
    pub status: String,
    pub error: String,
    pub result_preview: String,
    pub args_json: String,
}

/// 近期是否有 API 能力配置需求（对齐 hasRecentApiCapabilitySetupNeed）：
/// analyze_image / manage_api_capability 的最近结果里出现 not_configured 等信号。
pub fn has_recent_api_capability_setup_need(action_log: &[ActionLogEntry]) -> bool {
    action_log.iter().any(|entry| {
        if entry.tool != "analyze_image" && entry.tool != "manage_api_capability" {
            return false;
        }
        let text = format!(
            "{} {} {} {}",
            entry.status, entry.error, entry.result_preview, entry.args_json
        );
        api_setup_need_re().is_match(&text)
    })
}

/// API 配置方向提示（对齐 injector.js 的两条 direction）。
pub fn api_config_direction(
    message_body: &str,
    action_log: &[ActionLogEntry],
) -> Option<&'static str> {
    if api_key_re().is_match(message_body) && api_docs_re().is_match(message_body) {
        return Some("The current user message includes API documentation/config context plus an API key. Treat it as intent to configure an API-backed capability. Prefer manage_api_capability(action=\"configure\" or action=\"save_doc\") in this turn; for OpenAI-compatible vision APIs, do not build an ad-hoc tool or run raw scripts.");
    }
    let trimmed = message_body.trim().to_lowercase();
    if api_config_confirm_re().is_match(&trimmed)
        && has_recent_api_capability_setup_need(action_log)
    {
        return Some("The user is confirming your immediately previous offer to configure an API capability after a not_configured or missing-credential result. Call manage_api_capability to configure the capability slot using the provider/docs/model/key already in recent context; do not switch to tool factory or an ad-hoc script.");
    }
    None
}

// ── 纯函数：hint 清理 / 限额计算 / 文本组装 ───────────────────────────────

/// 清理 hint：剥掉 `<think>...</think>` 块并截断到 800 字符（对齐 hintText 处理）。
pub fn clean_hint(hint: &str) -> String {
    if hint.is_empty() {
        return String::new();
    }
    think_tag_re()
        .replace_all(hint, "")
        .chars()
        .take(HINT_MAX_CHARS)
        .collect()
}

/// 置信度提示（消费即焚，对齐 pendingConfidenceHint）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfidenceHint {
    Low,
    Medium,
    High,
}

impl ConfidenceHint {
    /// CONF_MULT：低置信度放宽检索额度，高置信度收紧。
    pub fn multiplier(self) -> f64 {
        match self {
            ConfidenceHint::Low => 1.5,
            ConfidenceHint::Medium => 1.0,
            ConfidenceHint::High => 0.7,
        }
    }
}

/// Math.max(1, Math.round(n * mult))（对齐 scale）。
pub fn scale_limit(n: usize, mult: f64) -> usize {
    ((n as f64) * mult).round().max(1.0) as usize
}

/// 一轮注入的检索限额（对齐 base* + CONF_MULT 缩放）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextLimits {
    pub focus_limit: usize,
    pub context_limit: usize,
    pub focus_keywords: usize,
    pub context_keywords: usize,
}

/// has_history / has_hint / confidence → 限额（对齐 injector.js 112-144 行）。
pub fn compute_context_limits(
    has_history: bool,
    has_hint: bool,
    confidence: Option<ConfidenceHint>,
) -> ContextLimits {
    let mult = confidence.map(ConfidenceHint::multiplier).unwrap_or(1.0);
    let base_focus = if has_history {
        15
    } else if has_hint {
        12
    } else {
        8
    };
    let base_context = if has_history { 10 } else { 0 };
    // hasHistory 与 hint 的 focus keywords 基数同为 10（对齐 injector.js 138 行）
    let base_focus_keywords = if has_history || has_hint { 10 } else { 8 };
    let base_context_keywords = if has_history { 14 } else { 0 };
    ContextLimits {
        focus_limit: scale_limit(base_focus, mult),
        // 0 不放大（has_history=false 时 context 路径整体关掉）
        context_limit: if base_context == 0 {
            0
        } else {
            scale_limit(base_context, mult)
        },
        focus_keywords: scale_limit(base_focus_keywords, mult),
        context_keywords: if base_context_keywords == 0 {
            0
        } else {
            scale_limit(base_context_keywords, mult)
        },
    }
}

/// 召回上限：有对话历史 30，否则 12（对齐 mergeCap）。
pub fn merge_cap(has_history: bool) -> usize {
    if has_history {
        MERGE_CAP_WITH_HISTORY
    } else {
        MERGE_CAP_NO_HISTORY
    }
}

/// 对话窗口 → 拼接文本（空格连接、截 4000 字符；对齐 conversationText）。
pub fn build_conversation_text(window: &[Conversation]) -> String {
    window
        .iter()
        .map(|c| c.content.as_str())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(CONVERSATION_MAX_CHARS)
        .collect()
}

/// 焦点文本组装（对齐 focusText）：时间词剥除后的消息体 + task + hint，空格连接。
pub fn build_focus_text(
    body: &str,
    task: Option<&str>,
    hint: &str,
    strip_temporal: bool,
) -> String {
    let body_part = if strip_temporal {
        strip_temporal_words(body)
    } else {
        body.to_string()
    };
    [
        body_part,
        task.unwrap_or_default().to_string(),
        hint.to_string(),
    ]
    .into_iter()
    .filter(|s| !s.trim().is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

// ── 主动召回（RECALL） ─────────────────────────────────────────────────────

/// 处理上一刻的 RECALL 请求：主查 5 条；空则逐关键词补漏（id 去重、上限 5）。
/// 返回 (命中记忆, 方向提示)。方向提示恒有（命中/未命中各一条文案，对齐 Node）。
pub fn handle_recall(db: &Db, query: &str) -> (Vec<Memory>, String) {
    let mut hits = search(db, query, RECALL_LIMIT).unwrap_or_default();
    if hits.is_empty() {
        let keywords = extract_keywords(query, 12);
        let mut seen: HashSet<i64> = HashSet::new();
        'outer: for keyword in &keywords {
            for m in search(db, keyword, RECALL_PER_KEYWORD).unwrap_or_default() {
                if seen.insert(m.id) {
                    hits.push(m);
                }
                if hits.len() as u32 >= RECALL_LIMIT {
                    break 'outer;
                }
            }
        }
    }
    if hits.is_empty() {
        (
            Vec::new(),
            format!(
                "You proactively requested memory recall for \"{query}\", but no related memory was found."
            ),
        )
    } else {
        (
            hits,
            format!(
                "You proactively requested memory recall for \"{query}\" in the previous moment. Relevant details have been injected."
            ),
        )
    }
}

// ── runInjector 输入 / 上下文 / 输出 ───────────────────────────────────────

/// run_injector 的一次性输入（对齐 runInjector 的入参 + state 投影）。
#[derive(Debug, Clone, Default)]
pub struct InjectorInput {
    pub message: String,
    pub hint: String,
    pub current_channel: String,
    /// state.task（有任务时并入焦点文本）
    pub task: Option<String>,
    /// state.prev_recall（上一刻的主动召回请求）
    pub prev_recall: Option<String>,
    /// state.pendingConfidenceHint（消费即焚）
    pub confidence_hint: Option<ConfidenceHint>,
    /// state.lastToolResult（透传回输出）
    pub last_tool_result: Option<String>,
}

/// 尚未移植子系统提供的外部上下文（本模块只透传 + 组装方向提示）。
#[derive(Debug, Clone, Default)]
pub struct InjectorContext {
    /// person memory（getPersonMemory 的结果，person 记忆的根）
    pub person_memory: Option<Memory>,
    /// 用户画像渲染文本（getUserProfile 的调用方渲染）
    pub user_profile: Option<String>,
    /// 任务知识库（getTaskKnowledge 的结果）
    pub task_knowledge: Vec<Memory>,
    /// 近期行动日志（getRecentActionLogs 的投影）
    pub action_log: Vec<ActionLogEntry>,
    /// 有效预热缓存（getValidPrefetchCache 的投影）
    pub prefetched_items: Vec<String>,
    /// 活动约束（getActiveConstraints 的结果）
    pub constraints: Vec<String>,
    /// 按意图选出的工具（selectTools 的结果；tool-router 里程碑后直连）
    pub tools: Vec<String>,
    /// 活动政策（selectActivePolicies 的结果；active-policies 里程碑后直连）
    pub active_policies: Vec<ScoredMemory>,
    /// 自我感知渲染文本（computeSelfPerception）
    pub self_perception: Option<String>,
    /// 自我快照渲染文本（computeSelfSnapshot）
    pub self_snapshot: Option<String>,
    /// 自我进化上下文渲染文本（formatSelfEvolutionForPrompt）
    pub self_evolution: String,
    /// 浏览器运行时上下文文本（formatBrowserRuntimeContext）
    pub browser_runtime_text: Option<String>,
    /// 天气预喂上下文文本（buildWeatherRuntimeContext → `## Weather Reference`）；
    /// 非 None 时 run_user_turn 直接透传，不再重复抓取。
    pub weather_runtime_text: Option<String>,
    /// 是否允许 run_user_turn 主动运行天气预喂（对齐 Node 注入器默认开启；
    /// 测试/无网络环境可关）。
    pub enable_weather_prefeed: bool,
}

/// run_injector 输出（对齐 Node 返回结构；person_memory / user_profile /
/// prefetched_items / tools 等由外部子系统注入透传）。
#[derive(Debug, Clone, Default)]
pub struct InjectorOutput {
    pub memories: Vec<ScoredMemory>,
    pub active_policies: Vec<ScoredMemory>,
    pub recall_memories: Vec<Memory>,
    pub conversation_window: Vec<Conversation>,
    pub person_memory: Option<Memory>,
    pub user_profile: Option<String>,
    pub directions: Vec<String>,
    pub constraints: Vec<String>,
    pub thought: Option<String>,
    pub task_knowledge: Vec<Memory>,
    pub tools: Vec<String>,
    pub last_tool_result: Option<String>,
    pub action_log: Vec<ActionLogEntry>,
    pub prefetched_items: Vec<String>,
    pub ui_signal_summary: String,
    pub temporal_recall: Option<Vec<TemporalBucket>>,
    pub self_perception: Option<String>,
    pub self_snapshot: Option<String>,
    pub self_evolution: String,
    pub browser_runtime_text: Option<String>,
    pub weather_runtime_text: Option<String>,
}

/// 上下文窗口配置（对齐 getContextWindowConfig 的字段）。
#[derive(Debug, Clone, Copy)]
pub struct ContextWindowConfig {
    pub conversation_message_limit: usize,
    pub tick_message_limit: usize,
}

impl Default for ContextWindowConfig {
    fn default() -> Self {
        // DEFAULT_CONTEXT_MESSAGE_LIMIT = 10
        ContextWindowConfig {
            conversation_message_limit: 10,
            tick_message_limit: 10,
        }
    }
}

// ── runInjector 编排 ───────────────────────────────────────────────────────

/// 一次注入回合（对齐 runInjector）。
///
/// DB 访问均为 best-effort（失败降级为空，不影响编排）；未移植子系统经
/// [`InjectorContext`] 注入。`agent_name` 用于身份锚（缺省取 DEFAULT_AGENT_NAME）。
pub async fn run_injector(
    db: &Db,
    embedder: &dyn Embedder,
    input: &InjectorInput,
    ctx: &InjectorContext,
    window: &ContextWindowConfig,
    agent_name: &str,
) -> InjectorOutput {
    let last_tool_result = input.last_tool_result.clone();
    let confidence = input.confidence_hint;

    let parsed = parse_message_input(&input.message);
    let is_tick_message = parsed.is_tick;
    let sender_id = parsed.sender_id.clone();
    let message_body = parsed.message_body.clone();

    // 说话人上下文：sender_id → 该实体的画像/会话/实体记忆；TICK → 主用户 + 全局时间线
    let mut conversation_window: Vec<Conversation> = Vec::new();
    let mut sender_memories: Vec<ScoredMemory> = Vec::new();
    if let Some(sid) = &sender_id {
        let limit = window.conversation_message_limit as u32;
        let since_ms = now_ms() - 24 * 3600 * 1000;
        conversation_window = recent_by_party(db, sid, limit, since_ms).unwrap_or_default();
        sender_memories = get_by_entity(db, sid, 10)
            .unwrap_or_default()
            .into_iter()
            .map(|m| ScoredMemory {
                memory: m,
                fts_score: None,
                vec_score: None,
            })
            .collect();
    } else if is_tick_message {
        let limit = window.tick_message_limit as u32;
        let since_ms = now_ms() - L2_CONTEXT_HOURS * 3600 * 1000;
        conversation_window = recent_timeline(db, limit, since_ms).unwrap_or_default();
    }

    // 时间词触发的轮廓注入：除 TICK 心跳外都跑
    let temporal_recall = if is_tick_message {
        None
    } else {
        gather_temporal_recall(db, &message_body).unwrap_or(None)
    };

    let hint_text = clean_hint(&input.hint);
    let conversation_text = build_conversation_text(&conversation_window);
    // 时间词剥除：跨边界 ngram（如"昨天我"）会污染字面搜索
    let focus_body_for_keywords = if temporal_recall.is_some() {
        strip_temporal_words(&message_body)
    } else {
        message_body.clone()
    };
    let focus_text = build_focus_text(
        &focus_body_for_keywords,
        input.task.as_deref(),
        &hint_text,
        false, // 已剥过，不再剥
    );

    let has_history = !conversation_text.is_empty();
    let limits = compute_context_limits(has_history, !hint_text.is_empty(), confidence);
    let relevant_memories = if focus_text.is_empty() {
        Vec::new()
    } else {
        search_relevant_memories(
            db,
            embedder,
            &focus_text,
            &conversation_text,
            &SearchOptions {
                focus_limit: limits.focus_limit,
                context_limit: limits.context_limit,
                focus_keywords: limits.focus_keywords,
                context_keywords: limits.context_keywords,
                per_keyword: 5,
            },
        )
        .await
    };

    let mut recall_memories: Vec<Memory> = Vec::new();
    let mut directions: Vec<String> = Vec::new();
    if let Some(query) = &input.prev_recall {
        let (hits, direction) = handle_recall(db, query);
        recall_memories.extend(hits);
        directions.push(direction);
    }

    // 「少即是强」：保相关度序，只给高 salience 锚留窄保留道
    let cap = merge_cap(has_history);
    let merged = deduplicate_memories(&[relevant_memories, sender_memories]);
    let memories = select_context_memories(
        &merged,
        &SelectOptions {
            cap,
            anchor_lane: 2,
            fts_floor: None,
        },
    );

    if let Some(browser_text) = &ctx.browser_runtime_text {
        directions.push(browser_text.clone());
    }
    if let Some(api_dir) = api_config_direction(&message_body, &ctx.action_log) {
        directions.push(api_dir.to_string());
    }

    // UI 信号：窗口内拉取 → 摘要 → 标记已消费
    let mut ui_signal_summary = String::new();
    if let Ok(signals) = get_unconsumed_ui_signals(db, UI_SIGNAL_WINDOW_MS) {
        if !signals.is_empty() {
            ui_signal_summary = summarize_ui_signals(&signals, now_ms());
            let ids: Vec<i64> = signals.iter().map(|s| s.id).collect();
            let _ = mark_ui_signals_consumed(db, &ids);
        }
    }

    // 工具去重保序（[...new Set(tools)]）
    let mut seen_tools: HashSet<String> = HashSet::new();
    let tools: Vec<String> = ctx
        .tools
        .iter()
        .filter(|t| seen_tools.insert(t.to_string()))
        .cloned()
        .collect();

    let agent_name = if agent_name.trim().is_empty() {
        DEFAULT_AGENT_NAME
    } else {
        agent_name
    };

    InjectorOutput {
        memories,
        active_policies: ctx.active_policies.clone(),
        recall_memories,
        conversation_window,
        person_memory: ctx.person_memory.clone(),
        user_profile: ctx.user_profile.clone(),
        directions,
        constraints: ctx.constraints.clone(),
        thought: None,
        task_knowledge: ctx.task_knowledge.clone(),
        tools,
        last_tool_result,
        action_log: ctx.action_log.clone(),
        prefetched_items: ctx.prefetched_items.clone(),
        ui_signal_summary,
        temporal_recall,
        self_perception: ctx.self_perception.clone(),
        self_snapshot: ctx.self_snapshot.clone(),
        self_evolution: ctx.self_evolution.clone(),
        browser_runtime_text: ctx.browser_runtime_text.clone(),
        weather_runtime_text: ctx.weather_runtime_text.clone(),
    }
    .with_agent_name_note(agent_name)
}

impl InjectorOutput {
    /// 身份锚提示：agent 名变更时补一条 direction（对齐身份锚的渲染职责；轻量实现）。
    fn with_agent_name_note(self, agent_name: &str) -> Self {
        if !agent_name.is_empty() {
            // 占位：身份锚的完整渲染在 prompt 里程碑；这里只在日志可见层面留钩子。
            let _ = agent_name;
        }
        self
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── 测试（纯函数 + 临时库集成；对齐 injector.js 行为） ─────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{normalize_conversation_party_id, now_iso, NewConversation, NewMemory};
    use crate::db::repositories::conversations::insert_conversation;
    use crate::db::repositories::memories::insert_memory;
    use crate::db::repositories::ui_signals::UiSignal;
    use crate::db::{open_database, Db};
    use crate::embedding::NoopEmbedder;
    use serde_json::json;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.db");
        open_database(path).unwrap()
    }

    fn insert_mem(db: &Db, event_type: &str, content: &str, entities: &[&str], salience: i64) {
        insert_memory(
            db,
            &NewMemory {
                event_type: event_type.into(),
                content: content.into(),
                detail: String::new(),
                title: String::new(),
                mem_id: None,
                entities: entities.iter().map(|s| s.to_string()).collect(),
                concepts: Vec::new(),
                tags: Vec::new(),
                links: Vec::new(),
                salience,
                source_ref: None,
                timestamp: now_iso(),
                parent_id: None,
                embedding: None,
                embedding_dim: None,
                embedding_model: None,
            },
        )
        .unwrap();
    }

    fn insert_conv(db: &Db, role: &str, from_id: &str, content: &str) {
        let from_id = normalize_conversation_party_id(Some(from_id)).unwrap_or_default();
        insert_conversation(
            db,
            &NewConversation {
                role: role.into(),
                from_id,
                to_id: None,
                content: content.into(),
                timestamp: now_iso(),
                channel: "wechat".into(),
                external_party_id: String::new(),
                focus_topic: String::new(),
                open_question: false,
                thread_id: String::new(),
                delivery_status: String::new(),
            },
        )
        .unwrap();
    }

    fn run(
        db: &Db,
        message: &str,
        task: Option<&str>,
        prev_recall: Option<&str>,
    ) -> InjectorOutput {
        let input = InjectorInput {
            message: message.into(),
            hint: String::new(),
            current_channel: "wechat".into(),
            task: task.map(|s| s.to_string()),
            prev_recall: prev_recall.map(|s| s.to_string()),
            confidence_hint: None,
            last_tool_result: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(run_injector(
            db,
            &NoopEmbedder,
            &input,
            &InjectorContext::default(),
            &ContextWindowConfig::default(),
            "小白龙",
        ))
    }

    // ── 纯函数 ──

    #[test]
    fn self_evolution_context_detection() {
        assert!(should_inject_self_evolution_context("", true)); // TICK 恒注入
        assert!(should_inject_self_evolution_context(
            "上次的教训是什么",
            false
        ));
        assert!(should_inject_self_evolution_context("总结一下教训", false));
        assert!(should_inject_self_evolution_context(
            "反思一下这个失败",
            false
        ));
        assert!(should_inject_self_evolution_context(
            "what did you learn from this",
            false
        ));
        // 对齐 Node 词表："失败" 本身不在表内 → 不注入
        assert!(!should_inject_self_evolution_context(
            "总结一下这个失败",
            false
        ));
        assert!(!should_inject_self_evolution_context("今天天气不错", false));
    }

    #[test]
    fn api_setup_need_detection() {
        let log = vec![ActionLogEntry {
            tool: "analyze_image".into(),
            status: "error".into(),
            error: "not_configured: no API key".into(),
            ..Default::default()
        }];
        assert!(has_recent_api_capability_setup_need(&log));
        let unrelated = vec![ActionLogEntry {
            tool: "browser_click".into(),
            status: "ok".into(),
            ..Default::default()
        }];
        assert!(!has_recent_api_capability_setup_need(&unrelated));
    }

    #[test]
    fn api_direction_heuristics() {
        let empty: Vec<ActionLogEntry> = Vec::new();
        // key + docs 线索 → 配置意图
        assert!(api_config_direction("sk-abcdefghijklmnop 这是接口文档", &empty).is_some());
        // 确认词 + 近期配置失败 → 确认方向
        let setup_need = vec![ActionLogEntry {
            tool: "manage_api_capability".into(),
            error: "slot_not_found".into(),
            ..Default::default()
        }];
        assert!(api_config_direction("好的", &setup_need).is_some());
        // 确认词但没有配置失败 → 无方向
        assert!(api_config_direction("好的", &empty).is_none());
        // 普通消息 → 无方向
        assert!(api_config_direction("今天天气怎么样", &empty).is_none());
    }

    #[test]
    fn hint_cleaning_strips_think_tags() {
        assert_eq!(
            clean_hint("前因后果 <think>内部推理</think> 结论"),
            "前因后果  结论"
        );
        assert_eq!(clean_hint(""), "");
    }

    #[test]
    fn confidence_scales_limits() {
        assert_eq!(scale_limit(8, 1.5), 12);
        assert_eq!(scale_limit(8, 0.7), 6);
        assert_eq!(scale_limit(1, 0.7), 1); // 地板 1
        let low = compute_context_limits(true, false, Some(ConfidenceHint::Low));
        assert_eq!(low.focus_limit, 23); // 15 * 1.5
        assert_eq!(low.context_limit, 15); // 10 * 1.5
        assert_eq!(low.context_keywords, 21); // 14 * 1.5
        let high = compute_context_limits(true, false, Some(ConfidenceHint::High));
        assert_eq!(high.focus_limit, 11); // 15 * 0.7 ≈ 10.5 → 11
        let no_history = compute_context_limits(false, false, None);
        assert_eq!(no_history.focus_limit, 8);
        assert_eq!(no_history.context_limit, 0); // context 路径关掉
        assert_eq!(no_history.context_keywords, 0);
        let hint_only = compute_context_limits(false, true, None);
        assert_eq!(hint_only.focus_limit, 12);
        assert_eq!(hint_only.focus_keywords, 10);
    }

    #[test]
    fn merge_cap_by_history() {
        assert_eq!(merge_cap(true), 30);
        assert_eq!(merge_cap(false), 12);
    }

    #[test]
    fn focus_text_builds_with_task_and_hint() {
        let t = build_focus_text(
            "部署脚本优化一下",
            Some("继续优化部署"),
            "参考昨天的思路",
            false,
        );
        assert!(t.contains("部署脚本优化一下"));
        assert!(t.contains("继续优化部署"));
        assert!(t.contains("参考昨天的思路"));
        // 时间词剥除（temporal 命中时的消息体）
        let stripped = build_focus_text("昨天我部署了脚本", None, "", true);
        assert!(!stripped.contains("昨天"));
        assert!(stripped.contains("部署"));
    }

    #[test]
    fn ui_signal_summary_formats() {
        let now = now_ms();
        let signals = vec![
            UiSignal {
                id: 1,
                r#type: "card.mounted".into(),
                target: Some("#panel".into()),
                payload: json!({}),
                ts: now - 3000,
            },
            UiSignal {
                id: 2,
                r#type: "card.action".into(),
                target: None,
                payload: json!({"action": "click"}),
                ts: now - 10_000,
            },
            UiSignal {
                id: 3,
                r#type: "card.dismissed".into(),
                target: None,
                payload: json!({"by": "user", "dwell_ms": 2000}),
                ts: now - 20_000,
            },
        ];
        let s = summarize_ui_signals(&signals, now);
        assert!(s.contains("3s ago: Card finished mounting (#panel)"));
        assert!(s.contains("10s ago: User acted on card: click"));
        assert!(s.contains("20s ago: User dismissed the card (user, dwell 2s)"));
        assert!(summarize_ui_signals(&[], now).is_empty());
    }

    // ── RECALL ──

    #[test]
    fn recall_finds_by_keyword_expansion() {
        let db = test_db();
        insert_mem(&db, "fact", "部署脚本配置了 nginx 反代", &[], 3);
        // 主查无精确命中 → 逐关键词补漏
        let (hits, direction) = handle_recall(&db, "反代配置");
        assert!(!hits.is_empty());
        assert!(direction.contains("Relevant details have been injected"));
    }

    #[test]
    fn recall_missing_reports_no_result() {
        let db = test_db();
        insert_mem(&db, "fact", "部署脚本配置了 nginx", &[], 3);
        let (hits, direction) = handle_recall(&db, "量子纠缠实验");
        assert!(hits.is_empty());
        assert!(direction.contains("no related memory was found"));
    }

    // ── run_injector 集成 ──

    #[test]
    fn injector_gathers_relevant_memories_and_directions() {
        let db = test_db();
        insert_mem(&db, "fact", "部署脚本已配置 nginx 反代", &["部署"], 5);
        insert_mem(&db, "fact", "用户偏好深色主题", &["user1"], 3);
        insert_conv(&db, "user", "ID:user1", "把部署脚本优化一下");
        let out = run(
            &db,
            "[ID:user1] 2026-08-09-10:00:00 [wechat] 部署脚本怎么优化",
            None,
            None,
        );
        // 有对话历史 → cap 30；部署相关记忆应被召回
        assert!(!out.memories.is_empty());
        assert!(out
            .memories
            .iter()
            .any(|m| m.memory.content.contains("nginx")));
        // sender_id 存在 → person 相关上下文路径开启
        assert_eq!(out.conversation_window.len(), 1);
        assert_eq!(out.person_memory, None);
        assert!(out.temporal_recall.is_none());
    }

    #[test]
    fn injector_handles_tick_message() {
        let db = test_db();
        insert_conv(&db, "user", "ID:user1", "昨天聊到部署脚本");
        let out = run(&db, "TICK 2026-08-09-10:00:00", None, None);
        assert!(out.memories.is_empty() || out.conversation_window.is_empty());
        // TICK 走全局时间线：对话窗口应能拉到
        assert_eq!(out.conversation_window.len(), 1);
    }

    #[test]
    fn injector_processes_recall_and_api_direction() {
        let db = test_db();
        insert_mem(&db, "fact", "昨天的部署脚本有 nginx 反代配置", &[], 4);
        let out = run(
            &db,
            "[ID:user1] 2026-08-09-10:00:00 [wechat] 回忆一下部署脚本",
            None,
            Some("部署脚本"),
        );
        assert!(!out.recall_memories.is_empty());
        assert!(out
            .directions
            .iter()
            .any(|d| d.contains("Relevant details have been injected")));

        // API key + docs → 配置方向
        let out2 = run(
            &db,
            "[ID:user1] 2026-08-09-10:00:00 [wechat] sk-abcdefghijklmnopqrst 这是接口文档",
            None,
            None,
        );
        assert!(out2
            .directions
            .iter()
            .any(|d| d.contains("manage_api_capability")));
    }
}
