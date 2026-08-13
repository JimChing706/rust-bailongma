# -*- coding: utf-8 -*-
"""波5 补丁：注册 watchdog 模块 + AppRuntime 挂守护状态 + 重写 spawn_wakeup_loop + /status 探活。"""
import io
import re
import sys

def patch(path, pairs, regex_pairs=None):
    with io.open(path, "r", encoding="utf-8") as f:
        text = f.read()
    for old, new in pairs:
        assert old in text, f"{path}: 未找到锚点: {old[:60]!r}"
        text = text.replace(old, new, 1)
    for pat, new in (regex_pairs or []):
        text2, n = re.subn(pat, new, text, count=1, flags=re.S)
        assert n == 1, f"{path}: 正则未命中: {pat[:60]!r}"
        text = text2
    with io.open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    print(f"[ok] {path}")

# 1) lib.rs: 注册模块
patch("crates/app/src/lib.rs", [
    ("pub mod service;\n", "pub mod service;\npub mod watchdog;\n"),
])

# 2) service.rs: import + 字段 + assemble 初始化 + 重写 spawn_wakeup_loop + 新测试
patch("crates/app/src/service.rs", [
    (
        "use bailongma_core::wakeup::{coalesced_wakeup, CoalescedWakeup};\n",
        "use bailongma_core::wakeup::{coalesced_wakeup, CoalescedWakeup};\nuse crate::watchdog::{LoopSupervisor, WatchdogState};\n",
    ),
    (
        "    pub llm_metrics_flusher: FlusherHandle,\n",
        "    pub llm_metrics_flusher: FlusherHandle,\n"
        "    /// 波5：唤醒循环守护状态（心跳/重启计数；/status 探活数据源）。\n"
        "    pub wakeup_watchdog: WatchdogState,\n",
    ),
    (
        "            llm_metrics_flusher,\n        }",
        "            llm_metrics_flusher,\n            wakeup_watchdog: WatchdogState::default(),\n        }",
    ),
    (
        '        assert!(line.contains("喂猫"));\n    }',
        '        assert!(line.contains("喂猫"));\n    }\n\n'
        "    #[tokio::test]\n"
        "    async fn wakeup_watchdog_heartbeat_visible_after_first_tick() {\n"
        "        // 波5：健康循环心跳新鲜、零重启；快照可探活\n"
        "        let runtime = test_runtime();\n"
        '        insert_due_reminder(&runtime.db, "2026-08-11T08:00:00+08:00", "探活测试");\n'
        "        let mut rx = runtime.bus.subscribe();\n"
        "        let _handle = runtime.spawn_wakeup_loop();\n"
        "        let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv())\n"
        "            .await\n"
        "            .expect(\"首个 tick 应广播\")\n"
        "            .expect(\"channel 不应关闭\");\n"
        "        assert_eq!(runtime.wakeup_watchdog.restart_count(), 0);\n"
        "        assert!(runtime.wakeup_watchdog.heartbeat_age() < Duration::from_secs(5));\n"
        "        let snap = runtime.wakeup_watchdog.snapshot();\n"
        "        assert_eq!(snap[\"restart_count\"], 0);\n"
        "        assert!(snap[\"last_heartbeat\"].is_string());\n"
        "    }",
    ),
], regex_pairs=[
    (
        r"    pub fn spawn_wakeup_loop\(&self\) -> tokio::task::JoinHandle<\(\)> \{[\s\S]*?\n    \}\n}",
        """    /// 波5（唤醒可靠性）：外层套 LoopSupervisor 守护——panic/退出自动重启
    /// （指数退避）、心跳超时假死自愈（abort+重启）、/status 探活。
    /// 新增配置：wakeup_watchdog_timeout_secs（心跳超时，默认 180s，最小 5s）、
    /// wakeup_watchdog_backoff_secs（重启退避基数，默认 1s）。
    pub fn spawn_wakeup_loop(&self) -> tokio::task::JoinHandle<()> {
        let runtime = self.clone();
        let interval_secs = self
            .cfg
            .extra
            .get("wakeup_interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(60)
            .max(1);
        let days = self
            .cfg
            .extra
            .get("wakeup_days")
            .and_then(|v| v.as_i64())
            .unwrap_or(7);
        let budget = self
            .cfg
            .extra
            .get("wakeup_budget_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let watchdog_timeout = Duration::from_secs(
            self.cfg
                .extra
                .get("wakeup_watchdog_timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(180)
                .max(5),
        );
        let backoff_base = Duration::from_secs(
            self.cfg
                .extra
                .get("wakeup_watchdog_backoff_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(1),
        );

        let worker = {
            let runtime = runtime.clone();
            move |wstate: WatchdogState| {
                let runtime = runtime.clone();
                async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
                    loop {
                        interval.tick().await;
                        wstate.beat();
                        let now = now_input_ts();
                        match coalesced_wakeup(&runtime.db, &now, days, budget) {
                            Ok(Some(wake)) => {
                                tracing::info!(
                                    "[wakeup] {} 条到期提醒合并为 1 次唤醒",
                                    wake.trigger_count
                                );
                                runtime.run_wakeup_turn(wake).await;
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!("[wakeup] 唤醒轮失败: {e}"),
                        }
                    }
                }
            }
        };

        // 波5：每次重启落 brain_ui_events（自愈事件可观测）+ error 日志
        let on_restart = {
            let db = runtime.db.clone();
            move |reason: &str| {
                tracing::error!("[wakeup] 循环重启（{reason}），watchdog 已拉起新循环");
                brain_ui_events::insert_brain_ui_event(
                    &db,
                    &now_input_ts(),
                    "l2",
                    "wakeup_restart",
                    &json!({ "reason": reason }),
                );
            }
        };

        LoopSupervisor::spawn(
            self.wakeup_watchdog.clone(),
            worker,
            watchdog_timeout,
            backoff_base,
            on_restart,
        )
    }
}""",
    ),
])

# 3) api_host.rs: /status 注入 wakeup 探活
patch("crates/app/src/api_host.rs", [
    (
        '    let status = Arc::new(|| json!({ "running": true }));',
        """    // 波5：/status 探活 —— wakeup 守护状态（心跳/重启计数/待处理提醒数）
    let wakeup_state = runtime.wakeup_watchdog.clone();
    let status_db = runtime.db.clone();
    let status = Arc::new(move || {
        let pending = status_db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM reminders WHERE status = 'pending'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        let mut wakeup = wakeup_state.snapshot();
        if let Some(obj) = wakeup.as_object_mut() {
            obj.insert("pending_reminders".into(), json!(pending));
        }
        json!({ "running": true, "wakeup": wakeup })
    });""",
    ),
])

print("ALL PATCHES APPLIED")
