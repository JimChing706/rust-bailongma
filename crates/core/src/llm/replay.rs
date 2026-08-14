//! P1-2 工具执行防重放（幂等修复第二件）。
//!
//! 场景：provider 已完成一轮（含工具调用）但响应在传输中丢失 → 上层以同一逻辑请求重试，
//! 若直接重跑，有副作用的工具（send_message / express / delegate 等）会被执行两次。
//!
//! 对策（M1/M2 已打底，本模块补齐读侧）：
//! - 执行工具前先查 `llm_tool_calls` 台账：同一逻辑请求（request_id + round + tool_name）
//!   已有 `status='ok'` 的记录 → 复用记录结果，不重复执行；
//! - 执行后同步落账（不等 flusher）：保证「执行了就有账」，响应丢失后重试可复用。
//!
//! 零侵入：`call_llm` 的守卫参数为 `Option`，未接线时行为与之前完全一致。

use serde_json::Value;

use crate::db::repositories::llm_metrics::{
    find_tool_call_result, upsert_tool_calls_batch, LlmToolCallRow,
};
use crate::db::Db;

/// 工具防重放守卫（可插拔；DB 实现见 [`DbToolReplayGuard`]）。
pub trait ToolReplayGuard: Send + Sync {
    /// 查询同一逻辑工具调用（request_id + round + tool_name + **args**）是否已有成功执行结果；
    /// 命中返回记录的结果 JSON。审计 L1 修复：键含参数维度，同轮同名不同参数必须重新执行。
    fn find_result(
        &self,
        request_id: &str,
        round: usize,
        tool_name: &str,
        args: &Value,
    ) -> Option<String>;
    /// 同步记录一次工具执行（幂等：同键重复记录按最新参数/结果覆盖）。
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        request_id: &str,
        round: usize,
        tool_name: &str,
        args: &Value,
        result: &str,
        status: &str,
        duration_ms: i64,
        delegated_from: &str,
    );
}

/// 基于 `llm_tool_calls` 台账的 DB 守卫。
/// attempt 固定 1，与工具循环记账路径（`record_tool_call`）同键，天然去重。
pub struct DbToolReplayGuard {
    db: Db,
}

impl DbToolReplayGuard {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl ToolReplayGuard for DbToolReplayGuard {
    fn find_result(
        &self,
        request_id: &str,
        round: usize,
        tool_name: &str,
        args: &Value,
    ) -> Option<String> {
        match find_tool_call_result(&self.db, request_id, round as i64, tool_name, &args.to_string())
        {
            Ok(Some(r)) => Some(r),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("[工具防重放] 台账查询失败，按未命中处理（允许重新执行）: {e}");
                None
            }
        }
    }

    fn record(
        &self,
        request_id: &str,
        round: usize,
        tool_name: &str,
        args: &Value,
        result: &str,
        status: &str,
        duration_ms: i64,
        delegated_from: &str,
    ) {
        if let Err(e) = upsert_tool_calls_batch(
            &self.db,
            &[LlmToolCallRow {
                request_id: request_id.to_string(),
                round: round as i64,
                attempt: 1,
                tool_name: tool_name.to_string(),
                args_json: args.to_string(),
                result_json: result.to_string(),
                status: status.to_string(),
                duration_ms,
                delegated_from: delegated_from.to_string(),
            }],
        ) {
            tracing::warn!("[工具防重放] 同步落账失败（防重放降级为尽力而为）: {e}");
        }
    }
}
