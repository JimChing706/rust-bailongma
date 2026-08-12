//! LLM 调用器 —— OpenAI 兼容 `/chat/completions` 流式调用。
//!
//! 对齐 Node 版 `src/llm.js` 的 `buildChatCompletionRequestParams` + `streamOnce`：
//! - 按 provider/model 适配请求参数（采样省略 / max_completion_tokens / thinking / tool_stream）
//! - SSE 流式解析：usage、思考流（reasoning_content）、文本流（含 `<think>` 标签）、工具调用增量拼装
//! - 空闲超时（45s 无增量判卡死，对齐 STREAM_IDLE_TIMEOUT_MS）与外部中止
//! - M1 观测埋点（P0）：CallStarted / TTFT / CallFinished / CallFailed（5 个错误分支），
//!   全部经 `ctx.metrics`（mpsc send，<1ms），未挂采集器时零额外开销

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};

use crate::error::{CoreError, Result};
use crate::prelude::LogLevel;

use super::providers::{
    should_omit_sampling_for_provider_model, should_send_thinking_disabled_for_provider_model,
    should_use_max_completion_tokens_for_provider_model, ZHIPU,
};
use super::sse::{ChatChunk, SseEvent, SseParser};
use super::types::{
    ChatCompletionRequest, ChatMessage, StreamEvent, StreamMode, StreamOnceResult, ThinkEvent,
    ThinkStreamState, ToolCall,
};

/// 空闲超时：与 Node 版 STREAM_IDLE_TIMEOUT_MS 一致（45s）
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// LLM 调用所需的运行期配置（来自 Config + provider 注册表）
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub provider: String,
    pub model: String,
    pub fast_model: String,
    pub api_key: String,
    pub base_url: String,
}

impl LlmConfig {
    /// 从配置构造；未激活（缺 provider/api_key）返回 `LlmNotConfigured`（对齐 getClient）
    pub fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        let provider = cfg.provider.trim().to_string();
        if provider.is_empty() {
            return Err(CoreError::LlmNotConfigured("未指定 provider".into()));
        }
        let api_key = cfg.api_key.clone().unwrap_or_default();
        if api_key.is_empty() {
            return Err(CoreError::LlmNotConfigured(
                "LLM 尚未激活，请先通过激活页填入 API Key".into(),
            ));
        }
        let base_url = cfg.base_url.clone().unwrap_or_else(|| {
            super::providers::get_provider_config(&provider)
                .base_url
                .to_string()
        });
        let model = super::providers::normalize_model(cfg.model.as_deref(), &provider);
        let fast_model = cfg
            .fast_model
            .as_deref()
            .filter(|m| !m.trim().is_empty())
            .map(|m| super::providers::normalize_model(Some(m), &provider))
            .unwrap_or_else(|| model.clone());
        Ok(Self {
            provider,
            model,
            fast_model,
            api_key,
            base_url,
        })
    }

    /// 聊天补全端点 URL
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// P3-2 模型路由：后台/低强度场景（tick、wakeup、startup）走 fast_model（若配置），
    /// 其余场景（交互、工具循环等）走主模型。fast_model 未配置时回退主模型。
    pub fn route_model(&self, scenario: &str) -> String {
        match scenario {
            "tick" | "wakeup" | "startup" => {
                if self.fast_model.is_empty() {
                    self.model.clone()
                } else {
                    self.fast_model.clone()
                }
            }
            _ => self.model.clone(),
        }
    }
}

/// 流式调用上下文（回调 + 中止标志 + 空闲超时 + M1 观测句柄）
#[derive(Clone)]
pub struct StreamContext {
    /// 外部中止标志（watchdog / 抢占）
    pub aborted: Arc<AtomicBool>,
    /// 流事件回调（UI 实时渲染）
    pub on_stream: Option<Arc<dyn Fn(StreamEvent) + Send + Sync>>,
    /// 空闲超时；None 表示禁用（测试用）
    pub idle_timeout: Option<Duration>,
    /// M1 观测：本次逻辑请求 ID（重试/降级共享；None 时流内自生成匿名 ID）
    pub request_id: Option<String>,
    /// M1 观测：指标采集句柄（None = 关闭观测，流路径零额外开销）
    pub metrics: Option<super::metrics::MetricsCollector>,
    /// M3：调用阶段（run_turn / tool_loop / wakeup / startup；空串 = 未标注）
    pub stage: String,
}

