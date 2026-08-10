//! 瞬时错误判定 + 退避重试 + MiMo 模型降级链。
//!
//! 对齐 Node 版 `src/llm.js` 的 `isTransientError` / `streamOnceWithRetry` /
//! `streamOnceWithModelFallback`：
//! - 瞬时错误（5xx/408/网络抖动/空闲超时）有限次退避重试（800ms / 2500ms）
//! - 已流出内容（had_content）不重试，避免 UI 重复
//! - 429（限流）不重试，交给上层配额逻辑
//! - 认证错误（401）不重试、不做模型降级
//! - MiMo provider 主模型失败后按 fallback 链逐个尝试
//! - M1 埋点：三个决策点记录 `RetryDecision`（retry / no_retry_* / fallback / no_fallback_auth）

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;

use crate::error::{CoreError, Result};

use super::caller::{stream_once, LlmConfig, StreamContext};
use super::providers::{get_provider_model_fallbacks, MIMO};
use super::types::{ChatCompletionRequest, StreamOnceResult};

/// 重试退避序列（对齐 BACKOFFS_MS）
pub const BACKOFFS_MS: [u64; 2] = [800, 2500];
/// 最大尝试次数（对齐 MAX_ATTEMPTS = BACKOFFS.length + 1）
pub const MAX_ATTEMPTS: usize = 3;

/// 重试/降级信息（对齐 Node onRetry 回调的载荷）
#[derive(Debug, Clone)]
pub struct RetryInfo {
    pub attempt: u32,
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    pub error: String,
    /// 模型降级（MiMo fallback）
    pub model_fallback: bool,
    pub model: Option<String>,
    pub next_model: Option<String>,
}

/// 瞬时错误判定（对齐 isTransientError：5xx / 408 / 网络类）。
/// 流中断且尚未流出内容（空闲超时 / 建连后断流）视为可重试，对齐 Node
/// `/timeout|socket hang up/` 判定；已流出内容的流错误由调用方按 `had_content`
/// 单独拦截（防重试导致副作用重复执行）。
pub fn is_transient_error(err: &CoreError) -> bool {
    match err {
        CoreError::LlmHttp { status, .. } => (500..600).contains(status) || *status == 408,
        CoreError::Llm(_) => true,
        CoreError::Network(_) => true,
        CoreError::LlmStream {
            had_content: false, ..
        } => true,
        _ => false,
    }
}

/// 认证错误（对齐 isAuthenticationError：401 / unauthorized）
pub fn is_authentication_error(err: &CoreError) -> bool {
    match err {
        CoreError::LlmHttp { status, message } => {
            *status == 401
                || message.to_ascii_lowercase().contains("unauthoriz")
                || message.to_ascii_lowercase().contains("invalid api key")
                || message.to_ascii_lowercase().contains("authentication")
        }
        _ => false,
    }
}

/// 限流错误（429，交给外层配额逻辑，不重试）
pub fn is_rate_limited(err: &CoreError) -> bool {
    matches!(err, CoreError::LlmHttp { status: 429, .. })
}

/// 是否已流出内容（该错误不应重试，避免 UI 重复）
pub fn has_had_content(err: &CoreError) -> bool {
    matches!(
        err,
        CoreError::LlmStream {
            had_content: true,
            ..
        }
    )
}

type OnRetry = Arc<dyn Fn(RetryInfo) + Send + Sync>;

/// M1 埋点助手：重试/降级决策记账
#[allow(clippy::too_many_arguments)]
fn record_decision(
    ctx: &StreamContext,
    request_id: &str,
    attempt: usize,
    decision: &str,
    delay_ms: u64,
    model: Option<&str>,
    next_model: Option<&str>,
) {
    if let Some(m) = &ctx.metrics {
        m.record(super::metrics::MetricEvent::RetryDecision {
            request_id: request_id.to_string(),
            attempt: (attempt + 1) as u32,
            decision: decision.to_string(),
            delay_ms,
            model: model.map(str::to_string),
            next_model: next_model.map(str::to_string),
        });
    }
}

