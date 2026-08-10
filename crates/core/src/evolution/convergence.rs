//! 收敛判据（P4 自动迭代三件套之一）。
//!
//! 自动迭代不能无限跑。`ConvergenceTracker` 以「收益分」为单一度量，
//! 按以下判据终止（缺一不启动的第三件，评审 §5.1 R8「自动迭代误改码」缓解）：
//!
//! 1. **持续改进**：每轮收益分较上轮提升 ≥ `min_improvement` → 继续迭代；
//! 2. **停滞收敛**：连续 `stable_rounds` 轮无有效改进 → `Converged`（正常收敛）；
//! 3. **轮数上限**：达到 `max_rounds` → `MaxRounds`（防失控硬停，即使仍在改进）。
//!
//! 判据顺序：先查轮数上限（失控优先），再查停滞收敛。
//! 调用方保证 `score` 非 NaN（f64 全序比较的前提）。

/// 收敛配置。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConvergenceConfig {
    /// 最大迭代轮数（含首轮），达到即硬停。
    pub max_rounds: u32,
    /// 连续多少轮无有效改进判定为收敛。
    pub stable_rounds: u32,
    /// 「有效改进」的最小收益分增量。
    pub min_improvement: f64,
}

impl Default for ConvergenceConfig {
    fn default() -> Self {
        Self {
            max_rounds: 5,
            stable_rounds: 2,
            min_improvement: 0.0,
        }
    }
}

/// 单轮记录后的迭代状态。
#[derive(Debug, Clone, PartialEq)]
pub enum ConvergenceStatus {
    /// 继续迭代（已进行轮数）。
    Continue(u32),
    /// 停滞收敛（正常终止）。
    Converged { reason: String },
    /// 达到轮数上限（防失控硬停）。
    MaxRounds,
}

/// 收敛追踪器：跨轮维护上一轮收益分与停滞计数。
#[derive(Debug, Clone)]
pub struct ConvergenceTracker {
    pub config: ConvergenceConfig,
    round: u32,
    last_score: Option<f64>,
    stable_count: u32,
}

impl ConvergenceTracker {
    pub fn new(config: ConvergenceConfig) -> Self {
        Self {
            config,
            round: 0,
            last_score: None,
            stable_count: 0,
        }
    }

    /// 当前已进行轮数。
    pub fn round(&self) -> u32 {
        self.round
    }

    /// 记录一轮的收益分，返回下一轮该做什么。
    pub fn record(&mut self, score: f64) -> ConvergenceStatus {
        debug_assert!(!score.is_nan(), "score 不允许 NaN");
        self.round += 1;

        match self.last_score {
            None => {
                // 首轮：只建立基线，无法判断改进
                self.last_score = Some(score);
            }
            Some(prev) => {
                let delta = score - prev;
                if delta > self.config.min_improvement {
                    self.stable_count = 0;
                } else {
                    self.stable_count += 1;
                }
                self.last_score = Some(score);
            }
        }

        // 1) 轮数上限优先：防失控
        if self.round >= self.config.max_rounds {
            return ConvergenceStatus::MaxRounds;
        }
        // 2) 停滞收敛
        if self.stable_count >= self.config.stable_rounds {
            return ConvergenceStatus::Converged {
                reason: format!(
                    "连续 {} 轮收益分无有效改进（stable_rounds={}, min_improvement={})",
                    self.stable_count, self.config.stable_rounds, self.config.min_improvement
                ),
            };
        }
        ConvergenceStatus::Continue(self.round)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_improvement_runs_until_max_rounds() {
        let cfg = ConvergenceConfig {
            max_rounds: 4,
            stable_rounds: 2,
            min_improvement: 0.0,
        };
        let mut t = ConvergenceTracker::new(cfg);
        // 每轮都在改进 → 永不收敛 → 到轮数上限硬停
        assert_eq!(t.record(1.0), ConvergenceStatus::Continue(1));
        assert_eq!(t.record(1.5), ConvergenceStatus::Continue(2));
        assert_eq!(t.record(2.0), ConvergenceStatus::Continue(3));
        assert_eq!(t.record(2.5), ConvergenceStatus::MaxRounds);
    }

    #[test]
    fn stagnation_converges_after_stable_rounds() {
        let cfg = ConvergenceConfig {
            max_rounds: 10,
            stable_rounds: 2,
            min_improvement: 0.0,
        };
        let mut t = ConvergenceTracker::new(cfg);
        assert_eq!(t.record(3.0), ConvergenceStatus::Continue(1)); // 基线
        assert_eq!(t.record(3.0), ConvergenceStatus::Continue(2)); // 停滞 1
        assert!(matches!(
            t.record(3.0),
            ConvergenceStatus::Converged { .. }
        )); // 停滞 2 → 收敛
        assert!(matches!(
            t.record(3.0),
            ConvergenceStatus::Converged { .. }
        ));
    }

    #[test]
    fn improvement_resets_stagnation_counter() {
        let cfg = ConvergenceConfig {
            max_rounds: 10,
            stable_rounds: 2,
            min_improvement: 0.1,
        };
        let mut t = ConvergenceTracker::new(cfg);
        assert_eq!(t.record(1.0), ConvergenceStatus::Continue(1)); // 基线
        assert_eq!(t.record(1.05), ConvergenceStatus::Continue(2)); // 改进 0.05 < 0.1 → 停滞 1
        assert_eq!(t.record(1.2), ConvergenceStatus::Continue(3)); // 改进 0.15 ≥ 0.1 → 重置
        assert_eq!(t.record(1.21), ConvergenceStatus::Continue(4)); // 停滞 1
        assert!(matches!(
            t.record(1.21),
            ConvergenceStatus::Converged { .. }
        )); // 停滞 2 → 收敛
    }

    #[test]
    fn max_rounds_wins_over_convergence() {
        let cfg = ConvergenceConfig {
            max_rounds: 2,
            stable_rounds: 1,
            min_improvement: 0.0,
        };
        let mut t = ConvergenceTracker::new(cfg);
        assert_eq!(t.record(5.0), ConvergenceStatus::Continue(1));
        // 第 2 轮同时满足停滞(1)与上限(2) → 上限优先
        assert_eq!(t.record(5.0), ConvergenceStatus::MaxRounds);
    }
}
