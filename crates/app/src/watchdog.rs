//! 波5：唤醒可靠性 —— 循环级 watchdog + 假死自愈。
//!
//! 背景（RESEARCH_RELIABILITY_LLM_SAFETY.md 唤醒可靠性 P2/P3）：波4实证显示
//! 唤醒循环是裸 `tokio::spawn` + `loop`——无 panic 捕获、无心跳、无探活。
//! 循环一旦 panic 或卡死，提醒静默失效且无人知晓。
//!
//! 本模块提供：
//! - [`WatchdogState`]：心跳 + 重启计数 + 最近错误（`/status` 探活数据源）。
//! - [`LoopSupervisor`]：守护一个 worker 循环——panic/退出自动重启（指数退避），
//!   心跳超时判假死 → abort 重启；每次重启回调 `on_restart`（落 brain_ui_events）。
//!
//! 语义边界：本模块只保证「循环活着且被看见」，不改变唤醒业务（合并/幂等/预算
//! 仍在 wakeup.rs / coalesced_wakeup 内）；fired_at 幂等兜底重启窗口的重复唤醒。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use tokio::task::JoinHandle;

/// 循环守护状态（/status 探活数据源；多处 Clone 共享同一份）。
#[derive(Clone, Default)]
pub struct WatchdogState {
    /// 最近一次心跳（epoch 毫秒；0 = 从未跳）。
    last_heartbeat_ms: Arc<AtomicU64>,
    /// 累计重启次数（panic / 假死 / 异常退出）。
    restart_count: Arc<AtomicU64>,
    /// 其中因假死（心跳超时）重启的次数。
    stuck_count: Arc<AtomicU64>,
    /// 最近一次重启原因。
    last_error: Arc<Mutex<Option<String>>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl WatchdogState {
    /// worker 每轮迭代刷一次心跳（证明循环活着）。
    pub fn beat(&self) {
        self.last_heartbeat_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// 距上次心跳的时间；从未跳 → `Duration::MAX`（视为假死）。
    pub fn heartbeat_age(&self) -> Duration {
        let last = self.last_heartbeat_ms.load(Ordering::Relaxed);
        if last == 0 {
            return Duration::MAX;
        }
        Duration::from_millis(now_ms().saturating_sub(last))
    }

    pub fn restart_count(&self) -> u64 {
        self.restart_count.load(Ordering::Relaxed)
    }

    pub fn stuck_count(&self) -> u64 {
        self.stuck_count.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    /// 记录一次重启（stuck=true 表示假死重启）。
    pub fn record_restart(&self, reason: &str, stuck: bool) {
        self.restart_count.fetch_add(1, Ordering::Relaxed);
        if stuck {
            self.stuck_count.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut g) = self.last_error.lock() {
            *g = Some(reason.to_string());
        }
    }

    /// /status 探活快照。
    pub fn snapshot(&self) -> Value {
        let last = self.last_heartbeat_ms.load(Ordering::Relaxed);
        let last_heartbeat = if last == 0 {
            Value::Null
        } else {
            chrono::DateTime::from_timestamp_millis(last as i64)
                .map(|d| json!(d.to_rfc3339()))
                .unwrap_or(Value::Null)
        };
        let age = if last == 0 {
            Value::Null
        } else {
            json!(self.heartbeat_age().as_millis())
        };
        json!({
            "last_heartbeat": last_heartbeat,
            "heartbeat_age_ms": age,
            "restart_count": self.restart_count(),
            "stuck_count": self.stuck_count(),
            "last_error": self.last_error(),
        })
    }
}

/// 循环监督器：守护 worker 循环，panic/退出/假死 → 指数退避重启。
pub struct LoopSupervisor;

/// A1（审计修复）：假死 worker 强杀后的限时等待。
///
/// 旧实现 `worker_handle.abort()` 后直接 `await`：若 worker 卡在同步阻塞段
/// （std Mutex 锁 DB / 长 SQLite 查询，abort 无法抢占），await 永久挂起 →
/// supervisor 连同监控一起死亡，假死自愈彻底失效。
/// 现在限时等待：正常取消立即返回；超时则放弃该任务（占用线程但保活），
/// supervisor 记录 stuck 并立即进入重启流程。
const STUCK_ABORT_GRACE: Duration = Duration::from_millis(500);

impl LoopSupervisor {
    /// 启动监督循环（返回 supervisor 的 JoinHandle；abort 它可整体停掉）。
    ///
    /// - `worker(state)`：每次启动/重启都会重新调用，生成新的 worker future。
    /// - `heartbeat_timeout`：心跳超时判假死（应 ≥ 数倍轮询间隔，防误杀）。
    /// - `backoff_base`：指数退避基数（实际 `base * 2^min(restarts,6)`，上限 60s）。
    /// - `on_restart(reason)`：每次重启回调（落 brain_ui_events / 告警日志）。
    pub fn spawn<F, Fut>(
        state: WatchdogState,
        worker: F,
        heartbeat_timeout: Duration,
        backoff_base: Duration,
        on_restart: impl Fn(&str) + Send + Sync + 'static,
    ) -> JoinHandle<()>
    where
        F: Fn(WatchdogState) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(async move {
            let mut restarts: u64 = 0;
            loop {
                let worker_state = state.clone();
                let mut worker_handle = tokio::spawn(worker(worker_state));

                // 假死监视：周期检查心跳，超时则发信号（supervisor abort worker）。
                let mon_state = state.clone();
                let mut monitor = tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(heartbeat_timeout).await;
                        if mon_state.heartbeat_age() > heartbeat_timeout {
                            return; // 心跳超时 → 通知 supervisor 重启
                        }
                    }
                });

                let outcome = tokio::select! {
                    r = &mut worker_handle => {
                        monitor.abort();
                        match r {
                            Ok(()) => "clean-exit",
                            Err(e) if e.is_panic() => "panic",
                            Err(_) => "cancelled",
                        }
                    }
                    _ = &mut monitor => {
                        // 假死：强杀 worker。异步任务 abort 后很快取消；但若 worker
                        // 卡在同步阻塞段（std Mutex 锁 DB / 长查询）abort 不生效，
                        // 无限 await 会让 supervisor 陪葬（审计 A1 实锤）。
                        // 限时等待：超时放弃该任务（占用线程但保活），重启新循环。
                        worker_handle.abort();
                        tokio::time::timeout(STUCK_ABORT_GRACE, &mut worker_handle)
                            .await
                            .ok();
                        "stuck-heartbeat-timeout"
                    }
                };

                state.record_restart(outcome, outcome.starts_with("stuck"));
                on_restart(outcome);
                restarts += 1;

                // 指数退避（封顶 60s）：崩溃风暴时降频重启，避免打满 CPU。
                let factor = 2u32.pow(restarts.min(6) as u32);
                let backoff = std::cmp::min(backoff_base * factor, Duration::from_secs(60));
                tokio::time::sleep(backoff).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 轮询等待条件成立（async 版：让出 runtime，不阻塞 supervisor 任务）。
    async fn wait_until(cond: impl Fn() -> bool, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn healthy_worker_beats_heartbeat_and_stays_running() {
        let state = WatchdogState::default();
        let handle = LoopSupervisor::spawn(
            state.clone(),
            |w| async move {
                loop {
                    w.beat();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            },
            Duration::from_millis(500),
            Duration::from_millis(1),
            |_| {},
        );
        // 跑 120ms：心跳新鲜、零重启
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(state.heartbeat_age() < Duration::from_millis(200));
        assert_eq!(state.restart_count(), 0);
        let snap = state.snapshot();
        assert_eq!(snap["restart_count"], 0);
        assert!(snap["last_heartbeat"].is_string());
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn panic_worker_restarts_and_counts() {
        let state = WatchdogState::default();
        let handle = LoopSupervisor::spawn(
            state.clone(),
            |_| async { panic!("wakeup worker boom") },
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_| {},
        );
        let ok = wait_until(|| state.restart_count() >= 1, Duration::from_secs(3)).await;
        assert!(ok, "panic 后应自动重启");
        assert_eq!(state.stuck_count(), 0);
        let err = state.last_error().unwrap_or_default();
        assert!(err.contains("panic"), "最近错误应为 panic，实际: {err}");
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn stuck_in_sync_block_supervisor_survives() {
        // A1（审计修复）：worker 卡死在【同步阻塞段】（不 yield 到 runtime，
        // 模拟 std Mutex 锁 DB / 长 SQLite 查询）时 abort 无法立即取消——
        // supervisor 必须限时放弃/等待，而不是无限 await 陪葬。
        //
        // 场景建模：首次启动即假死（偶发 DB 锁），同步段持续 150ms 后自行恢复；
        // 重启后的新 worker 恢复健康。注意不能写成「永久卡死」——僵尸 worker
        // 会永久占用一个 multi_thread worker 线程，测试结束 drop runtime 时
        // 无法 join 该线程导致进程挂住（超出 supervisor 职责，非产品缺陷）。
        // 必须用 multi_thread runtime：current_thread 会被同步阻塞整个卡死。
        let state = WatchdogState::default();
        let handle = LoopSupervisor::spawn(
            state.clone(),
            |w| async move {
                w.beat();
                // 同步段：模拟卡死在 DB 锁（150ms 后自行恢复）
                let stuck_until = std::time::Instant::now() + Duration::from_millis(150);
                while std::time::Instant::now() < stuck_until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // 恢复后：健康循环
                loop {
                    w.beat();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            },
            Duration::from_millis(40),
            Duration::from_millis(1),
            |_| {},
        );
        let ok = wait_until(|| state.stuck_count() >= 1, Duration::from_secs(3)).await;
        assert!(ok, "同步段卡死也应判假死并重启（限时放弃保活）");
        // 重启后的健康 worker 持续心跳 → 不再产生新 stuck
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(state.stuck_count(), 1, "恢复后不应再判假死");
        let err = state.last_error().unwrap_or_default();
        assert!(err.contains("stuck"), "最近错误应为假死，实际: {err}");
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn stuck_worker_restarted_by_heartbeat_timeout() {
        let state = WatchdogState::default();
        let handle = LoopSupervisor::spawn(
            state.clone(),
            |w| async move {
                // 只跳一次心跳，然后假死（模拟 run_wakeup_turn 卡死）
                w.beat();
                loop {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
            },
            Duration::from_millis(60),
            Duration::from_millis(1),
            |_| {},
        );
        let ok = wait_until(|| state.stuck_count() >= 1, Duration::from_secs(3)).await;
        assert!(ok, "心跳超时应判假死并重启");
        assert!(state.restart_count() >= 1);
        let err = state.last_error().unwrap_or_default();
        assert!(err.contains("stuck"), "最近错误应为假死，实际: {err}");
        handle.abort();
        let _ = handle.await;
    }
}
