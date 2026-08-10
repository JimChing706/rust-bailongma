//! 显式 Turn 状态机（Phase 1）。
//!
//! 计划（bailongma-multiagent-enhancement Phase 1「显式 Turn 状态机」）：
//! - 每个 user/tick turn 在 `turn_state` 表占一行（见 [`crate::db::repositories::turn_state`]），
//!   状态全程落库：`received → running → waiting_approval → completed / failed / cancelled`；
//! - 启动时扫描未终态 turn，按 `recover_policy` 恢复（resume / retry / mark_failed）；
//! - 后续小步提供 `resume_turn` / `cancel_turn` / `replay_turn(dry_run)` / `inspect_turn_trace`
//!   API（本模块先给纯逻辑核心，落库与 HTTP 接线在数据层/API 小步接入）。
//!
//! 本模块只做状态转移与恢复决策（纯函数），不碰 DB —— 便于单测与离线回放。

use std::str::FromStr;

/// Turn 状态集合（初版；后续按方案 §4.1 展开）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnState {
    /// 已入队（turn_state 行已建，等待执行）
    Received,
    /// 执行中（LLM / 工具循环 / 落库广播）
    Running,
    /// 等待人工确认（Phase 1 人工确认机制：高风险工具调用挂起）
    WaitingApproval,
    /// 正常完成
    Completed,
    /// 失败（`last_error` 记录原因；可按 recover_policy 恢复）
    Failed,
    /// 用户/策略取消
    Cancelled,
}

impl TurnState {
    /// 落库用的稳定字符串（与 DB 列值一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnState::Received => "received",
            TurnState::Running => "running",
            TurnState::WaitingApproval => "waiting_approval",
            TurnState::Completed => "completed",
            TurnState::Failed => "failed",
            TurnState::Cancelled => "cancelled",
        }
    }

    /// 是否终态（终态不再参与恢复扫描）。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TurnState::Completed | TurnState::Failed | TurnState::Cancelled
        )
    }

    /// 合法转移矩阵（显式白名单；非法转移返回 false，由调用方拒绝落库）。
    pub fn can_transition_to(&self, to: TurnState) -> bool {
        use TurnState::*;
        matches!(
            (self, to),
            (Received, Running)
                | (Received, Cancelled)
                | (Running, WaitingApproval)
                | (Running, Completed)
                | (Running, Failed)
                | (Running, Cancelled)
                | (WaitingApproval, Running) // 人工确认通过后恢复执行
                | (WaitingApproval, Completed)
                | (WaitingApproval, Failed)
                | (WaitingApproval, Cancelled)
                | (Failed, Running) // 重试：失败后可回到 running
        )
    }
}

impl FromStr for TurnState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "received" => TurnState::Received,
            "running" => TurnState::Running,
            "waiting_approval" => TurnState::WaitingApproval,
            "completed" => TurnState::Completed,
            "failed" => TurnState::Failed,
            "cancelled" => TurnState::Cancelled,
            other => return Err(format!("未知 turn 状态: {other}")),
        })
    }
}

/// 恢复策略（turn 创建时写入 `turn_state.recover_policy` 列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverPolicy {
    /// 挂起/中断 → 恢复执行（waiting_approval 除外，见 [`decide_recovery`]）
    Resume,
    /// 失败/中断 → 重试（attempt +1，受 max_attempts 约束）
    Retry,
    /// 失败/中断 → 直接标记 failed
    MarkFailed,
}

impl RecoverPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecoverPolicy::Resume => "resume",
            RecoverPolicy::Retry => "retry",
            RecoverPolicy::MarkFailed => "mark_failed",
        }
    }
}

impl FromStr for RecoverPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "resume" => RecoverPolicy::Resume,
            "mark_failed" => RecoverPolicy::MarkFailed,
            "retry" => RecoverPolicy::Retry,
            other => return Err(format!("未知恢复策略: {other}")),
        })
    }
}

/// 启动扫描后对单个未终态 turn 的恢复决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverDecision {
    /// 进入 running（恢复执行 / 重试）
    Running,
    /// 标记 failed（原状态保留在 last_error/审计中）
    MarkFailed,
    /// 保持现状不动（终态；或 waiting_approval 等待人工确认）
    Hold,
}

