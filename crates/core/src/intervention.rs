//! 人工介入硬通道（DELIBERATION_FINAL_PLAN Q6，P0 收口项）。
//!
//! 目标：agent 自主循环（工具循环）运行期间，人工可硬性暂停/恢复/回滚，
//! 不依赖 LLM 自觉。默认关闭（`enabled=false`）时零侵入，行为与现状完全一致。
//!
//! 语义：
//! - `request_pause`  = 硬停：工具循环在下一个检查点停止，不再派发新工具调用；
//! - `resume`         = 解除暂停，继续执行；
//! - `request_rescue` = 回滚：解除暂停并登记一次 rescue 事件（供审计/上报），
//!   由上层决定是否重放本轮。
//!
//! 并发模型：`InterventionGate` 以 `Arc` 共享；暂停/恢复/救援由任意线程
//! （API / 消息入口）发起，工具循环线程只读检查，无锁竞争面。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::error::{CoreError, Result};

/// 检查点结果：工具循环在每个派发点调用 [`InterventionGate::check`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterventionStatus {
    /// 放行（默认；未启用或未暂停）
    Open,
    /// 人工暂停（带原因说明）
    Paused { notice: String },
}

/// 人工介入硬通道（线程安全；`enabled=false` 时恒放行，零侵入）。
#[derive(Debug)]
pub struct InterventionGate {
    enabled: bool,
    pause_requested: AtomicBool,
    rescue_count: AtomicU64,
    notice: Mutex<String>,
}

impl InterventionGate {
    /// 新建通道。`enabled=false`（默认）时 check 恒 Open，与未接入时行为一致。
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pause_requested: AtomicBool::new(false),
            rescue_count: AtomicU64::new(0),
            notice: Mutex::new(String::new()),
        }
    }

    /// 通道是否启用（config 开关）。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 检查点：工具循环在每次派发工具前调用。
    /// 未启用 → 恒 Open；启用且被暂停 → Paused{notice}。
    pub fn check(&self) -> InterventionStatus {
        if !self.enabled {
            return InterventionStatus::Open;
        }
        if self.pause_requested.load(Ordering::Acquire) {
            let notice = self
                .notice
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            return InterventionStatus::Paused { notice };
        }
        InterventionStatus::Open
    }

    /// 请求暂停（幂等：已暂停则仅更新 notice）。
    pub fn request_pause(&self, notice: impl Into<String>) -> Result<()> {
        if !self.enabled {
            return Err(CoreError::Config(
                "人工介入通道未启用（intervention.enabled=false）".into(),
            ));
        }
        *self.notice.lock().unwrap_or_else(|p| p.into_inner()) = notice.into();
        self.pause_requested.store(true, Ordering::Release);
        Ok(())
    }

    /// 解除暂停。
    pub fn resume(&self) {
        self.pause_requested.store(false, Ordering::Release);
    }

    /// 救援回滚：解除暂停并登记一次 rescue（幂等；未暂停时也允许登记）。
    pub fn request_rescue(&self) -> Result<()> {
        if !self.enabled {
            return Err(CoreError::Config(
                "人工介入通道未启用（intervention.enabled=false）".into(),
            ));
        }
        self.resume();
        self.rescue_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// 累计 rescue 次数（审计/上报用）。
    pub fn rescue_count(&self) -> u64 {
        self.rescue_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_gate_always_open_and_rejects_commands() {
        let gate = InterventionGate::new(false);
        assert_eq!(gate.check(), InterventionStatus::Open);
        assert!(!gate.enabled());
        // 未启用时暂停/救援命令被拒绝（fail-closed，防止无效开关误报）
        assert!(gate.request_pause("x").is_err());
        assert!(gate.request_rescue().is_err());
        assert_eq!(gate.rescue_count(), 0);
    }

    #[test]
    fn pause_resume_cycle() {
        let gate = InterventionGate::new(true);
        assert_eq!(gate.check(), InterventionStatus::Open);
        gate.request_pause("人工复核中").unwrap();
        assert_eq!(
            gate.check(),
            InterventionStatus::Paused {
                notice: "人工复核中".into()
            }
        );
        gate.resume();
        assert_eq!(gate.check(), InterventionStatus::Open);
    }

    #[test]
    fn pause_is_idempotent_and_notice_updates() {
        let gate = InterventionGate::new(true);
        gate.request_pause("第一次").unwrap();
        gate.request_pause("第二次").unwrap();
        assert_eq!(
            gate.check(),
            InterventionStatus::Paused {
                notice: "第二次".into()
            }
        );
    }

    #[test]
    fn rescue_clears_pause_and_counts() {
        let gate = InterventionGate::new(true);
        gate.request_pause("需要回滚").unwrap();
        assert!(matches!(gate.check(), InterventionStatus::Paused { .. }));
        gate.request_rescue().unwrap();
        assert_eq!(gate.check(), InterventionStatus::Open);
        assert_eq!(gate.rescue_count(), 1);
        // 再次救援（未暂停）也允许登记
        gate.request_rescue().unwrap();
        assert_eq!(gate.rescue_count(), 2);
    }
}
