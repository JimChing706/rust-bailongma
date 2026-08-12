//! 工具循环 —— agentic 主循环（对齐 Node 版 `callLLM` 的核心骨架）。
//!
//! 连续执行"模型生成 → 工具调用 → 结果回喂"直到模型停止：
//! - 循环上限 maxRounds=100 / 工具总调用上限 maxTotalCalls=30
//! - 熔断：连续失败 / 同指纹反复失败 / 近期动作去重不足（死循环）
//! - 参数别名归一化（normalizeArgs）、XML 工具调用回退（MiniMax）
//! - 工具结果注入 assistant+tool 消息；已流出内容去重
//! - M1 装配：每轮 = 一个逻辑请求（request_id 印章，重试/降级共享）；
//!   M2 台账：工具执行结果写 llm_tool_calls（ok/error/tripped + delegated_from）
//!
//! M2 范围裁剪：社交投递（send_message 派发）、closer dedup、tick 证据、
//! action contract 等 runtime 层逻辑属于后续里程碑，此处保留核心循环 + 基础 nudge。

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use futures_util::FutureExt;
use reqwest::Client;
use serde_json::{json, Value};

use crate::error::Result;
use crate::intervention::{InterventionGate, InterventionStatus};

use super::caller::{LlmConfig, StreamContext};
use super::retry::{stream_once_with_model_fallback, RetryInfo};
use super::tools::ToolSchema;
use super::replay::ToolReplayGuard;
use super::types::{
    build_tool_fingerprint, parse_xml_tool_calls, ChatCompletionRequest, ChatMessage,
    StreamOnceResult, ToolCall, ToolCallPayload, ToolFunctionPayload,
};

// ─────────────────────────────────────────────────────────────
// 循环限额（对齐 TOOL_LOOP_LIMITS）
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolLoopLimits {
    pub max_rounds: usize,
    pub max_consecutive_failures: usize,
    pub max_same_failures: usize,
    pub loop_window_size: usize,
    pub loop_unique_threshold: usize,
}

impl Default for ToolLoopLimits {
    fn default() -> Self {
        Self {
            max_rounds: 100,
            max_consecutive_failures: 3,
            max_same_failures: 2,
            loop_window_size: 8,
            loop_unique_threshold: 2,
        }
    }
}

/// 汇报通道工具（对齐 Node REPORT_CHANNEL_TOOLS，llm.js:739）：
/// send_message / express 是 agent 向用户汇报 blocker 的唯一通道，
/// 豁免「连续失败 / 死循环」熔断，防止其他工具失败连带把 agent 嘴堵住。
const REPORT_CHANNEL_TOOLS: &[&str] = &["send_message", "express"];

/// 工具循环状态（对齐 createToolLoopState）
#[derive(Debug, Clone)]
pub struct ToolLoopState {
    pub total_calls: usize,
    pub consecutive_failures: usize,
    pub same_failure_counts: HashMap<String, usize>,
    pub recent_fingerprints: Vec<String>,
}

impl Default for ToolLoopState {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolLoopState {
    pub fn new() -> Self {
        Self {
            total_calls: 0,
            consecutive_failures: 0,
            same_failure_counts: HashMap::new(),
            recent_fingerprints: Vec::new(),
        }
    }

    /// 判定本次工具调用是否应被熔断（对齐 getToolLoopStopReason）。
    /// `send_message`/`express` 为 REPORT_CHANNEL_TOOLS：豁免「连续失败」与「死循环」熔断
    /// —— agent 汇报 blocker 的唯一通道不能被其他工具（exec_command 等）的失败连带熔断
    /// （对齐 llm.js 735-739 的 lessons-bailongma-silent-exit 教训）；「同指纹失败」不豁免。
    pub fn get_stop_reason(
        &self,
        limits: &ToolLoopLimits,
        tool_name: &str,
        fingerprint: &str,
    ) -> Option<String> {
        let is_report_channel = REPORT_CHANNEL_TOOLS.contains(&tool_name);
        if !is_report_channel && self.consecutive_failures >= limits.max_consecutive_failures {
            return Some(format!(
                "too many consecutive tool failures ({})",
                limits.max_consecutive_failures
            ));
        }
        let same = self
            .same_failure_counts
            .get(fingerprint)
            .copied()
            .unwrap_or(0);
        if same >= limits.max_same_failures {
            return Some(format!("same failing action repeated {same} times"));
        }
        let window_start = self
            .recent_fingerprints
            .len()
            .saturating_sub(limits.loop_window_size);
        let window: HashSet<&str> = self.recent_fingerprints[window_start..]
            .iter()
            .map(String::as_str)
            .collect();
        if !is_report_channel
            && self.recent_fingerprints.len() >= limits.loop_window_size
            && window.len() <= limits.loop_unique_threshold
        {
            return Some(format!(
                "stuck in a loop (only {} unique action(s) in last {} calls)",
                window.len(),
                limits.loop_window_size
            ));
        }
        None
    }