/// 默认最大重试次数（attempt 达到该值后 retry 策略转为 MarkFailed）。
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// 恢复决策（纯函数）：state + recover_policy + attempt/max_attempts → 决策。
///
/// 安全约束（Phase 1 人工确认机制）：
/// `waiting_approval` 在任何恢复策略下都返回 [`RecoverDecision::Hold`]——
/// 绝不自动恢复、绝不自动重试、绝不自动标记失败。否则 `Retry` 策略会在
/// 重启时把未获人工批准的高危工具调用直接恢复执行（绕过人工确认），
/// `MarkFailed` 也会擅自终结一个未决的确认。重启后保持挂起，等人确认或取消。
pub fn decide_recovery(
    state: TurnState,
    policy: RecoverPolicy,
    attempt: u32,
    max_attempts: u32,
) -> RecoverDecision {
    if state.is_terminal() {
        return RecoverDecision::Hold;
    }
    // 人工确认未完成：任何策略都 Hold（安全第一，见函数文档）。
    if state == TurnState::WaitingApproval {
        return RecoverDecision::Hold;
    }
    match policy {
        RecoverPolicy::Resume => RecoverDecision::Running,
        RecoverPolicy::Retry => {
            if attempt >= max_attempts {
                RecoverDecision::MarkFailed
            } else {
                RecoverDecision::Running
            }
        }
        RecoverPolicy::MarkFailed => RecoverDecision::MarkFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_whitelist() {
        use TurnState::*;
        // 合法
        assert!(Received.can_transition_to(Running));
        assert!(Running.can_transition_to(WaitingApproval));
        assert!(WaitingApproval.can_transition_to(Running));
        assert!(WaitingApproval.can_transition_to(Completed));
        assert!(Failed.can_transition_to(Running));
        // 非法
        assert!(!Received.can_transition_to(Completed));
        assert!(!Completed.can_transition_to(Running));
        assert!(!Cancelled.can_transition_to(Running));
        assert!(!Received.can_transition_to(Failed));
    }

    #[test]
    fn terminal_states() {
        use TurnState::*;
        assert!(Completed.is_terminal());
        assert!(Failed.is_terminal());
        assert!(Cancelled.is_terminal());
        assert!(!Running.is_terminal());
        assert!(!WaitingApproval.is_terminal());
    }

    #[test]
    fn parse_roundtrip() {
        for s in [
            "received",
            "running",
            "waiting_approval",
            "completed",
            "failed",
            "cancelled",
        ] {
            let st: TurnState = s.parse().unwrap();
            assert_eq!(st.as_str(), s);
        }
        assert!("nope".parse::<TurnState>().is_err());
        assert_eq!(
            "mark_failed".parse::<RecoverPolicy>().unwrap(),
            RecoverPolicy::MarkFailed
        );
        assert_eq!(
            RecoverPolicy::Retry.as_str(),
            "retry"
        );
    }

    #[test]
    fn recovery_decisions() {
        use RecoverDecision::*;
        // retry 未超限 → running
        assert_eq!(
            decide_recovery(TurnState::Running, RecoverPolicy::Retry, 1, 3),
            Running
        );
        // retry 超限 → mark_failed
        assert_eq!(
            decide_recovery(TurnState::Running, RecoverPolicy::Retry, 3, 3),
            MarkFailed
        );
        // resume + running → running
        assert_eq!(
            decide_recovery(TurnState::Running, RecoverPolicy::Resume, 1, 3),
            Running
        );
        // mark_failed → mark_failed
        assert_eq!(
            decide_recovery(TurnState::Running, RecoverPolicy::MarkFailed, 1, 3),
            MarkFailed
        );
        // 终态 → hold
        assert_eq!(
            decide_recovery(TurnState::Completed, RecoverPolicy::Retry, 1, 3),
            Hold
        );
    }

    /// 安全回归：waiting_approval 在任何策略下都不自动恢复/重试/终结。
    /// 防止 Retry 策略在重启时绕过人工确认直接执行高危工具。
    #[test]
    fn waiting_approval_never_auto_resumes() {
        use RecoverDecision::*;
        for policy in [
            RecoverPolicy::Resume,
            RecoverPolicy::Retry,
            RecoverPolicy::MarkFailed,
        ] {
            // attempt 0（远未超限）也不能恢复
            assert_eq!(
                decide_recovery(TurnState::WaitingApproval, policy, 0, 3),
                Hold,
                "policy={:?} 不得自动恢复 waiting_approval",
                policy
            );
            // attempt 超限也不能被擅自终结
            assert_eq!(
                decide_recovery(TurnState::WaitingApproval, policy, 5, 3),
                Hold,
                "policy={:?} 不得擅自终结 waiting_approval",
                policy
            );
        }
    }
}