/// 带退避重试的单次流式调用（对齐 streamOnceWithRetry）
pub async fn stream_once_with_retry(
    client: &Client,
    cfg: &LlmConfig,
    request: &ChatCompletionRequest,
    ctx: &StreamContext,
    on_retry: Option<OnRetry>,
) -> Result<StreamOnceResult> {
    let rid = ctx.request_id.clone().unwrap_or_default();
    let mut last_err: Option<CoreError> = None;
    // 循环跑 MAX_ATTEMPTS 次；BACKOFFS_MS 只有 MAX_ATTEMPTS-1 项（末次不退避），
    // 用索引访问最贴合 Node 语义，故允许 needless_range_loop
    #[allow(clippy::needless_range_loop)]
    for attempt in 0..MAX_ATTEMPTS {
        if ctx.is_aborted() {
            return Err(CoreError::LlmAborted);
        }
        match stream_once(client, cfg, request, ctx).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                if matches!(e, CoreError::LlmAborted) || ctx.is_aborted() {
                    return Err(e);
                }
                // 已流出内容不重试；非瞬时错误不重试；429 不重试（外层处理）
                // 每个不重试出口独立记 decision（M1 埋点）
                if has_had_content(&e) {
                    record_decision(ctx, &rid, attempt, "no_retry_had_content", 0, None, None);
                    return Err(e);
                }
                if !is_transient_error(&e) {
                    record_decision(ctx, &rid, attempt, "no_retry_not_transient", 0, None, None);
                    return Err(e);
                }
                if is_rate_limited(&e) {
                    record_decision(ctx, &rid, attempt, "no_retry_429", 0, None, None);
                    return Err(e);
                }
                let msg = error_message(&e);
                if attempt < MAX_ATTEMPTS - 1 {
                    let delay = BACKOFFS_MS[attempt];
                    // ── M1 埋点：重试决策 ──
                    record_decision(ctx, &rid, attempt, "retry", delay, None, None);
                    if let Some(cb) = &on_retry {
                        cb(RetryInfo {
                            attempt: (attempt + 1) as u32,
                            next_attempt: (attempt + 2) as u32,
                            max_attempts: MAX_ATTEMPTS as u32,
                            delay_ms: delay,
                            error: msg.clone(),
                            model_fallback: false,
                            model: None,
                            next_model: None,
                        });
                    }
                    tracing::warn!(
                        "[LLM] 瞬时错误 \"{}\"，{}ms 后第 {} 次尝试",
                        msg.chars().take(80).collect::<String>(),
                        delay,
                        attempt + 2
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| CoreError::Llm("未知瞬时错误".into())))
}

/// MiMo 模型降级链（对齐 streamOnceWithModelFallback）：
/// 非 MiMo 或无可降级模型 → 直接带重试调用；MiMo → 逐个模型尝试
pub async fn stream_once_with_model_fallback(
    client: &Client,
    cfg: &LlmConfig,
    request: &ChatCompletionRequest,
    ctx: &StreamContext,
    on_retry: Option<OnRetry>,
) -> Result<StreamOnceResult> {
    let rid = ctx.request_id.clone().unwrap_or_default();
    let models = get_provider_model_fallbacks(&cfg.provider, Some(&cfg.model));
    if cfg.provider != MIMO || models.len() <= 1 {
        return stream_once_with_retry(client, cfg, request, ctx, on_retry).await;
    }

    let mut last_err: Option<CoreError> = None;
    for (idx, model) in models.iter().enumerate() {
        let mut req = request.clone();
        req.model = model.clone();
        match stream_once_with_retry(client, cfg, &req, ctx, on_retry.clone()).await {
            Ok(r) => {
                if model != &cfg.model {
                    tracing::warn!("[LLM] MiMo model fallback selected \"{model}\"");
                }
                return Ok(r);
            }
            Err(e) => {
                if matches!(e, CoreError::LlmAborted) || ctx.is_aborted() {
                    return Err(e);
                }
                // 已流出内容 / 认证错误不降级
                if has_had_content(&e) || is_authentication_error(&e) {
                    record_decision(ctx, &rid, idx, "no_fallback_auth", 0, Some(model), None);
                    return Err(e);
                }
                let msg = error_message(&e);
                let Some(next) = models.get(idx + 1) else {
                    last_err = Some(e);
                    break;
                };
                // ── M1 埋点：降级决策 ──
                record_decision(ctx, &rid, idx, "fallback", 0, Some(model), Some(next));
                if let Some(cb) = &on_retry {
                    cb(RetryInfo {
                        attempt: (idx + 1) as u32,
                        next_attempt: (idx + 2) as u32,
                        max_attempts: models.len() as u32,
                        delay_ms: 0,
                        error: msg.clone(),
                        model_fallback: true,
                        model: Some(model.clone()),
                        next_model: Some(next.clone()),
                    });
                }
                tracing::warn!(
                    "[LLM] MiMo model \"{model}\" failed before content; falling back to \"{next}\": {}",
                    msg.chars().take(120).collect::<String>()
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| CoreError::Llm("model fallback exhausted".into())))
}

fn error_message(e: &CoreError) -> String {
    // thiserror Display 即为用户可读消息
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_classification() {
        assert!(is_transient_error(&CoreError::LlmHttp {
            status: 500,
            message: "upstream".into()
        }));
        assert!(is_transient_error(&CoreError::LlmHttp {
            status: 503,
            message: "busy".into()
        }));
        assert!(is_transient_error(&CoreError::LlmHttp {
            status: 408,
            message: "timeout".into()
        }));
        assert!(is_transient_error(&CoreError::Llm("socket reset".into())));
        // 空闲超时 / 断流未出内容 → 可重试（Node timeout/socket hang up 对齐）
        assert!(is_transient_error(&CoreError::LlmStream {
            message: "idle".into(),
            had_content: false
        }));
        assert!(!is_transient_error(&CoreError::LlmStream {
            message: "mid".into(),
            had_content: true
        }));
        assert!(!is_transient_error(&CoreError::LlmHttp {
            status: 429,
            message: "rate".into()
        }));
        assert!(!is_transient_error(&CoreError::LlmHttp {
            status: 401,
            message: "bad key".into()
        }));
        assert!(!is_transient_error(&CoreError::LlmHttp {
            status: 400,
            message: "bad request".into()
        }));
        assert!(!is_transient_error(&CoreError::Config("no".into())));
    }

    #[test]
    fn auth_and_rate_limits() {
        assert!(is_authentication_error(&CoreError::LlmHttp {
            status: 401,
            message: "invalid api key".into()
        }));
        assert!(is_authentication_error(&CoreError::LlmHttp {
            status: 403,
            message: "Unauthorized".into()
        }));
        assert!(!is_authentication_error(&CoreError::LlmHttp {
            status: 429,
            message: "limited".into()
        }));
        assert!(is_rate_limited(&CoreError::LlmHttp {
            status: 429,
            message: "slow down".into()
        }));
    }

    #[test]
    fn had_content_detection() {
        assert!(has_had_content(&CoreError::LlmStream {
            message: "idle".into(),
            had_content: true
        }));
        assert!(!has_had_content(&CoreError::LlmStream {
            message: "idle".into(),
            had_content: false
        }));
    }
}
