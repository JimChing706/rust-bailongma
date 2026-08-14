//! P4 · 自动迭代三件套 + 语料蒸馏（评审 DELIBERATION_FINAL_PLAN §5.1 P4）。
//!
//! 自动迭代 = 快照回滚 + 人工介入硬通道 + 收敛判据，**三件套缺一不启动**（R8 缓解）。
//! 语料蒸馏 = 规则层 ground truth 冷启动（R9 缓解），只蒸馏确定性子问题。
//!
//! - [`snapshot`]：SQLite 在线备份快照 / 整库回滚（任何自动动作可一键回滚）
//! - [`convergence`]：收敛判据（持续改进 / 停滞收敛 / 轮数上限防失控）
//! - [`distill`]：语料蒸馏（意图 / 敏感 / 嵌入块，JSONL 导出，label 写回）
//! - [`run_iteration`]：迭代编排（快照 → 人工批准 → 应用变更 → 验证评分 → 收敛判定 → 失败回滚）
//!
//! 人工介入硬通道本体在 [`crate::approval`]（Phase 1 预置：120s fail-closed mpsc 门），
//! 本模块通过 `approval_gate` 闭包接线，不重复实现。

pub mod convergence;
pub mod distill;
pub mod snapshot;

use std::path::Path;

use convergence::{ConvergenceConfig, ConvergenceStatus, ConvergenceTracker};
use snapshot::DbSnapshot;

use crate::db::Db;
use crate::error::{CoreError, Result};

/// 三件套成员名（缺失报告用）。
pub const TRIO_SNAPSHOT: &str = "snapshot";
pub const TRIO_APPROVAL: &str = "approval";
pub const TRIO_CONVERGENCE: &str = "convergence";

/// 自动迭代三件套就绪状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrioReadiness {
    /// 三件套齐全，可启动。
    Ready,
    /// 缺失成员（缺一不启动）。
    Missing { missing: Vec<&'static str> },
}

/// 三件套可用性声明（由接线方注入，测试可任意组合）。
#[derive(Debug, Clone, Default)]
pub struct AutoIterateGuard {
    /// 快照回滚可用（snapshot::DbSnapshot 接线）
    pub snapshot_available: bool,
    /// 人工介入硬通道可用（approval::ApprovalGate 接线）
    pub approval_available: bool,
    /// 收敛判据可用（convergence::ConvergenceTracker 接线）
    pub convergence_available: bool,
}

impl AutoIterateGuard {
    pub fn readiness(&self) -> TrioReadiness {
        let mut missing = Vec::new();
        if !self.snapshot_available {
            missing.push(TRIO_SNAPSHOT);
        }
        if !self.approval_available {
            missing.push(TRIO_APPROVAL);
        }
        if !self.convergence_available {
            missing.push(TRIO_CONVERGENCE);
        }
        if missing.is_empty() {
            TrioReadiness::Ready
        } else {
            TrioReadiness::Missing { missing }
        }
    }
}

/// 一次自动迭代的最终结果。
#[derive(Debug, Clone)]
pub struct IterationOutcome {
    /// 实际执行的变更轮数。
    pub rounds: u32,
    /// 最终收益分。
    pub final_score: f64,
    /// 终止状态（Converged 正常 / MaxRounds 硬停）。
    pub status: ConvergenceStatus,
    /// 快照文件路径（收敛成功后保留，供调用方决定清理或留作回滚点）。
    pub snapshot_path: String,
}

