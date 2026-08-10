//! 显式 Turn 状态机（Phase 1）。
//!
//! 计划（bailongma-multiagent-enhancement Phase 1「显式 Turn 状态机」）：
//! - 每个 user/tick turn 在 `turn_state` 表占一行（见 [`crate::db::repositories::turn_state`]），
//!   状态全程落库：`received → running → waiting_approval → completed / failed / cancelled`；
//! - 启动时扫描未终态 turn，按 `recover_policy` 恢复（resume / retry / mark_failed）；
//! - 后续小步提供 `resume_turn` / `cancel_turn` / `replay_turn(dry_run)` / `inspect_turn_trace`
//!   API（本模块先给纯逻辑核心，落库与 HTTP 接线在数据层/API 小步接入）。
//!
//! 状态转移与恢复决策是纯函数（不碰 DB，便于单测与离线回放）；
//! 启动恢复 [`recover_unfinished_turns`] 是唯一直接落库的入口（数据层在 [`crate::db`]）。

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

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
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

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
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

// ── 启动恢复（唯一直接落库入口；数据层在 crate::db::repositories::turn_state）──
// 注意：不使用 `use crate::error::Result` 别名——它会把 impl FromStr 的
// `Result<Self, Self::Err>` 误解析为单参别名导致编译失败，这里全用完整路径。

use crate::db::repositories::turn_state;
use crate::db::Db;
use crate::error::CoreError;

/// 启动恢复摘要（供日志/API 展示）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoverySummary {
    /// 恢复为 running（resume / retry 未超限）
    pub recovered: usize,
    /// 标记为 failed（mark_failed 策略 / retry 超限）
    pub marked_failed: usize,
    /// 保持不动（终态 / waiting_approval 等待人工确认）
    pub held: usize,
}

/// 启动扫描：对每个未终态 turn 按 recover_policy 落库恢复。
///
/// 安全约束（与 [`decide_recovery`] 一致）：
/// - `waiting_approval` 一律 Hold——不自动恢复、不自动重试、不自动终结；
/// - resume → running；retry 未超限 → attempt+1 后 running；retry 超限 /
///   mark_failed → failed（保留原 last_error 并追加恢复原因）。
pub fn recover_unfinished_turns(db: &Db) -> crate::error::Result<RecoverySummary> {
    let mut summary = RecoverySummary::default();
    for row in turn_state::scan_unfinished(db)? {
        let state: TurnState = row
            .state
            .parse()
            .map_err(|e: String| CoreError::State(e))?;
        let policy: RecoverPolicy = row
            .recover_policy
            .parse()
            .map_err(|e: String| CoreError::State(e))?;
        match decide_recovery(state, policy, row.attempt as u32, DEFAULT_MAX_ATTEMPTS) {
            RecoverDecision::Running => {
                if policy == RecoverPolicy::Retry {
                    turn_state::bump_attempt(db, row.turn_id)?;
                }
                turn_state::set_state(db, row.turn_id, "running")?;
                summary.recovered += 1;
            }
            RecoverDecision::MarkFailed => {
                let note = format!("startup recovery: {} policy", policy.as_str());
                turn_state::set_error(db, row.turn_id, &note)?;
                turn_state::mark_finished(db, row.turn_id, "failed", &now_ts())?;
                summary.marked_failed += 1;
            }
            RecoverDecision::Hold => {
                summary.held += 1;
            }
        }
    }
    Ok(summary)
}

fn now_ts() -> String {
    chrono::Local::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

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

    /// 启动恢复集成测试：scan_unfinished + decide_recovery + 落库闭环。
    #[test]
    fn startup_recovery_applies_policies() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_database(dir.path().join("t.db")).unwrap();

        // resume → recovered（running，attempt 不动）
        let a = turn_state::create_turn(&db, "t1", "rk-a", "TUI", "ID:000001", "a", None, "resume").unwrap();
        turn_state::set_state(&db, a, "running").unwrap();
        // retry 未超限 → recovered（running，attempt+1）
        let f = turn_state::create_turn(&db, "t6", "rk-f", "TUI", "ID:000001", "f", None, "retry").unwrap();
        turn_state::set_state(&db, f, "running").unwrap();
        // retry 超限（attempt=3 = max）→ marked_failed
        let b = turn_state::create_turn(&db, "t2", "rk-b", "TUI", "ID:000001", "b", None, "retry").unwrap();
        turn_state::set_state(&db, b, "running").unwrap();
        turn_state::bump_attempt(&db, b).unwrap(); // 2
        turn_state::bump_attempt(&db, b).unwrap(); // 3
        // mark_failed 策略 → marked_failed
        let c = turn_state::create_turn(&db, "t3", "rk-c", "TUI", "ID:000001", "c", None, "mark_failed").unwrap();
        turn_state::set_state(&db, c, "running").unwrap();
        // waiting_approval（即使 retry 策略）→ held
        let d = turn_state::create_turn(&db, "t4", "rk-d", "TUI", "ID:000001", "d", None, "retry").unwrap();
        turn_state::set_state(&db, d, "waiting_approval").unwrap();
        // 终态不参与扫描
        let e = turn_state::create_turn(&db, "t5", "rk-e", "TUI", "ID:000001", "e", None, "retry").unwrap();
        turn_state::mark_finished(&db, e, "completed", "now").unwrap();

        let summary = recover_unfinished_turns(&db).unwrap();
        assert_eq!(summary.recovered, 2, "a(resume) + f(retry未超限) 恢复为 running");
        assert_eq!(summary.marked_failed, 2, "b(retry超限) + c(mark_failed)");
        assert_eq!(summary.held, 1, "d(waiting_approval) 保持挂起");

        let ta = turn_state::get_turn(&db, a).unwrap().unwrap();
        assert_eq!(ta.state, "running");
        assert_eq!(ta.attempt, 1, "resume 类不动 attempt");
        let tf = turn_state::get_turn(&db, f).unwrap().unwrap();
        assert_eq!(tf.state, "running");
        assert_eq!(tf.attempt, 2, "retry 未超限 attempt+1");
        let tb = turn_state::get_turn(&db, b).unwrap().unwrap();
        assert_eq!(tb.state, "failed");
        assert_eq!(tb.attempt, 3);
        assert!(tb.last_error.contains("startup recovery"), "失败原因落审计: {}", tb.last_error);
        let tc = turn_state::get_turn(&db, c).unwrap().unwrap();
        assert_eq!(tc.state, "failed");
        let td = turn_state::get_turn(&db, d).unwrap().unwrap();
        assert_eq!(td.state, "waiting_approval", "人工确认绝不自动终结");
        let te = turn_state::get_turn(&db, e).unwrap().unwrap();
        assert_eq!(te.state, "completed");
    }
}