impl Default for StreamContext {
    fn default() -> Self {
        Self {
            aborted: Arc::new(AtomicBool::new(false)),
            on_stream: None,
            idle_timeout: Some(STREAM_IDLE_TIMEOUT),
            request_id: None,
            metrics: None,
            stage: String::new(),
        }
    }
}

impl StreamContext {
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Relaxed)
    }

    fn emit(&self, event: StreamEvent) {
        if let Some(cb) = &self.on_stream {
            cb(event);
        }
    }
}

/// 归一化 temperature（对齐 normalizeTemperatureForProvider）
fn normalize_temperature(provider: &str, model: &str, temperature: Option<f32>) -> Option<f32> {
    let t = temperature?;
    if should_omit_sampling_for_provider_model(provider, model) {
        return None;
    }
    if provider != ZHIPU {
        return Some(t);
    }
    // zhipu：clamp(0,1)，保留两位小数（对齐 Number(temperature.toFixed(2))）
    Some(((t.clamp(0.0, 1.0) * 100.0).round()) / 100.0)
}

/// 构建请求参数（对齐 Node buildChatCompletionRequestParams 75-121 行）
#[allow(clippy::too_many_arguments)] // 与 Node 参数一一对应，拆结构体反而难对齐
pub fn build_chat_completion_request(
    provider: &str,
    model: &str,
    messages: Vec<ChatMessage>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_tokens: Option<u32>,
    thinking: bool,
    tools: &[Value],
) -> ChatCompletionRequest {
    let provider_temperature = normalize_temperature(provider, model, temperature);

    let mut req = ChatCompletionRequest {
        model: model.to_string(),
        messages,
        stream: true,
        temperature: provider_temperature,
        ..Default::default()
    };

    // stream_options: zhipu 不支持
    if provider != ZHIPU {
        req.stream_options = Some(json!({ "include_usage": true }));
    }

    // top_p：>0 且非 zhipu 且非采样省略模型
    if let Some(tp) = top_p {
        if tp > 0.0
            && provider != ZHIPU
            && !should_omit_sampling_for_provider_model(provider, model)
        {
            req.top_p = Some(tp);
        }
    }

    // thinking：deepseek 用 reasoning_effort + thinking 开关；其他 provider 仅在关闭时发 disabled
    if provider == crate::llm::providers::DEEPSEEK {
        let enabled = thinking && model != "deepseek-chat";
        if enabled {
            req.reasoning_effort = Some("high".into());
            req.thinking = Some(json!({ "type": "enabled" }));
        } else {
            req.thinking = Some(json!({ "type": "disabled" }));
        }
    } else if !thinking && should_send_thinking_disabled_for_provider_model(provider, model) {
        req.thinking = Some(json!({ "type": "disabled" }));
    }

    // max_tokens / max_completion_tokens
    if let Some(mt) = max_tokens {
        if should_use_max_completion_tokens_for_provider_model(provider, model) {
            req.max_completion_tokens = Some(mt);
        } else {
            req.max_tokens = Some(mt);
        }
    }

    // tools
    if !tools.is_empty() {
        req.tools = Some(tools.to_vec());
        req.tool_choice = Some("auto".into());
        if provider == ZHIPU {
            req.tool_stream = Some(true);
        }
    }

    req
}

/// 单次流式调用结果
/// 返回 Ok(result)：正常完成或外部中止（aborted=true 且携带已累积内容）
/// 返回 Err：HTTP 错误（LlmHttp，含状态码）、网络/空闲超时（瞬时）、协议解析失败
/// P1-2 幂等键：由逻辑请求 ID 派生，重试共享。
/// `stream_once_with_retry` 内所有 attempt 复用同一 `ctx.request_id`（M1 打底），
/// 因此同一逻辑请求的重试携带同一 `Idempotency-Key` —— provider 完成但响应丢失时，
/// 重试不会在 provider 侧产生第二次执行/计费。
pub fn idempotency_key(request_id: &str) -> String {
    format!("blm-{request_id}")
}