/// 自动迭代主流程。
///
/// 每轮：人工批准 → 应用变更 → 记录收益分 → 收敛判定。
/// 任何一轮 `apply_change` 失败 → 从本轮开始前快照整库回滚，返回错误。
/// 变更未获人工批准 → 中止且不产生任何变更。
///
/// - `apply_change`：应用一次变更并返回收益分（调用方负责变更本身的可逆性之外的所有语义）。
/// - `approval_gate`：人工确认门，输入变更描述，返回是否放行
///   （生产接线用 [`crate::approval`]，测试注入假门；多轮场景可用 AllowSession 语义一次放行）。
///
/// `final_score` 初始值在正常路径必被覆盖（首轮成功即赋值，之后每轮覆盖）；
/// 仅批准被拒 / 首轮变更失败提前返回时不经读取，故允许 unused_assignments。
#[allow(unused_assignments)]
pub fn run_iteration<F, G>(
    db: &Db,
    snapshot_dir: &Path,
    tag: &str,
    config: ConvergenceConfig,
    guard: &AutoIterateGuard,
    apply_change: &mut F,
    approval_gate: &mut G,
) -> Result<IterationOutcome>
where
    F: FnMut() -> Result<f64>,
    G: FnMut(&str) -> Result<bool>,
{
    // 三件套缺一不启动（R8）
    if let TrioReadiness::Missing { missing } = guard.readiness() {
        return Err(CoreError::State(format!(
            "自动迭代三件套缺一不启动，缺失: {}",
            missing.join(", ")
        )));
    }

    // 迭代开始前快照（任何自动动作可一键回滚）
    let snapshot = DbSnapshot::create(db, snapshot_dir, tag)?;
    let mut tracker = ConvergenceTracker::new(config);
    let mut rounds: u32 = 0;
    let mut final_score: Option<f64> = None;

    loop {
        let description = format!("自动迭代第 {} 轮变更（tag={tag}）", rounds + 1);
        // 人工介入硬通道：拒绝即中止，无变更落库。
        // R2（审计修复）：approval_gate 返回 Err（门异常）同样先清理快照再返回——
        // 旧实现 `?` 提前返回跳过 cleanup，快照文件泄漏在磁盘上。
        match approval_gate(&description) {
            Ok(true) => {}
            Ok(false) => {
                snapshot.cleanup().ok();
                return Err(CoreError::State(
                    "变更未获人工批准，本轮自动迭代中止（快照已清理）".to_string(),
                ));
            }
            Err(e) => {
                snapshot.cleanup().ok();
                return Err(CoreError::State(format!(
                    "人工批准门异常，自动迭代中止（快照已清理）：{e}"
                )));
            }
        }

        match apply_change() {
            Ok(score) => {
                final_score = Some(score);
                rounds += 1;
                match tracker.record(score) {
                    ConvergenceStatus::Continue(_) => continue,
                    ConvergenceStatus::Converged { reason } => {
                        return Ok(IterationOutcome {
                            rounds,
                            final_score: final_score.unwrap_or(0.0),
                            status: ConvergenceStatus::Converged { reason },
                            snapshot_path: snapshot.path.to_string_lossy().into_owned(),
                        });
                    }
                    ConvergenceStatus::MaxRounds => {
                        return Ok(IterationOutcome {
                            rounds,
                            final_score: final_score.unwrap_or(0.0),
                            status: ConvergenceStatus::MaxRounds,
                            snapshot_path: snapshot.path.to_string_lossy().into_owned(),
                        });
                    }
                }
            }
            Err(e) => {
                // 失败 → 整库回滚到迭代前状态
                snapshot.restore(db)?;
                snapshot.cleanup().ok();
                return Err(CoreError::State(format!(
                    "第 {} 轮变更失败，已回滚到快照点：{e}",
                    rounds + 1
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::conversations::insert;

fn test_db() -> Db {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("blm_iter_test_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        Db::open(dir.join("test.db")).unwrap()
    }

    fn snap_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("blm_iter_snap_{}", std::process::id()))
    }

    fn full_guard() -> AutoIterateGuard {
        AutoIterateGuard {
            snapshot_available: true,
            approval_available: true,
            convergence_available: true,
        }
    }

    #[test]
    fn trio_missing_blocks_start() {
        let db = test_db();
        let mut apply = || Ok(1.0f64);
        let mut gate = |_: &str| Ok(true);

        // 缺快照
        let g = AutoIterateGuard { snapshot_available: false, ..full_guard() };
        assert!(matches!(g.readiness(), TrioReadiness::Missing { .. }));
        let err = run_iteration(&db, &snap_dir(), "t", ConvergenceConfig::default(), &g, &mut apply, &mut gate);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("缺一不启动"));

        // 缺批准
        let g = AutoIterateGuard { approval_available: false, ..full_guard() };
        assert!(matches!(g.readiness(), TrioReadiness::Missing { .. }));

        // 缺收敛
        let g = AutoIterateGuard { convergence_available: false, ..full_guard() };
        assert!(matches!(g.readiness(), TrioReadiness::Missing { .. }));

        // 全齐 → Ready
        assert_eq!(full_guard().readiness(), TrioReadiness::Ready);
    }

    #[test]
    fn converged_path_applies_changes() {
        let db = test_db();
        insert(&db, "user", "ID:000001", "初始").unwrap();
        let mut apply_calls = 0u32;
        let mut apply = || {
            apply_calls += 1;
            // 每轮追加一条消息作为"变更"，收益分恒定 → 第 3 轮收敛
            insert(&db, "user", "ID:000001", &format!("变更{apply_calls}")).unwrap();
            Ok(3.0f64)
        };
        let mut approvals = 0u32;
        let mut gate = |_: &str| {
            approvals += 1;
            Ok(true)
        };
        let cfg = ConvergenceConfig { max_rounds: 10, stable_rounds: 2, min_improvement: 0.0 };

        let out = run_iteration(&db, &snap_dir(), "conv", cfg, &full_guard(), &mut apply, &mut gate).unwrap();
        assert!(matches!(out.status, ConvergenceStatus::Converged { .. }));
        assert_eq!(out.rounds, 3);
        assert_eq!(apply_calls, 3);
        assert_eq!(approvals, 3);
        // 变更落库：初始 + 3 条
        let n: i64 = db.conn().query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 4);
        // 快照保留（供回滚）
        assert!(std::path::Path::new(&out.snapshot_path).exists());
        std::fs::remove_file(&out.snapshot_path).ok();
    }

    #[test]
    fn apply_failure_rolls_back_to_snapshot() {
        let db = test_db();
        insert(&db, "user", "ID:000001", "初始").unwrap();
        let mut first = true;
        let mut apply = || -> Result<f64> {
            if first {
                first = false;
                // 第一轮"变更"成功：写入一条
                insert(&db, "user", "ID:000001", "第一轮写入").unwrap();
                Ok(2.0)
            } else {
                // 第二轮失败
                Err(CoreError::State("模拟变更失败".into()))
            }
        };
        let mut gate = |_: &str| Ok(true);
        let cfg = ConvergenceConfig { max_rounds: 10, stable_rounds: 2, min_improvement: 0.0 };

        let err = run_iteration(&db, &snap_dir(), "rb", cfg, &full_guard(), &mut apply, &mut gate).unwrap_err();
        assert!(err.to_string().contains("已回滚"), "错误应说明已回滚: {err}");
        // 回滚后只剩初始 1 条（第一轮的写入也被撤销）
        let n: i64 = db.conn().query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "失败轮应整库回滚到迭代前");
    }

    #[test]
    fn denied_approval_aborts_without_changes() {
        let db = test_db();
        insert(&db, "user", "ID:000001", "初始").unwrap();
        let mut apply = || Ok(1.0f64);
        let mut gate = |_: &str| Ok(false); // 拒绝
        let cfg = ConvergenceConfig::default();

        let err = run_iteration(&db, &snap_dir(), "deny", cfg, &full_guard(), &mut apply, &mut gate).unwrap_err();
        assert!(err.to_string().contains("未获人工批准"));
        let n: i64 = db.conn().query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "拒绝后不得有任何变更");
    }

    #[test]
    fn max_rounds_hard_stop() {
        let db = test_db();
        let mut apply = || Ok(1.0f64);
        let mut gate = |_: &str| Ok(true);
        let cfg = ConvergenceConfig { max_rounds: 2, stable_rounds: 5, min_improvement: 0.0 };

        let out = run_iteration(&db, &snap_dir(), "max", cfg, &full_guard(), &mut apply, &mut gate).unwrap();
        assert!(matches!(out.status, ConvergenceStatus::MaxRounds));
        assert_eq!(out.rounds, 2);
        std::fs::remove_file(&out.snapshot_path).ok();
    }
}