    /// 记录一次工具调用结果（对齐 recordToolLoopOutcome）
    pub fn record_outcome(&mut self, fingerprint: &str, result: &str) {
        self.total_calls += 1;
        self.recent_fingerprints.push(fingerprint.to_string());
        if is_tool_failure(result) {
            self.consecutive_failures += 1;
            *self
                .same_failure_counts
                .entry(fingerprint.to_string())
                .or_insert(0) += 1;
        } else {
            self.consecutive_failures = 0;
            self.same_failure_counts.remove(fingerprint);
        }
    }
}

/// 熔断结果 JSON（对齐 makeToolLoopStoppedResult）
pub fn make_tool_loop_stopped_result(name: &str, reason: &str) -> String {
    json!({
        "ok": false,
        "tool": name,
        "error": "tool loop stopped",
        "reason": reason,
        "hint": "Stop retrying this action. Explain the blocker, ask for confirmation, or choose a materially different approach.",
    })
    .to_string()
}

/// 工具失败判定（对齐 isToolFailure）：
/// JSON `{ok:false}` 或 `{error && ok!==true}`，或中文错误前缀文本
pub fn is_tool_failure(result: &str) -> bool {
    let text = result.trim();
    if text.is_empty() {
        return false;
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(text) {
        if let Some(obj) = parsed.as_object() {
            if obj.get("ok").and_then(|v| v.as_bool()) == Some(false) {
                return true;
            }
            if obj.contains_key("error") && obj.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                return true;
            }
        }
        return false;
    }
    // 中文错误前缀（Node 正则的常见错误开头）
    const PREFIXES: [&str; 6] = [
        "错误",
        "请求失败",
        "执行失败",
        "命令超时",
        "命令执行失败",
        "参数非法",
    ];
    PREFIXES.iter().any(|p| text.starts_with(p))
}

// ─────────────────────────────────────────────────────────────
// 参数别名归一化（对齐 PARAM_ALIASES / normalizeArgs）
// ─────────────────────────────────────────────────────────────

const PARAM_ALIASES: &[(&str, &[(&str, &str)])] = &[
    (
        "send_message",
        &[
            ("to", "target_id"),
            ("message", "content"),
            ("text", "content"),
            ("recipient", "target_id"),
        ],
    ),
    (
        "read_file",
        &[("file", "path"), ("filename", "path"), ("filepath", "path")],
    ),
    (
        "write_file",
        &[
            ("file", "path"),
            ("filename", "path"),
            ("filepath", "path"),
            ("text", "content"),
            ("data", "content"),
        ],
    ),
    (
        "list_dir",
        &[("directory", "path"), ("dir", "path"), ("folder", "path")],
    ),
    (
        "make_dir",
        &[("directory", "path"), ("dir", "path"), ("folder", "path")],
    ),
    ("delete_file", &[("file", "path"), ("filename", "path")]),
    (
        "exec_command",
        &[
            ("cmd", "command"),
            ("shell", "command"),
            ("bg", "background"),
        ],
    ),
    (
        "web_search",
        &[
            ("q", "query"),
            ("keyword", "query"),
            ("keywords", "query"),
            ("search", "query"),
        ],
    ),
    (
        "web_read",
        &[("link", "url"), ("href", "url"), ("uri", "url")],
    ),
    (
        "fetch_url",
        &[("link", "url"), ("href", "url"), ("uri", "url")],
    ),
    (
        "browser_read",
        &[("link", "url"), ("href", "url"), ("uri", "url")],
    ),
    (
        "search_memory",
        &[("q", "keyword"), ("query", "keyword"), ("term", "keyword")],
    ),
];

/// 参数别名归一化（对齐 normalizeArgs：别名在且规范名不在时替换）
pub fn normalize_args(tool_name: &str, args: &Value) -> Value {
    let Some(aliases) = PARAM_ALIASES.iter().find(|(n, _)| *n == tool_name) else {
        return args.clone();
    };
    let Value::Object(map) = args else {
        return args.clone();
    };
    let mut out = map.clone();
    for (alias, canonical) in aliases.1 {
        if out.contains_key(*alias) && !out.contains_key(*canonical) {
            let v = out.remove(*alias).expect("contains_key checked");
            out.insert((*canonical).to_string(), v);
        }
    }
    Value::Object(out)
}

// ─────────────────────────────────────────────────────────────
// 流函数类型（测试可注入 mock，对齐 Node _streamOnceForTest）
// ─────────────────────────────────────────────────────────────

pub type RetryCb = Arc<dyn Fn(RetryInfo) + Send + Sync>;
pub type StreamFuture = futures_util::future::BoxFuture<'static, Result<StreamOnceResult>>;
pub type StreamFn = dyn Fn(&Client, &LlmConfig, &ChatCompletionRequest, &StreamContext, Option<RetryCb>) -> StreamFuture
    + Send
    + Sync;

/// 生产流函数：带重试 + MiMo 模型降级
pub fn real_stream_fn() -> Arc<StreamFn> {
    Arc::new(
        |client: &Client,
         cfg: &LlmConfig,
         request: &ChatCompletionRequest,
         ctx: &StreamContext,
         on_retry: Option<RetryCb>| {
            let client = client.clone();
            let cfg = cfg.clone();
            let request = request.clone();
            let ctx = ctx.clone();
            async move {
                stream_once_with_model_fallback(&client, &cfg, &request, &ctx, on_retry).await
            }
            .boxed()
        },
    )
}

// ─────────────────────────────────────────────────────────────
// 工具执行器
// ─────────────────────────────────────────────────────────────

/// 工具执行接口（M5 由真实执行器实现；M2 用 demo 执行器验证循环）
pub trait ToolExecutor: Send + Sync {
    /// 执行工具，返回结果字符串（通常为 JSON）
    fn execute(&self, name: &str, args: &Value) -> Result<String>;
}

/// 工具调用回调（UI/遥测）
pub type OnToolCall = Arc<dyn Fn(&str, &Value, &str) + Send + Sync>;

// ─────────────────────────────────────────────────────────────
// call_llm 参数与结果
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CallLlmArgs {
    pub system_prompt: String,
    pub message: String,
    pub messages: Option<Vec<ChatMessage>>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub tools: Vec<ToolSchema>,
    pub max_tokens: Option<u32>,
    pub thinking: bool,
    /// 本地渠道（TUI/语音）：纯文本即回复，无需 send_message
    pub local_reply: bool,
    /// 本轮是否要求回复用户
    pub must_reply: bool,
    /// M2 协作信任账本：发起委托的上级 agent（主 agent 直调为空串；委托链路接线后由上层填充）
    pub delegated_from: String,
    /// P1-2 测试接缝：固定本轮逻辑请求 ID 前缀（`{seed}#{round}`，跨调用确定），
    /// 供「响应丢失 → 同逻辑请求重试」故障注入测试使用。None = 生产行为（每轮新 ID）。
    pub round_request_id_seed: Option<String>,
    /// Q6 人工介入硬通道（None = 未接入，零侵入；生产经 AppRuntime 注入）。
    pub intervention: Option<Arc<InterventionGate>>,
}

impl Default for CallLlmArgs {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            message: String::new(),
            messages: None,
            temperature: None,
            top_p: None,
            tools: Vec::new(),
            max_tokens: None,
            thinking: true,
            local_reply: true,
            must_reply: true,
            delegated_from: String::new(),
            round_request_id_seed: None,
            intervention: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResultInfo {
    pub name: String,
    pub args: Value,
    pub result: String,
}

#[derive(Debug, Clone, Default)]
pub struct LlmCallResult {
    pub content: String,
    pub tool_result: Option<ToolResultInfo>,
    pub aborted: bool,
    pub delivered: bool,
    pub total_calls: usize,
    /// Q6 人工介入：本轮被硬暂停（check 命中 pause）。
    pub intervened: bool,
}

/// 单次工具执行结果（内部）
struct ToolExecOutcome {
    id: String,
    name: String,
    result: String,
}

// ─────────────────────────────────────────────────────────────
// 主循环（对齐 callLLM 核心骨架）
// ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)] // 与 Node callLLM 参数一一对应，保持对齐
pub async fn call_llm(
    client: &Client,
    cfg: &LlmConfig,
    stream: &StreamFn,
    executor: &dyn ToolExecutor,
    args: &CallLlmArgs,
    ctx: &StreamContext,
    on_tool_call: Option<OnToolCall>,
    limits: &ToolLoopLimits,
    replay_guard: Option<&dyn ToolReplayGuard>,
) -> Result<LlmCallResult> {
    let mut messages = match &args.messages {
        Some(msgs) if !msgs.is_empty() => msgs.clone(),
        _ => vec![
            ChatMessage::system(args.system_prompt.clone()),
            ChatMessage::user(args.message.clone()),
        ],
    };
    let tools_json: Vec<Value> = args.tools.iter().map(|t| t.to_openai_value()).collect();

    let mut state = ToolLoopState::new();
    let mut all_content = String::new();
    let mut last_tool_result: Option<ToolResultInfo> = None;
    let mut delivered = false;
    // 工具结果回喂后的继续指令是否已发过（一 turn 一次的"不确定回退"检查点）
    let mut uncertainty_used = false;
    let aborted = ctx.is_aborted();
    // Q6 人工介入：本轮是否被硬暂停（检查点命中）
    let mut intervened = false;

    let mut round = 0usize;
    let mut last_round_ctx: Option<StreamContext> = None;
    while round < limits.max_rounds {
        if ctx.is_aborted() {
            break;
        }
        // ── Q6 人工介入：轮级检查点（暂停 → 立即停循环，不发起新 LLM 请求）──
        if let Some(gate) = &args.intervention {
            if let InterventionStatus::Paused { .. } = gate.check() {
                tracing::warn!("[介入] 人工暂停命中轮级检查点，工具循环停止");
                intervened = true;
                break;
            }
        }

        // ── M1 装配：每轮 = 一个逻辑请求；该轮内重试/降级共享同一 request_id ──
        //（llm_calls 幂等键 + llm_tool_calls 关联键，见 DESIGN_LLM_METRICS.md）
        let round_ctx = StreamContext {
            request_id: Some(match &args.round_request_id_seed {
                Some(seed) => format!("{seed}#{round}"),
                None => super::metrics::new_request_id(),
            }),
            ..ctx.clone()
        };
        last_round_ctx = Some(round_ctx.clone());

        let request = super::caller::build_chat_completion_request(
            &cfg.provider,
            &cfg.model,
            messages.clone(),
            args.temperature,
            args.top_p,
            args.max_tokens,
            args.thinking,
            &tools_json,
        );

        let round_result = match stream(client, cfg, &request, &round_ctx, None).await {
            Ok(r) => r,
            Err(e) => {
                // 已有可投递回复时错误不吞掉内容（对齐 Node：有内容时跳出走兜底）
                if all_content.trim().is_empty() {
                    return Err(e);
                }
                tracing::warn!(
                    "[LLM] 轮内请求中断/失败({})，已有可投递回复 —— 跳出",
                    e.to_string().chars().take(80).collect::<String>()
                );
                break;
            }
        };

        // 跨轮累积去重（对齐 appendContent）
        if !round_result.content.trim().is_empty() {
            let trimmed = round_result.content.trim().to_string();
            if !all_content.trim().ends_with(&trimmed) {
                if all_content.is_empty() {
                    all_content = round_result.content.clone();
                } else {
                    all_content.push('\n');
                    all_content.push_str(&round_result.content);
                }
            }
        }

        if round_result.aborted {
            break;
        }

        // XML 工具调用回退（MiniMax）：JSON tool_calls 为空时从正文解析
        let mut effective_tool_calls: Vec<ToolCall> = round_result.tool_calls.clone();
        let mut xml_round = false;
        if effective_tool_calls.is_empty() && !round_result.content.trim().is_empty() {
            let xml = parse_xml_tool_calls(&round_result.content);
            if !xml.is_empty() {
                tracing::info!("[工具调用] 检测到 XML 格式工具调用，共 {} 个", xml.len());
                effective_tool_calls = xml;
                xml_round = true;
                // 从 all_content 移除 XML 块
                all_content = strip_invoke_blocks(&all_content);
            }
        }

        // 无工具调用：本轮结束
        if effective_tool_calls.is_empty() {
            // local reply：纯文本即回复，直接收尾
            break;
        }

        // 为缺失 id 的工具调用分配 id
        for (i, tc) in effective_tool_calls.iter_mut().enumerate() {
            if tc.id.is_empty() {
                tc.id = format!("tool_{round}_{i}");
            }
        }

        // 执行工具
        let mut tool_results: Vec<ToolExecOutcome> = Vec::new();
        let mut tool_loop_stop_reason: Option<String> = None;
        for tc in &effective_tool_calls {
            if ctx.is_aborted() {
                break;
            }
            // ── Q6 人工介入：派发级检查点（暂停 → 不再执行任何新工具）──
            if let Some(gate) = &args.intervention {
                if let InterventionStatus::Paused { notice } = gate.check() {
                    tracing::warn!("[介入] 人工暂停（{notice}），停止派发工具 {}", tc.name);
                    intervened = true;
                    break;
                }
            }
            let args_value = tc.parse_args();
            let normalized = normalize_args(&tc.name, &args_value);
            let fingerprint = build_tool_fingerprint(&tc.name, &normalized);

            let stop_reason = state.get_stop_reason(limits, &tc.name, &fingerprint);
            let (result, stopped) = match stop_reason {
                Some(reason) => {
                    tracing::warn!("[工具熔断] {}: {}", tc.name, reason);
                    state.consecutive_failures = 0; // 对齐 Node：熔断后重置全局连续失败
                    let stopped_result = make_tool_loop_stopped_result(&tc.name, &reason);
                    // ── M2 台账：熔断事件（status=tripped）──
                    record_tool_call(&round_ctx, round, &tc.name, &normalized, &stopped_result, "tripped", 0, &args.delegated_from);
                    (stopped_result, true)
                }
                None => {
                    let rid = round_ctx.request_id.clone().unwrap_or_default();
                    // ── P1-2 工具防重放：同逻辑请求（request_id+round+tool）已有成功记录 → 复用 ──
                    let cached = replay_guard.and_then(|g| g.find_result(&rid, round, &tc.name));
                    let (r, dur_ms) = if let Some(cached) = cached {
                        tracing::info!(
                            "[工具防重放] {} round {} 命中台账，复用记录结果（不重复执行）",
                            tc.name,
                            round
                        );
                        (cached, 0)
                    } else {
                        // 真正执行
                        let t0 = std::time::Instant::now();
                        let r = match executor.execute(&tc.name, &normalized) {
                            Ok(r) => r,
                            Err(e) => json!({
                                "ok": false,
                                "tool": tc.name,
                                "error": e.to_string(),
                            })
                            .to_string(),
                        };
                        let dur_ms = t0.elapsed().as_millis() as i64;
                        // ── P1-2 同步落账（不等 flusher）：执行了就有账，响应丢失后重试可复用 ──
                        if let Some(g) = replay_guard {
                            let status = if is_tool_failure(&r) { "error" } else { "ok" };
                            g.record(&rid, round, &tc.name, &normalized, &r, status, dur_ms, &args.delegated_from);
                        }
                        (r, dur_ms)
                    };
                    state.record_outcome(&fingerprint, &r);
                    // ── M2 台账：工具执行结果（键含 attempt，防重试误伤；重放行同键被 IGNORE 去重）──
                    let status = if is_tool_failure(&r) { "error" } else { "ok" };
                    record_tool_call(&round_ctx, round, &tc.name, &normalized, &r, status, dur_ms, &args.delegated_from);
                    (r, false)
                }
            };
            if !stopped {
                // delivered 语义（对齐 Node）：send_message 等投递工具成功 → delivered=true
                if let Ok(parsed) = serde_json::from_str::<Value>(&result) {
                    if parsed.get("delivered").and_then(|v| v.as_bool()) == Some(true)
                        && parsed.get("message_sent").and_then(|v| v.as_bool()) == Some(true)
                    {
                        delivered = true;
                    }
                }
            }
            last_tool_result = Some(ToolResultInfo {
                name: tc.name.clone(),
                args: normalized.clone(),
                result: result.clone(),
            });
            if let Some(cb) = &on_tool_call {
                cb(&tc.name, &normalized, &result);
            }
            tracing::info!(
                "[工具结果] {}: {}",
                tc.name,
                result.chars().take(100).collect::<String>()
            );
            tool_results.push(ToolExecOutcome {
                id: tc.id.clone(),
                name: tc.name.clone(),
                result,
            });
            if stopped {
                tool_loop_stop_reason = Some(tc.name.clone());
            }
        }

        if ctx.is_aborted() {
            break;
        }

        // 注入 assistant 消息 + 工具结果（对齐 Node 1523-1602）
        if xml_round {
            // XML 工具调用：assistant 纯文本 + tool 结果作 user 消息
            if !round_result.content.trim().is_empty() {
                messages.push(ChatMessage::assistant(round_result.content.clone()));
            }
            let summary: Vec<String> = tool_results
                .iter()
                .map(|tr| format!("[Tool result] {}: {}", tr.name, truncate(&tr.result, 300)))
                .collect();
            if !summary.is_empty() {
                messages.push(ChatMessage::user(summary.join("\n")));
            }
        } else {
            let mut assistant_msg = ChatMessage::assistant("");
            assistant_msg.content = if round_result.content.trim().is_empty() {
                None
            } else {
                Some(round_result.content.clone())
            };
            if !round_result.reasoning_content.trim().is_empty() {
                assistant_msg.reasoning_content = Some(round_result.reasoning_content.clone());
            }
            assistant_msg.tool_calls = Some(
                effective_tool_calls
                    .iter()
                    .map(|tc| ToolCallPayload {
                        id: tc.id.clone(),
                        r#type: "function".into(),
                        function: ToolFunctionPayload {
                            name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                        },
                    })
                    .collect(),
            );
            messages.push(assistant_msg);
            for tr in &tool_results {
                messages.push(ChatMessage::tool(tr.id.clone(), tr.result.clone()));
            }
        }

        // 基础 nudge：有工具结果但还没给用户回复
        if args.must_reply && tool_loop_stop_reason.is_none() && !delivered {
            let continue_nudge = if args.local_reply {
                "Tool results have returned. Give the user your final reply now as plain text — in this local channel your message text reaches the user directly. If information is insufficient, explain what was found and the limitations; do not end silently."
            } else {
                "Tool results have returned. Continue completing the user request based on the available results. If the information is sufficient, call send_message now to deliver your final reply to the user. If a tool failed, explain the failure and available clues; do not end silently."
            };
            let _ = &uncertainty_used;
            messages.push(ChatMessage::user(if state.total_calls >= 18 && !uncertainty_used {
                uncertainty_used = true;
                format!(
                    "You have run {} tool calls this turn and still have not delivered a result to the user. Pause for one beat. In <think>, ask yourself honestly: am I converging, or pushing forward anyway? If stuck, tell the user what you have done and what you need. This is a one-time internal checkpoint; do not narrate it to the user.",
                    state.total_calls
                )
            } else {
                continue_nudge.to_string()
            }));
        }
        round += 1;
    }

    // ── M2：round_limit 事件（循环走满 max_rounds 上限退出 → llm_calls.finish_reason=round_limit）──
    if round >= limits.max_rounds {
        if let Some(last) = &last_round_ctx {
            if let Some(m) = &last.metrics {
                m.record(super::metrics::MetricEvent::RoundLimit {
                    request_id: last.request_id.clone().unwrap_or_default(),
                });
            }
        }
    }

    let aborted = ctx.is_aborted() || aborted;
    // 投递净化：剥离 <think>/<thinking> 块与 [RECALL:]/[SET_TASK:]/[CLEAR_TASK]/[UPDATE_PERSONA:]
    // 文本标记 + 松散前奏行（对齐 Node finalizeAssistantReply 的 sanitizeAssistantReplyForDelivery）。
    // 注意：不能剥 <invoke>（XML 回退已在上游处理），这里只处理协议标记。
    let content = crate::llm::markers::sanitize_assistant_reply_for_delivery(&all_content);
    Ok(LlmCallResult {
        content,
        tool_result: last_tool_result,
        aborted,
        delivered,
        total_calls: state.total_calls,
        intervened,
    })
}

/// M2 台账助手：写一条工具执行记录（best-effort，经 round_ctx.metrics 入队）。
/// `attempt` 固定 1：重试发生在 stream 层（round 内不可见），工具不会因重试重复执行；
/// 唯一键含 attempt 维度是为将来「round 级重试」预留，且重放不误伤。
#[allow(clippy::too_many_arguments)]
fn record_tool_call(
    round_ctx: &StreamContext,
    round: usize,
    tool_name: &str,
    args: &Value,
    result: &str,
    status: &str,
    duration_ms: i64,
    delegated_from: &str,
) {
    if let Some(m) = &round_ctx.metrics {
        m.record(super::metrics::MetricEvent::ToolCall {
            request_id: round_ctx.request_id.clone().unwrap_or_default(),
            round: round as i64,
            attempt: 1,
            tool_name: tool_name.to_string(),
            args_json: args.to_string(),
            result_json: result.to_string(),
            status: status.to_string(),
            duration_ms,
            delegated_from: delegated_from.to_string(),
        });
    }
}

/// 从正文移除 `<invoke ...>...</invoke>` 块（对齐 Node allContent.replace）
fn strip_invoke_blocks(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<invoke") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find("</invoke>") {
            Some(close) => rest = &after[close + "</invoke>".len()..],
            None => {
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;
    use crate::error::CoreError;
    use crate::llm::metrics::init_with;
    use crate::llm::caller::build_chat_completion_request;
    use crate::llm::types::ChatRole;
    use serde_json::json;

    // ── 纯逻辑测试 ──

    #[test]
    fn tool_failure_detection() {
        assert!(is_tool_failure(r#"{"ok":false,"error":"boom"}"#));
        assert!(is_tool_failure(r#"{"error":"boom"}"#));
        assert!(is_tool_failure("错误: 文件不存在"));
        assert!(is_tool_failure("执行失败"));
        assert!(!is_tool_failure(r#"{"ok":true,"data":1}"#));
        assert!(!is_tool_failure(r#"{"data":1}"#));
        assert!(!is_tool_failure("正常返回"));
        assert!(!is_tool_failure(""));
    }

    #[test]
    fn normalize_args_maps_aliases() {
        let v = normalize_args("web_search", &json!({"q": "rust", "limit": 3}));
        assert_eq!(v["query"], "rust");
        assert!(v.get("q").is_none());
        assert_eq!(v["limit"], 3);
        // 规范名已存在时不动别名
        let v2 = normalize_args("web_search", &json!({"q": "a", "query": "b"}));
        assert_eq!(v2["query"], "b");
        assert!(v2.get("q").is_some());
        // 无别名工具原样返回
        let v3 = normalize_args("get_time", &json!({"format": "iso"}));
        assert_eq!(v3["format"], "iso");
        // send_message
        let v4 = normalize_args("send_message", &json!({"to": "ID:1", "text": "hi"}));
        assert_eq!(v4["target_id"], "ID:1");
        assert_eq!(v4["content"], "hi");
    }

    #[test]
    fn loop_state_fuses_on_consecutive_failures() {
        let limits = ToolLoopLimits::default();
        let mut st = ToolLoopState::new();
        // 三个不同指纹各失败一次 → 连续失败=3，但同指纹计数各为 1（不触发 same-failure）
        for i in 0..3 {
            st.record_outcome(&format!("tool{i}:{{}}"), r#"{"ok":false}"#);
        }
        assert_eq!(st.total_calls, 3);
        assert_eq!(st.consecutive_failures, 3);
        // 普通工具 → 连续失败熔断
        let reason = st.get_stop_reason(&limits, "exec_command", "toolX:{}");
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("consecutive"));
        // send_message/express 豁免连续失败熔断（对齐 Node REPORT_CHANNEL_TOOLS）
        assert!(st
            .get_stop_reason(&limits, "send_message", "toolX:{}")
            .is_none());
        assert!(st.get_stop_reason(&limits, "express", "toolX:{}").is_none());
    }

    #[test]
    fn loop_state_fuses_on_same_failure() {
        let limits = ToolLoopLimits::default();
        let mut st = ToolLoopState::new();
        let f = "web_search:{\"q\":\"x\"}".to_string();
        // 两次相同失败 + 一次成功（重置连续失败）→ 仍由同指纹计数熔断
        st.record_outcome(&f, r#"{"ok":false}"#);
        st.record_outcome(&f, r#"{"ok":false}"#);
        st.record_outcome("other:{}", "ok");
        assert_eq!(st.consecutive_failures, 0);
        assert!(st.get_stop_reason(&limits, "web_search", &f).is_some());
        // 同指纹失败对汇报通道同样熔断（Node 不豁免 same-failure）
        assert!(st.get_stop_reason(&limits, "send_message", &f).is_some());
    }

    #[test]
    fn loop_state_fuses_on_stuck_loop() {
        let limits = ToolLoopLimits::default();
        let mut st = ToolLoopState::new();
        let f = "tool:{}".to_string();
        for _ in 0..8 {
            st.record_outcome(&f, "ok");
        }
        let reason = st.get_stop_reason(&limits, "tool", &f);
        assert!(reason.unwrap().contains("loop"));
        // 汇报通道豁免死循环熔断
        assert!(st.get_stop_reason(&limits, "send_message", &f).is_none());
    }

    #[test]
    fn loop_state_allows_diverse_actions() {
        let limits = ToolLoopLimits::default();
        let mut st = ToolLoopState::new();
        for i in 0..8 {
            st.record_outcome(&format!("tool{i}:{{}}"), "ok");
        }
        assert!(st.get_stop_reason(&limits, "toolX", "toolX:{}").is_none());
    }

    #[test]
    fn stopped_result_shape() {
        let r =
            make_tool_loop_stopped_result("exec_command", "too many consecutive tool failures (3)");
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["tool"], "exec_command");
        assert!(v["reason"].as_str().unwrap().contains("consecutive"));
    }

    #[test]
    fn strip_invoke_blocks_removes_xml_calls() {
        let content =
            "前文<invoke name=\"get_time\"><parameter name=\"format\">iso</parameter></invoke>后文";
        assert_eq!(strip_invoke_blocks(content), "前文后文");
    }

    // ── 循环测试（mock stream） ──

    /// demo 执行器：echo / get_time / fail 工具
    struct DemoExecutor;
    impl ToolExecutor for DemoExecutor {
        fn execute(&self, name: &str, args: &Value) -> Result<String> {
            match name {
                "echo" => Ok(json!({"ok": true, "echo": args["text"]}).to_string()),
                "get_time" => Ok(json!({"ok": true, "time": "2026-08-09T00:00:00Z"}).to_string()),
                "fail" => Ok(json!({"ok": false, "error": "boom"}).to_string()),
                _ => Err(CoreError::Tool(format!("未知工具: {name}"))),
            }
        }
    }

    fn mock_stream(script: Vec<StreamOnceResult>) -> Arc<StreamFn> {
        use std::sync::Mutex;
        let queue = Arc::new(Mutex::new(script));
        Arc::new(
            move |_client: &Client,
                  _cfg: &LlmConfig,
                  request: &ChatCompletionRequest,
                  ctx: &StreamContext,
                  _retry: Option<RetryCb>| {
                let queue = queue.clone();
                let request = request.clone();
                let metrics = ctx.metrics.clone();
                let request_id = ctx.request_id.clone();
                async move {
                    // 记录请求消息数供断言（模拟轮次推进依赖外部注入）
                    let _ = request;
                    // 模拟真实 caller：发 CallStarted / CallFinished（metrics=None 时 no-op，零侵入）
                    if let Some(m) = &metrics {
                        let rid = request_id.clone().unwrap_or_default();
                        m.record(crate::llm::metrics::MetricEvent::CallStarted {
                            request_id: rid.clone(),
                            provider: "deepseek".into(),
                            model: "deepseek-v4-pro".into(),
                            started_at: "2026-08-10T10:00:00+08:00".into(),
                            stage: String::new(),
                        });
                    }
                    let mut q = queue.lock().unwrap();
                    if q.is_empty() {
                        return Ok(StreamOnceResult::default());
                    }
                    let res = q.remove(0);
                    if let Some(m) = &metrics {
                        m.record(crate::llm::metrics::MetricEvent::CallFinished {
                            request_id: request_id.clone().unwrap_or_default(),
                            duration_ms: 100,
                            total_tokens: 10,
                            cached_tokens: 0,
                            usage_raw: "{}".into(),
                            aborted: false,
                        });
                    }
                    Ok(res)
                }
                .boxed()
            },
        )
    }

    fn test_ctx() -> StreamContext {
        StreamContext {
            aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            on_stream: None,
            idle_timeout: None,
            request_id: None,
            metrics: None,
            stage: String::new(),
        }
    }

    fn tool(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: format!("call_{}", name),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    fn cfg() -> LlmConfig {
        LlmConfig {
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            fast_model: String::new(),
            api_key: "test-key".into(),
            base_url: "https://api.deepseek.com".into(),
        }
    }

    #[tokio::test]
    async fn completes_simple_plain_text_turn() {
        // 第一轮直接给出正文，无工具调用
        let stream = mock_stream(vec![StreamOnceResult {
            content: "你好！".into(),
            ..Default::default()
        }]);
        let args = CallLlmArgs {
            system_prompt: "你是一个助手".into(),
            message: "你好".into(),
            ..Default::default()
        };
        let result = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.content, "你好！");
        assert!(!result.aborted);
        assert!(result.tool_result.is_none());
    }

    #[tokio::test]
    async fn runs_tool_then_final_reply() {
        // 第 1 轮：调用 get_time 工具
        // 第 2 轮：给出正文
        let stream = mock_stream(vec![
            StreamOnceResult {
                tool_calls: vec![tool("get_time", r#"{"format":"iso"}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                content: "现在是 2026-08-09T00:00:00Z".into(),
                ..Default::default()
            },
        ]);
        let args = CallLlmArgs {
            system_prompt: "你是一个助手".into(),
            message: "现在几点".into(),
            tools: vec![ToolSchema::new("get_time", "获取时间")],
            ..Default::default()
        };
        let result = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.total_calls, 1);
        assert_eq!(result.content, "现在是 2026-08-09T00:00:00Z");
        let tr = result.tool_result.unwrap();
        assert_eq!(tr.name, "get_time");
        assert_eq!(tr.args["format"], "iso");
        assert_eq!(tr.result, r#"{"ok":true,"time":"2026-08-09T00:00:00Z"}"#);
    }

    #[tokio::test]
    async fn intervention_pause_stops_tool_dispatch() {
        // Q6：启用介入通道并暂停 → 工具循环不再派发任何工具，intervened=true
        let gate = Arc::new(InterventionGate::new(true));
        gate.request_pause("人工介入测试").unwrap();
        let executed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct CountingExecutor(Arc<std::sync::atomic::AtomicUsize>);
        impl ToolExecutor for CountingExecutor {
            fn execute(&self, _name: &str, _args: &Value) -> Result<String> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(json!({"ok": true}).to_string())
            }
        }
        let stream = mock_stream(vec![StreamOnceResult {
            tool_calls: vec![tool("echo", r#"{"text":"a"}"#)],
            ..Default::default()
        }]);
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "echo".into(),
            tools: vec![ToolSchema::new("echo", "回显")],
            intervention: Some(gate.clone()),
            ..Default::default()
        };
        let result = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &CountingExecutor(executed.clone()),
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        assert!(result.intervened);
        assert_eq!(result.total_calls, 0);
        assert_eq!(executed.load(std::sync::atomic::Ordering::SeqCst), 0);
        // 恢复后可继续
        gate.resume();
        assert_eq!(gate.check(), InterventionStatus::Open);
    }

    #[tokio::test]
    async fn tool_arguments_normalized_before_execution() {
        // 模型用别名 q=… 调 web_search，执行器应收到 query
        struct SearchExecutor;
        impl ToolExecutor for SearchExecutor {
            fn execute(&self, name: &str, args: &Value) -> Result<String> {
                assert_eq!(name, "web_search");
                assert_eq!(args["query"], "rust 教程");
                Ok(json!({"ok": true}).to_string())
            }
        }
        let stream = mock_stream(vec![
            StreamOnceResult {
                tool_calls: vec![tool("web_search", r#"{"q":"rust 教程"}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                content: "查到了".into(),
                ..Default::default()
            },
        ]);
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "搜索".into(),
            ..Default::default()
        };
        let result = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &SearchExecutor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        assert!(result.content.contains("查到了"));
    }

    #[tokio::test]
    async fn xml_tool_calls_parsed_from_content() {
        // MiniMax 风格：无 JSON tool_calls，正文内嵌 XML 调用
        let stream = mock_stream(vec![StreamOnceResult {
            content: "<invoke name=\"echo\"><parameter name=\"text\">hi</parameter></invoke>"
                .into(),
            ..Default::default()
        }]);
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "echo".into(),
            tools: vec![ToolSchema::new("echo", "回显")],
            ..Default::default()
        };
        let result = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.total_calls, 1);
        assert_eq!(result.tool_result.unwrap().name, "echo");
        // XML 块已从 content 移除
        assert!(!result.content.contains("<invoke"));
    }

    #[tokio::test]
    async fn loop_stops_when_model_stops_calling_tools() {
        // 多轮工具调用后停止
        let stream = mock_stream(vec![
            StreamOnceResult {
                tool_calls: vec![tool("echo", r#"{"text":"a"}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                tool_calls: vec![tool("echo", r#"{"text":"b"}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                content: "完成".into(),
                ..Default::default()
            },
        ]);
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "多轮".into(),
            ..Default::default()
        };
        let result = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.total_calls, 2);
        assert_eq!(result.content, "完成");
    }

    #[tokio::test]
    async fn abort_stops_loop() {
        let stream = mock_stream(vec![
            StreamOnceResult {
                tool_calls: vec![tool("echo", r#"{"text":"a"}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                tool_calls: vec![tool("echo", r#"{"text":"b"}"#)],
                ..Default::default()
            },
        ]);
        let ctx = StreamContext {
            aborted: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ..test_ctx()
        };
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "中止".into(),
            ..Default::default()
        };
        let result = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &ctx,
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        assert!(result.aborted);
        assert_eq!(result.total_calls, 0);
    }

    #[tokio::test]
    async fn assistant_messages_carry_tool_calls_for_replay() {
        // 验证工具调用后的下一轮请求包含 assistant(tool_calls) + tool 消息
        let request_seen = Arc::new(std::sync::Mutex::new(None::<ChatCompletionRequest>));
        let seen = request_seen.clone();
        let queue = Arc::new(std::sync::Mutex::new(vec![
            StreamOnceResult {
                tool_calls: vec![tool("get_time", r#"{}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                content: "时间如上".into(),
                ..Default::default()
            },
        ]));
        let stream: Arc<StreamFn> = Arc::new(
            move |_c: &Client,
                  _cfg: &LlmConfig,
                  request: &ChatCompletionRequest,
                  _ctx: &StreamContext,
                  _r: Option<RetryCb>| {
                let seen = seen.clone();
                let queue = queue.clone();
                let request = request.clone();
                async move {
                    *seen.lock().unwrap() = Some(request.clone());
                    let mut q = queue.lock().unwrap();
                    Ok(q.remove(0))
                }
                .boxed()
            },
        );
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "时间".into(),
            ..Default::default()
        };
        let _ = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        let seen_req = request_seen.lock().unwrap().take().unwrap();
        // 第二轮请求（工具结果注入后）：messages 中应含 assistant tool_calls 与 tool 消息
        let roles: Vec<ChatRole> = seen_req.messages.iter().map(|m| m.role).collect();
        assert!(roles.contains(&ChatRole::Assistant));
        assert!(roles.contains(&ChatRole::Tool));
        let assistant = seen_req
            .messages
            .iter()
            .find(|m| m.role == ChatRole::Assistant)
            .unwrap();
        assert!(assistant.tool_calls.is_some());
        assert_eq!(
            assistant.tool_calls.as_ref().unwrap()[0].function.name,
            "get_time"
        );
    }

    #[tokio::test]
    async fn build_request_used_in_loop() {
        // 确保循环使用 build_chat_completion_request（工具 schema 传入）
        let request_seen = Arc::new(std::sync::Mutex::new(None::<ChatCompletionRequest>));
        let seen = request_seen.clone();
        let stream: Arc<StreamFn> = Arc::new(
            move |_c: &Client,
                  _cfg: &LlmConfig,
                  request: &ChatCompletionRequest,
                  _ctx: &StreamContext,
                  _r: Option<RetryCb>| {
                let seen = seen.clone();
                let request = request.clone();
                async move {
                    *seen.lock().unwrap() = Some(request.clone());
                    Ok(StreamOnceResult {
                        content: "ok".into(),
                        ..Default::default()
                    })
                }
                .boxed()
            },
        );
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "hi".into(),
            tools: vec![ToolSchema::new("echo", "回显").required("text", json!({"type":"string"}))],
            ..Default::default()
        };
        let _ = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        let req = request_seen.lock().unwrap().take().unwrap();
        let tools = req.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "echo");
        let _ = build_chat_completion_request("x", "y", vec![], None, None, None, true, &[]);
    }

    // ── M1 装配回归：每轮 request_id 印章透传（零侵入的另一半证据） ──

    #[tokio::test]
    async fn each_round_stamps_request_id_on_context() {
        // 捕获 stream 收到的 ctx：两轮应有两个不同 request_id
        let ids_seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = ids_seen.clone();
        let queue = Arc::new(std::sync::Mutex::new(vec![
            StreamOnceResult {
                tool_calls: vec![tool("echo", r#"{"text":"a"}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                content: "done".into(),
                ..Default::default()
            },
        ]));
        let stream: Arc<StreamFn> = Arc::new(
            move |_c: &Client,
                  _cfg: &LlmConfig,
                  _request: &ChatCompletionRequest,
                  ctx: &StreamContext,
                  _r: Option<RetryCb>| {
                let seen = seen.clone();
                let queue = queue.clone();
                let request = _request.clone();
                let request_id = ctx.request_id.clone().unwrap_or_default();
                async move {
                    seen.lock().unwrap().push(request_id);
                    let _ = request;
                    let mut q = queue.lock().unwrap();
                    Ok(q.remove(0))
                }
                .boxed()
            },
        );
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "hi".into(),
            tools: vec![ToolSchema::new("echo", "回显")],
            ..Default::default()
        };
        let _ = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        let ids = ids_seen.lock().unwrap();
        assert_eq!(ids.len(), 2, "两轮各应拿到一个 request_id");
        assert!(ids.iter().all(|s| s.starts_with("llm-")), "ids: {ids:?}");
        assert_ne!(ids[0], ids[1], "每轮 request_id 必须不同");
    }

    // ── M2：round_limit 事件 + delegated_from 台账透传（集成） ──

    #[tokio::test]
    async fn loop_exhausts_max_rounds_emits_round_limit() {
        // 循环走满 max_rounds=2 → 最后一轮 request_id 的 llm_calls.finish_reason = round_limit
        let dir = tempfile::tempdir().unwrap();
        let db = open_database(dir.path().join("t.db")).unwrap();
        let (col, flusher) = init_with(db.clone(), std::time::Duration::from_secs(60_000), 10_000);
        let stream = mock_stream(vec![
            StreamOnceResult {
                tool_calls: vec![tool("echo", r#"{"text":"a"}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                tool_calls: vec![tool("echo", r#"{"text":"b"}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                tool_calls: vec![tool("echo", r#"{"text":"c"}"#)],
                ..Default::default()
            },
        ]);
        let ctx = StreamContext {
            metrics: Some(col),
            ..test_ctx()
        };
        let limits = ToolLoopLimits {
            max_rounds: 2,
            ..Default::default()
        };
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "连续调用".into(),
            ..Default::default()
        };
        let _ = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &ctx,
            None,
            &limits,
            None,
        )
        .await
        .unwrap();
        flusher.flush_now().await;

        // 两轮工具执行都入台账
        let tool_n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM llm_tool_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tool_n, 2, "两轮工具调用各一条台账");
        // 第一轮正常 done
        let reason0: String = db
            .conn()
            .query_row(
                "SELECT c.finish_reason FROM llm_calls c
                 JOIN llm_tool_calls t ON c.request_id = t.request_id
                 WHERE t.round = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason0, "done");
        // 最后一轮（round=1）的 llm_calls 终态 = round_limit
        let reason: String = db
            .conn()
            .query_row(
                "SELECT c.finish_reason FROM llm_calls c
                 JOIN llm_tool_calls t ON c.request_id = t.request_id
                 WHERE t.round = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason, "round_limit", "走满上限必须标 round_limit");
    }

    /// P1-2 故障注入验收：模拟「provider 完成但响应丢失」→ 同逻辑请求重试 →
    /// 副作用工具不得重复执行（台账复用记录结果）。
    #[tokio::test]
    async fn tool_replay_guard_prevents_double_execution_on_lost_response() {
        use crate::db::open_database;
        use crate::llm::replay::DbToolReplayGuard;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingExecutor {
            calls: Arc<AtomicUsize>,
        }
        impl ToolExecutor for CountingExecutor {
            fn execute(&self, name: &str, _args: &Value) -> Result<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "ok": true, "tool": name }).to_string())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let db = open_database(dir.path().join("t.db")).unwrap();
        let guard = DbToolReplayGuard::new(db);
        let calls = Arc::new(AtomicUsize::new(0));
        let executor = CountingExecutor { calls: calls.clone() };
        let args = CallLlmArgs {
            system_prompt: "你是助手".into(),
            message: "发条消息".into(),
            tools: vec![ToolSchema::new("send_message", "发消息")],
            round_request_id_seed: Some("fault_inject_turn".into()),
            ..Default::default()
        };
        let script = || vec![
            StreamOnceResult {
                tool_calls: vec![tool("send_message", r#"{"to":"u","text":"hi"}"#)],
                ..Default::default()
            },
            StreamOnceResult {
                content: "已发送".into(),
                ..Default::default()
            },
        ];

        // 第 1 次调用：工具真实执行一次，结果同步落账；随后模拟响应在传输中丢失
        // （上层拿不到结果，以同一逻辑请求重试）。
        let stream1 = mock_stream(script());
        let r1 = call_llm(
            &Client::new(),
            &cfg(),
            stream1.as_ref(),
            &executor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            Some(&guard),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "第 1 次调用应真实执行一次");

        // 第 2 次调用（重试）：seed 相同 → round request_id 相同 → 防重放守卫命中台账，
        // 工具不得再次执行，直接复用记录结果。
        let stream2 = mock_stream(script());
        let r2 = call_llm(
            &Client::new(),
            &cfg(),
            stream2.as_ref(),
            &executor,
            &args,
            &test_ctx(),
            None,
            &ToolLoopLimits::default(),
            Some(&guard),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "重试不得重复执行副作用工具");
        assert_eq!(
            r1.tool_result.as_ref().unwrap().result,
            r2.tool_result.as_ref().unwrap().result,
            "重试应复用台账记录结果"
        );
        assert_eq!(r2.content, "已发送");
    }

    #[tokio::test]
    async fn delegated_from_propagates_to_tool_ledger() {
        // 协作信任账本：delegated_from 由上层注入，台账原样落库
        let dir = tempfile::tempdir().unwrap();
        let db = open_database(dir.path().join("t.db")).unwrap();
        let (col, flusher) = init_with(db.clone(), std::time::Duration::from_secs(60_000), 10_000);
        let stream = mock_stream(vec![
            StreamOnceResult {
                tool_calls: vec![tool("get_time", "{}")],
                ..Default::default()
            },
            StreamOnceResult {
                content: "时间如上".into(),
                ..Default::default()
            },
        ]);
        let ctx = StreamContext {
            metrics: Some(col),
            ..test_ctx()
        };
        let args = CallLlmArgs {
            system_prompt: "助手".into(),
            message: "几点了".into(),
            delegated_from: "collaborator_alpha".into(),
            ..Default::default()
        };
        let _ = call_llm(
            &Client::new(),
            &cfg(),
            stream.as_ref(),
            &DemoExecutor,
            &args,
            &ctx,
            None,
            &ToolLoopLimits::default(),
            None,
        )
        .await
        .unwrap();
        flusher.flush_now().await;
        let df: String = db
            .conn()
            .query_row("SELECT delegated_from FROM llm_tool_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(df, "collaborator_alpha", "台账必须携带委托来源");
    }
}