pub async fn stream_once(
    client: &Client,
    cfg: &LlmConfig,
    request: &ChatCompletionRequest,
    ctx: &StreamContext,
) -> Result<StreamOnceResult> {
    if ctx.is_aborted() {
        return Ok(StreamOnceResult {
            aborted: true,
            ..Default::default()
        });
    }

    // ── M1 埋点：请求开始（request_id 重试共享；t0 用于 TTFT/duration）──
    let started_at = std::time::Instant::now();
    let request_id =
        ctx.request_id.clone().unwrap_or_else(super::metrics::new_request_id);
    let mut first_chunk_ms: Option<i64> = None;
    if let Some(m) = &ctx.metrics {
        m.record(super::metrics::MetricEvent::CallStarted {
            request_id: request_id.clone(),
            provider: cfg.provider.clone(),
            model: cfg.model.clone(),
            started_at: chrono::Local::now().to_rfc3339(),
            stage: ctx.stage.clone(),
        });
    }

    // 建连也在空闲看门狗内（对齐 Node：idle timer 在 fetch 前就 arm，45s 无数据即 abort）。
    // 防止 provider 建连挂死（TCP 黑洞 / DNS 慢）时 turn 无限卡住。
    let idle_timeout = ctx.idle_timeout.unwrap_or(STREAM_IDLE_TIMEOUT);
    let resp = match tokio::time::timeout(idle_timeout, async {
        let mut builder = client
            .post(cfg.chat_completions_url())
            .bearer_auth(&cfg.api_key);
        // ── P1-2 幂等键：同一逻辑请求（重试共享 request_id）携带同一 Idempotency-Key ──
        if ctx.request_id.is_some() {
            if let Ok(hv) = reqwest::header::HeaderValue::from_str(&idempotency_key(&request_id)) {
                builder = builder.header("Idempotency-Key", hv);
            }
        }
        builder.json(request).send().await
    })
    .await
    {
        Err(_) => {
            // 建连超时（无响应头，空闲看门狗）
            record_failure(ctx, &request_id, &started_at, "connect", "timeout", None, false, true);
            return Err(CoreError::LlmStream {
                message: format!(
                    "connect timeout after {}s (no response headers, idle watchdog)",
                    idle_timeout.as_secs()
                ),
                had_content: false,
            });
        }
        Ok(Err(e)) => {
            // 网络层错误（DNS / TCP 拒绝等，可重试）
            record_failure(ctx, &request_id, &started_at, "connect", "network", None, false, true);
            return Err(CoreError::Llm(format!("连接 {cfg} 失败: {e}")));
        }
        Ok(Ok(resp)) => resp,
    };
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let message = if body.is_empty() {
            format!("HTTP {status}")
        } else {
            format!("HTTP {status}: {}", truncate(&body, 300))
        };
        let retryable = (500..600).contains(&status) || status == 408;
        record_failure(ctx, &request_id, &started_at, "http", "http", Some(status), false, retryable);
        return Err(CoreError::LlmHttp { status, message });
    }

    let mut parser = SseParser::new();
    let mut result = StreamOnceResult::default();
    let mut tool_calls_map: std::collections::BTreeMap<usize, ToolCall> = Default::default();
    let mut think_state = ThinkStreamState::new();
    let mut text_stream_started = false;
    let mut idle_fired = false;

    let mut stream = resp.bytes_stream();

    loop {
        // 外部中止：立即停止（对齐 Node `if (signal?.aborted) break`）
        if ctx.is_aborted() {
            result.aborted = true;
            break;
        }

        tokio::select! {
            chunk = stream.next() => {
                let Some(chunk) = chunk else {
                    // 流结束
                    break;
                };
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        // 读取流失败：先结束进行中的文本流，再上抛（对齐 Node 319 行）
                        end_text_stream(&mut text_stream_started, ctx);
                        if ctx.is_aborted() {
                            // reqwest 内部因外部 abort 报错视为中止而非错误
                            return Err(CoreError::LlmAborted);
                        }
                        let had = has_content(&result, &tool_calls_map);
                        record_failure(ctx, &request_id, &started_at, "stream", "read_failed", None, had, !had);
                        return Err(CoreError::LlmStream {
                            message: format!("读取流失败: {e}"),
                            had_content: had,
                        });
                    }
                };
                for ev in parser.push(&bytes) {
                    match ev {
                        SseEvent::Done => {
                            return finish_result(
                                result,
                                cfg,
                                tool_calls_map,
                                &mut text_stream_started,
                                ctx,
                                started_at,
                                first_chunk_ms,
                                &request_id,
                            );
                        }
                        SseEvent::Data(payload) => {
                            if let Err(e) = handle_data(
                                &payload,
                                &mut result,
                                &mut tool_calls_map,
                                &mut think_state,
                                &mut text_stream_started,
                                ctx,
                            ) {
                                end_text_stream(&mut text_stream_started, ctx);
                                let had = has_content(&result, &tool_calls_map);
                                record_failure(ctx, &request_id, &started_at, "parse", "protocol", None, had, !had);
                                return Err(e);
                            }
                            // ── M1 埋点：首个内容 chunk（TTFT；reasoning/正文/工具任一先到即算）──
                            if first_chunk_ms.is_none() && has_content(&result, &tool_calls_map) {
                                let ttft = started_at.elapsed().as_millis() as i64;
                                first_chunk_ms = Some(ttft);
                                if let Some(m) = &ctx.metrics {
                                    m.record(super::metrics::MetricEvent::Ttft {
                                        request_id: request_id.clone(),
                                        ttft_ms: ttft,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep(idle_timeout) => {
                if ctx.is_aborted() {
                    result.aborted = true;
                    break;
                }
                idle_fired = true;
                break;
            }
        }
    }

    if idle_fired && !ctx.is_aborted() {
        // 空闲超时：先结束进行中的文本流，再上抛瞬时错误（带 had_content），由 retry 层决定是否重试
        // 对齐 Node llm.js:300-303
        end_text_stream(&mut text_stream_started, ctx);
        let had = has_content(&result, &tool_calls_map);
        record_failure(ctx, &request_id, &started_at, "stream", "idle_timeout", None, had, !had);
        return Err(CoreError::LlmStream {
            message: format!("stream idle timeout after {}s", idle_timeout.as_secs()),
            had_content: had,
        });
    }

    // 正常收尾：冲刷剩余缓冲
    for ev in parser.finish() {
        if let SseEvent::Data(payload) = ev {
            if let Err(e) = handle_data(
                &payload,
                &mut result,
                &mut tool_calls_map,
                &mut think_state,
                &mut text_stream_started,
                ctx,
            ) {
                end_text_stream(&mut text_stream_started, ctx);
                let had = has_content(&result, &tool_calls_map);
                record_failure(ctx, &request_id, &started_at, "parse", "protocol", None, had, !had);
                return Err(e);
            }
            // ── M1 埋点：首个内容 chunk（is_none() 守卫保证只记一次）──
            if first_chunk_ms.is_none() && has_content(&result, &tool_calls_map) {
                let ttft = started_at.elapsed().as_millis() as i64;
                first_chunk_ms = Some(ttft);
                if let Some(m) = &ctx.metrics {
                    m.record(super::metrics::MetricEvent::Ttft {
                        request_id: request_id.clone(),
                        ttft_ms: ttft,
                    });
                }
            }
        }
    }
    finish_result(
        result,
        cfg,
        tool_calls_map,
        &mut text_stream_started,
        ctx,
        started_at,
        first_chunk_ms,
        &request_id,
    )
}

/// 收尾：冲刷 think 状态机、结束文本流（若仍在进行则补发 End），
/// 并把拼装好的工具调用（按 index 排序）写入结果。
/// 对齐 Node llm.js:325-326 的收尾必发 end（UI 依赖 End 结束光标/动画）。
#[allow(clippy::too_many_arguments)]
fn finish_result(
    mut result: StreamOnceResult,
    cfg: &LlmConfig,
    tool_calls_map: std::collections::BTreeMap<usize, ToolCall>,
    text_stream_started: &mut bool,
    ctx: &StreamContext,
    started_at: std::time::Instant,
    first_chunk_ms: Option<i64>,
    request_id: &str,
) -> Result<StreamOnceResult> {
    let _ = first_chunk_ms; // M1 预留：TTFT 已随事件上报；此处保留参数供终态核对
    // BTreeMap 按 index 升序迭代，保证工具调用顺序与流中一致
    result.tool_calls = tool_calls_map.into_values().collect();
    // 纯文本流（无工具调用、无 </think> 切换）走到 [DONE]/EOF 时补发 End
    end_text_stream(text_stream_started, ctx);
    if result.usage.total_tokens > 0 {
        tracing::info!(
            "[配额] 本轮 tokens: {} (cache hit {} / miss {})",
            result.usage.total_tokens,
            result.usage.prompt_cache_hit_tokens,
            result.usage.prompt_cache_miss_tokens
        );
    }
    // ── M1 埋点：请求结束（含外部中止；usage 归一化 + 原始字段兜底）──
    if let Some(m) = &ctx.metrics {
        m.record(super::metrics::MetricEvent::CallFinished {
            request_id: request_id.to_string(),
            duration_ms: started_at.elapsed().as_millis() as i64,
            total_tokens: result.usage.total_tokens,
            cached_tokens: super::metrics::normalize_cached_tokens(&cfg.provider, &result.usage),
            usage_raw: serde_json::json!({
                "total_tokens": result.usage.total_tokens,
                "prompt_cache_hit_tokens": result.usage.prompt_cache_hit_tokens,
                "prompt_cache_miss_tokens": result.usage.prompt_cache_miss_tokens,
            })
            .to_string(),
            aborted: result.aborted,
        });
    }
    result.content = result.content.trim_end().to_string();
    Ok(result)
}

/// 是否已流出内容（供错误携带 had_content，对齐 Node err.hadContent）
fn has_content(
    result: &StreamOnceResult,
    tool_calls_map: &std::collections::BTreeMap<usize, ToolCall>,
) -> bool {
    !result.content.is_empty() || !result.reasoning_content.is_empty() || !tool_calls_map.is_empty()
}

/// M1 埋点助手：错误分支统一记账（阶段/类别/状态码/已出内容/可重试性）。
/// 外部中止（LlmAborted）不记——它是主动停，不是错误。
#[allow(clippy::too_many_arguments)]
fn record_failure(
    ctx: &StreamContext,
    request_id: &str,
    started_at: &std::time::Instant,
    stage: &str,
    class: &str,
    http_status: Option<u16>,
    had_content: bool,
    retryable: bool,
) {
    if let Some(m) = &ctx.metrics {
        m.record(super::metrics::MetricEvent::CallFailed {
            request_id: request_id.to_string(),
            duration_ms: started_at.elapsed().as_millis() as i64,
            error_stage: stage.to_string(),
            error_class: class.to_string(),
            http_status,
            had_content,
            retryable,
        });
    }
}

/// 处理一个 SSE data 负载（一个 chunk JSON）
fn handle_data(
    payload: &str,
    result: &mut StreamOnceResult,
    tool_calls_map: &mut std::collections::BTreeMap<usize, ToolCall>,
    think_state: &mut ThinkStreamState,
    text_stream_started: &mut bool,
    ctx: &StreamContext,
) -> Result<()> {
    let chunk: ChatChunk = match serde_json::from_str(payload) {
        Ok(c) => c,
        Err(e) => {
            // 空 payload / 非 JSON 事件忽略（部分 provider 会发注释性事件）
            tracing::debug!("[sse] 忽略无法解析的 data 负载: {e}");
            return Ok(());
        }
    };

    // usage（末帧）
    if let Some(u) = chunk.usage {
        result.usage.total_tokens = u.total_tokens;
        result.usage.prompt_cache_hit_tokens = u.prompt_cache_hit_tokens;
        result.usage.prompt_cache_miss_tokens = u.prompt_cache_miss_tokens;
    }

    let Some(choice) = chunk.choices.first() else {
        return Ok(());
    };
    let Some(delta) = &choice.delta else {
        return Ok(());
    };

    // 1. 工具调用增量（对齐 Node 207-234 行）
    if let Some(tcs) = &delta.tool_calls {
        end_text_stream(text_stream_started, ctx);
        for tc in tcs {
            let idx = tc.index.unwrap_or(0);
            let entry = tool_calls_map.entry(idx).or_insert_with(|| ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            if let Some(id) = &tc.id {
                entry.id = id.clone();
            }
            if let Some(name) = tc.function.as_ref().and_then(|f| f.name.as_ref()) {
                let was_empty = entry.name.is_empty();
                entry.name.push_str(name);
                // 第一次拿到完整 name → 通知上层（对齐 tool_preparing）
                if was_empty && !entry.name.is_empty() {
                    ctx.emit(StreamEvent::ToolPreparing {
                        name: entry.name.clone(),
                    });
                }
            }
            if let Some(args) = tc.function.as_ref().and_then(|f| f.arguments.as_ref()) {
                entry.arguments.push_str(args);
            }
        }
        return Ok(());
    }

    // 2. 思考内容（reasoning_content 独立字段，不进 content）
    if let Some(reasoning) = &delta.reasoning_content {
        if !reasoning.is_empty() {
            result.reasoning_content.push_str(reasoning);
            start_stream(text_stream_started, StreamMode::Think, ctx);
            ctx.emit(StreamEvent::Chunk {
                text: reasoning.clone(),
            });
        }
        return Ok(());
    }

    // 3. 文本增量（含 <think> 标签流式解析，对齐 Node 249-293 行）
    let Some(text) = &delta.content else {
        return Ok(());
    };
    if text.is_empty() {
        return Ok(());
    }

    result.content.push_str(text);
    for ev in think_state.push(text) {
        match ev {
            ThinkEvent::Think(t) => {
                start_stream(text_stream_started, StreamMode::Think, ctx);
                ctx.emit(StreamEvent::Chunk { text: t });
            }
            ThinkEvent::Text(t) => {
                // 对齐 Node llm.js:180-181：正文首次出现才发 start，
                // 不在每个 chunk 之间 end/start（切换 end 只在 </think> 处发生）
                start_stream(text_stream_started, StreamMode::Text, ctx);
                ctx.emit(StreamEvent::Chunk { text: t });
            }
            ThinkEvent::EndThink => {
                end_think_stream(text_stream_started, ctx);
            }
        }
    }

    Ok(())
}

fn start_stream(started: &mut bool, mode: StreamMode, ctx: &StreamContext) {
    if !*started {
        *started = true;
        ctx.emit(StreamEvent::Start { mode });
    }
}

fn end_text_stream(started: &mut bool, ctx: &StreamContext) {
    if *started {
        *started = false;
        ctx.emit(StreamEvent::End);
    }
}

fn end_think_stream(started: &mut bool, ctx: &StreamContext) {
    end_text_stream(started, ctx);
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().take(max).collect();
    chars.into_iter().collect()
}

impl std::fmt::Display for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.provider, self.model)
    }
}

/// 便捷：默认 HTTP 客户端（超时设置对齐 Node openai SDK 的默认行为）
pub fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .expect("构建 HTTP client 失败")
}

/// 测试用：不触发日志初始化
#[allow(dead_code)]
fn _unused(_: LogLevel) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::providers::{DEEPSEEK, MIMO, MOONSHOT, OPENAI, QWEN, ZHIPU};

    fn msg(role: &str, content: &str) -> ChatMessage {
        match role {
            "system" => ChatMessage::system(content),
            "user" => ChatMessage::user(content),
            _ => ChatMessage::assistant(content),
        }
    }

    fn req(
        provider: &str,
        model: &str,
        temp: Option<f32>,
        top_p: Option<f32>,
        max_tokens: Option<u32>,
        thinking: bool,
        tools: &[Value],
    ) -> ChatCompletionRequest {
        build_chat_completion_request(
            provider,
            model,
            vec![msg("user", "hi")],
            temp,
            top_p,
            max_tokens,
            thinking,
            tools,
        )
    }

    #[test]
    fn base_request_shape() {
        let r = req(DEEPSEEK, "deepseek-v4-pro", None, None, None, true, &[]);
        assert_eq!(r.model, "deepseek-v4-pro");
        assert!(r.stream);
        assert_eq!(r.messages.len(), 1);
        // deepseek + thinking → reasoning_effort + enabled
        assert_eq!(r.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(r.thinking, Some(json!({ "type": "enabled" })));
        // stream_options 带 include_usage
        assert_eq!(r.stream_options, Some(json!({ "include_usage": true })));
        // 无 temperature/top_p/max_tokens 时不序列化这些键
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("temperature").is_none());
        assert!(v.get("max_tokens").is_none());
    }

    #[test]
    fn deepseek_chat_disables_thinking() {
        let r = req(DEEPSEEK, "deepseek-chat", None, None, None, true, &[]);
        assert_eq!(r.thinking, Some(json!({ "type": "disabled" })));
        assert!(r.reasoning_effort.is_none());
        // thinking=false 同样 disabled
        let r2 = req(DEEPSEEK, "deepseek-v4-pro", None, None, None, false, &[]);
        assert_eq!(r2.thinking, Some(json!({ "type": "disabled" })));
    }

    #[test]
    fn zhipu_omits_sampling_and_stream_options() {
        let r = req(ZHIPU, "glm-5.1", Some(0.8), Some(0.9), None, true, &[]);
        // temperature clamp + 两位小数
        assert_eq!(r.temperature, Some(0.8));
        assert!(r.top_p.is_none()); // zhipu 不带 top_p
        assert!(r.stream_options.is_none()); // zhipu 不带 stream_options
                                             // zhipu 无思考时发 disabled
        let r2 = req(ZHIPU, "glm-5.1", None, None, None, false, &[]);
        assert_eq!(r2.thinking, Some(json!({ "type": "disabled" })));
    }

    #[test]
    fn route_model_selects_fast_for_background_scenarios() {
        let fast = LlmConfig {
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            fast_model: "deepseek-chat".into(),
            api_key: "k".into(),
            base_url: "http://x".into(),
        };
        assert_eq!(fast.route_model("tick"), "deepseek-chat");
        assert_eq!(fast.route_model("wakeup"), "deepseek-chat");
        assert_eq!(fast.route_model("startup"), "deepseek-chat");
        assert_eq!(fast.route_model("interactive"), "deepseek-v4-pro");
        assert_eq!(fast.route_model(""), "deepseek-v4-pro");

        let no_fast = LlmConfig {
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            fast_model: String::new(),
            api_key: "k".into(),
            base_url: "http://x".into(),
        };
        assert_eq!(no_fast.route_model("tick"), "deepseek-v4-pro", "无 fast_model 时回退主模型");
    }

    #[test]
    fn openai_default_sampling_model_omits_temperature() {
        let r = req(
            OPENAI,
            "gpt-5.5",
            Some(0.5),
            Some(0.9),
            Some(100),
            true,
            &[],
        );
        assert!(r.temperature.is_none());
        assert!(r.top_p.is_none());
        // 用 max_completion_tokens
        assert!(r.max_tokens.is_none());
        assert_eq!(r.max_completion_tokens, Some(100));
        // gpt-4o 正常带采样
        let r2 = req(OPENAI, "gpt-4o", Some(0.5), Some(0.9), Some(100), true, &[]);
        assert_eq!(r2.temperature, Some(0.5));
        assert_eq!(r2.top_p, Some(0.9));
        assert_eq!(r2.max_tokens, Some(100));
        assert!(r2.max_completion_tokens.is_none());
    }

    #[test]
    fn moonshot_kimi_omits_sampling_and_thinking_rules() {
        let r = req(MOONSHOT, "kimi-k2.6", Some(0.5), Some(0.9), None, true, &[]);
        assert!(r.temperature.is_none());
        assert!(r.top_p.is_none());
        // k2.6 支持关闭 thinking（thinking=true 时不发 disabled）
        assert!(r.thinking.is_none());
        let r2 = req(MOONSHOT, "kimi-k2.6", None, None, None, false, &[]);
        assert_eq!(r2.thinking, Some(json!({ "type": "disabled" })));
        // k2.7-code 强制思考：thinking=false 也不发 disabled
        let r3 = req(MOONSHOT, "kimi-k2.7-code", None, None, None, false, &[]);
        assert!(r3.thinking.is_none());
    }

    #[test]
    fn tools_added_with_auto_choice() {
        let tools = vec![json!({"type": "function", "function": {"name": "get_time"}})];
        let r = req(DEEPSEEK, "deepseek-v4-pro", None, None, None, true, &tools);
        assert_eq!(r.tool_choice.as_deref(), Some("auto"));
        assert_eq!(r.tools.as_ref().unwrap().len(), 1);
        // zhipu 加 tool_stream
        let r2 = req(ZHIPU, "glm-5.1", None, None, None, true, &tools);
        assert_eq!(r2.tool_stream, Some(true));
        // qwen 正常
        let r3 = req(QWEN, "qwen-turbo", None, None, None, true, &tools);
        assert!(r3.tool_stream.is_none());
        assert_eq!(r3.tool_choice.as_deref(), Some("auto"));
    }

    #[test]
    fn mimo_uses_normal_request() {
        let r = req(MIMO, "mimo-v2.5-pro", Some(0.3), Some(0.9), None, true, &[]);
        assert_eq!(r.temperature, Some(0.3));
        assert_eq!(r.top_p, Some(0.9));
        assert!(r.thinking.is_none()); // mimo 不在 thinking disabled 名单
    }

    #[test]
    fn normalize_temperature_zhipu_clamps() {
        let t = normalize_temperature(ZHIPU, "glm-5.1", Some(1.5));
        assert_eq!(t, Some(1.0));
        let t2 = normalize_temperature(ZHIPU, "glm-5.1", Some(0.12345));
        assert_eq!(t2, Some(0.12));
    }

    /// 护栏：纯文本流走 [DONE] 正常结束后必须补发 End（对齐 Node llm.js:326）。
    /// 修复前：finish_result 不发 End → events.last() 是 Chunk 而非 End，本测试必失败。
    #[tokio::test]
    async fn plain_text_stream_emits_end_on_done() {
        use std::sync::atomic::AtomicBool;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::mpsc;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await; // 读掉请求头即可
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"世界\"}}]}\n\n",
                "data: [DONE]\n\n",
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let llm_cfg = LlmConfig {
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            api_key: "test-key".into(),
            fast_model: String::new(),
            base_url: format!("http://{addr}"),
        };
        let request = build_chat_completion_request(
            DEEPSEEK,
            "deepseek-v4-pro",
            vec![ChatMessage::user("hi")],
            None,
            None,
            None,
            false,
            &[],
        );
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let ctx = StreamContext {
            aborted: Arc::new(AtomicBool::new(false)),
            on_stream: Some(Arc::new(move |e| {
                let _ = tx.send(e);
            })),
            idle_timeout: None,
            request_id: Some("test_req".into()),
            metrics: None,
            stage: String::new(),
        };

        let res = stream_once(&Client::new(), &llm_cfg, &request, &ctx)
            .await
            .expect("mock SSE 流应成功");
        assert_eq!(res.content, "你好世界");

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert!(
            matches!(
                events.first(),
                Some(StreamEvent::Start {
                    mode: StreamMode::Text
                })
            ),
            "首个事件应为 Start(text): {events:?}"
        );
        assert!(
            matches!(events.last(), Some(StreamEvent::End)),
            "末位事件必须是 End（流正常结束未补发 End）: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::End))
                .count(),
            1,
            "End 必须恰好一次: {events:?}"
        );
        assert_eq!(
            events.len(),
            4,
            "序列: Start/Chunk/Chunk/End, 实际 {events:?}"
        );
    }

    /// 零侵入回归（评审修订 #8）：未挂采集器（metrics=None）时 stream_once 行为与
    /// 埋点前完全一致——无指标事件、无额外错误，本测试即证据。
    /// P1-2：幂等键由逻辑请求 ID 派生，重试共享（provider 完成但响应丢失 → 重试带同一键）。
    #[test]
    fn idempotency_key_is_stable_across_retries() {
        let k1 = idempotency_key("req_abc123");
        assert_eq!(k1, idempotency_key("req_abc123"), "同一逻辑请求幂等键必须稳定");
        assert_ne!(k1, idempotency_key("req_abc124"), "不同逻辑请求幂等键必须不同");
        assert!(
            reqwest::header::HeaderValue::from_str(&k1).is_ok(),
            "幂等键必须可作为 HTTP header 值"
        );
    }

    #[tokio::test]
    async fn stream_without_metrics_collector_is_zero_invasion() {
        use std::sync::atomic::AtomicBool;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::mpsc;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let llm_cfg = LlmConfig {
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            api_key: "test-key".into(),
            fast_model: String::new(),
            base_url: format!("http://{addr}"),
        };
        let request = build_chat_completion_request(
            DEEPSEEK,
            "deepseek-v4-pro",
            vec![ChatMessage::user("hi")],
            None,
            None,
            None,
            false,
            &[],
        );
        let ctx = StreamContext {
            aborted: Arc::new(AtomicBool::new(false)),
            on_stream: None,
            idle_timeout: None,
            request_id: Some("test_req_no_metrics".into()),
            metrics: None,
            stage: String::new(),
        };
        let res = stream_once(&Client::new(), &llm_cfg, &request, &ctx)
            .await
            .expect("无采集器时流调用应正常");
        assert_eq!(res.content, "ok");
        assert_eq!(res.usage.total_tokens, 0);
        let _ = mpsc::unbounded_channel::<StreamEvent>();
    }
}
