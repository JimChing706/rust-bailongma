//! P1-1 唤醒闭环：reminders 合并器 + 周窗口预算闸门。
//!
//! 目标：把 N 个到期提醒触发器合并成 **1 次唤醒**（一次 LLM 调用），
//! 避免 N 个触发器各自唤醒导致 LLM 成本翻 N 倍；周窗口预算闸门
//! 直接消费 M4 的唤醒成本账本（`wakeup_cost_weekly`，stage='wakeup'），
//! 本周唤醒 tokens 超预算时拒绝唤醒（先有账再设闸，R7）。

use crate::db::repositories::llm_metrics::wakeup_cost_weekly;
use crate::db::repositories::reminders::{due_reminders, mark_fired, ReminderRow};
use crate::db::Db;
use crate::error::Result;

/// 合并唤醒结果：N 条到期提醒 → 1 次唤醒。
#[derive(Debug, Clone)]
pub struct CoalescedWakeup {
    /// 被合并的提醒 id（消费后标记 fired）
    pub reminder_ids: Vec<i64>,
    /// 合并后的唤醒消息（单条 system_message，喂给一次 LLM 调用）
    pub merged_message: String,
    /// 触发器数量（N）
    pub trigger_count: usize,
}

/// 合并器（纯函数）：N 个触发器 → 1 条唤醒消息。
/// 消息按 due_at 顺序排列，每条一行；task 为空时回退到 system_message。
pub fn coalesce(reminders: &[ReminderRow]) -> CoalescedWakeup {
    let mut ids = Vec::with_capacity(reminders.len());
    let mut lines = Vec::with_capacity(reminders.len());
    for r in reminders {
        ids.push(r.id);
        let body = if r.task.trim().is_empty() {
            r.system_message.trim().to_string()
        } else {
            r.task.trim().to_string()
        };
        lines.push(format!("- [{}] {}", r.due_at, body));
    }
    let merged_message = if lines.is_empty() {
        String::new()
    } else {
        let mut s = format!("有 {} 条到期提醒待处理：", lines.len());
        s.push('\n');
        s.push_str(&lines.join("\n"));
        s
    };
    CoalescedWakeup {
        reminder_ids: ids,
        merged_message,
        trigger_count: reminders.len(),
    }
}

/// 预算闸门裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// 放行：本周已耗唤醒 tokens + 剩余额度
    Allow { used_tokens: i64, remaining_tokens: i64 },
    /// 拦截：本周唤醒 tokens 已超预算
    Blocked { used_tokens: i64, budget_tokens: i64 },
}

/// 周窗口预算闸门：消费 M4 唤醒成本账本（stage='wakeup' 周窗口聚合）。
/// budget_tokens <= 0 表示闸门关闭（纯观测，不拦截）。
pub fn wakeup_budget_gate(db: &Db, days: i64, budget_tokens: i64) -> Result<GateDecision> {
    let cost = wakeup_cost_weekly(db, days)?;
    if budget_tokens > 0 && cost.total_tokens >= budget_tokens {
        Ok(GateDecision::Blocked {
            used_tokens: cost.total_tokens,
            budget_tokens,
        })
    } else {
        let remaining = if budget_tokens > 0 {
            budget_tokens - cost.total_tokens
        } else {
            i64::MAX
        };
        Ok(GateDecision::Allow {
            used_tokens: cost.total_tokens,
            remaining_tokens: remaining,
        })
    }
}

/// 唤醒闭环查询（审计 A2 修复）：查到期 → 合并 → 预算闸门，**不消费提醒**。
///
/// 原实现（`coalesced_wakeup`）先 `mark_fired` 再返回，交付方广播/LLM 失败时
/// 提醒已置 fired 永久丢失。现拆出查询态：交付方在**成功广播后**再 `mark_fired`，
/// 失败时提醒保持 pending，下轮轮询自动重试（最多重复唤醒一次，可接受）。
/// 闸门拦截时：不唤醒、不消费提醒（保持 pending，等待预算恢复或人工放行）。
/// 无到期提醒时返回 `Ok(None)`。
pub fn due_wakeup(
    db: &Db,
    now: &str,
    days: i64,
    budget_tokens: i64,
) -> Result<Option<CoalescedWakeup>> {
    let due = due_reminders(db, now)?;
    if due.is_empty() {
        return Ok(None);
    }
    let decision = wakeup_budget_gate(db, days, budget_tokens)?;
    match decision {
        GateDecision::Blocked { .. } => Ok(None),
        GateDecision::Allow { .. } => Ok(Some(coalesce(&due))),
    }
}

/// 唤醒闭环入口（查询 + 闸门 + **立即 mark_fired** 消费）。
/// ⚠️ 消费前置的语义缺陷见 [`due_wakeup`]（审计 A2）——生产唤醒循环
/// 已改用 `due_wakeup` 后置标记；本函数保留供测试/兼容旧调用。
pub fn coalesced_wakeup(
    db: &Db,
    now: &str,
    days: i64,
    budget_tokens: i64,
) -> Result<Option<CoalescedWakeup>> {
    let wake = due_wakeup(db, now, days, budget_tokens)?;
    if let Some(w) = &wake {
        mark_fired(db, &w.reminder_ids, now)?;
    }
    Ok(wake)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        open_database(dir.path().join("t.db")).unwrap()
    }

    fn insert_reminder(db: &Db, due_at: &str, task: &str) -> i64 {
        let conn = db.conn();
        conn.execute(
            "INSERT INTO reminders (user_id, due_at, task, system_message, status, source)
             VALUES (?1, ?2, ?3, ?4, 'pending', 'test')",
            rusqlite::params!["ID:000001", due_at, task, format!("sys:{task}")],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_wakeup_call(db: &Db, request_id: &str, started_at: &str, tokens: i64) {
        let conn = db.conn();
        conn.execute(
            "INSERT INTO llm_calls
               (request_id, provider, model, stage, started_at, finish_reason,
                total_tokens, cached_tokens, had_content, retryable)
             VALUES (?1, 'deepseek', 'deepseek-v4-pro', 'wakeup', ?2, 'done',
                     ?3, 0, 1, 0)",
            rusqlite::params![request_id, started_at, tokens],
        )
        .unwrap();
    }

    // ── 验收门槛：合并器风暴测试（N 触发器 → 1 次唤醒） ──
    #[test]
    fn coalesce_storm_n_triggers_one_wakeup() {
        let db = test_db();
        // 8 个触发器同时到期
        let ids: Vec<i64> = (1..=8)
            .map(|i| insert_reminder(&db, &format!("2026-08-10T0{i}:00:00+08:00"), &format!("任务{i}")))
            .collect();

        let wake = coalesced_wakeup(&db, "2026-08-10T08:00:00+08:00", 7, 100_000)
            .unwrap()
            .expect("到期提醒应产生 1 次唤醒");

        // 关键断言：8 个触发器 → 1 条合并消息（1 次 LLM 调用）
        assert_eq!(wake.trigger_count, 8);
        assert_eq!(wake.reminder_ids.len(), 8);
        assert_eq!(wake.merged_message.lines().count(), 9, "1 行标题 + 8 行明细");
        assert!(wake.merged_message.contains("8 条到期提醒"));
        assert!(wake.merged_message.contains("任务1"));
        assert!(wake.merged_message.contains("任务8"));

        // 8 个触发器消费后全部标记 fired（不会重复唤醒）
        let conn = db.conn();
        let fired: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM reminders WHERE id IN (?,?,?,?,?,?,?,?) AND status='fired'",
                rusqlite::params_from_iter(ids.iter()),
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fired, 8, "风暴后全部标记 fired");
    }

    #[test]
    fn coalesce_pure_function_groups_all_rows() {
        let rows = vec![
            ReminderRow {
                id: 1,
                user_id: "ID:000001".into(),
                due_at: "2026-08-10T08:00:00+08:00".into(),
                task: "喂猫".into(),
                system_message: "sys:喂猫".into(),
                source: "test".into(),
            },
            ReminderRow {
                id: 2,
                user_id: "ID:000001".into(),
                due_at: "2026-08-10T08:05:00+08:00".into(),
                task: "".into(),
                system_message: "系统维护检查".into(),
                source: "test".into(),
            },
        ];
        let w = coalesce(&rows);
        assert_eq!(w.trigger_count, 2);
        assert!(w.merged_message.contains("喂猫"));
        assert!(
            w.merged_message.contains("系统维护检查"),
            "task 为空应回退 system_message"
        );
    }

    // ── 预算闸门：先有账再设闸 ──
    #[test]
    fn budget_gate_allows_within_budget() {
        let db = test_db();
        insert_wakeup_call(&db, "w1", "2026-08-09T09:00:00+08:00", 30_000);
        let d = wakeup_budget_gate(&db, 7, 100_000).unwrap();
        assert_eq!(
            d,
            GateDecision::Allow {
                used_tokens: 30_000,
                remaining_tokens: 70_000,
            }
        );
    }

    #[test]
    fn budget_gate_blocks_when_over_budget() {
        let db = test_db();
        insert_wakeup_call(&db, "w1", "2026-08-09T09:00:00+08:00", 120_000);
        let d = wakeup_budget_gate(&db, 7, 100_000).unwrap();
        assert_eq!(
            d,
            GateDecision::Blocked {
                used_tokens: 120_000,
                budget_tokens: 100_000,
            }
        );
    }

    #[test]
    fn budget_gate_zero_budget_disabled() {
        let db = test_db();
        insert_wakeup_call(&db, "w1", "2026-08-09T09:00:00+08:00", 999_999);
        let d = wakeup_budget_gate(&db, 7, 0).unwrap();
        assert!(matches!(d, GateDecision::Allow { .. }), "0 预算 = 闸门关闭");
    }

    #[test]
    fn coalesced_wakeup_blocked_keeps_reminders_pending() {
        let db = test_db();
        let id = insert_reminder(&db, "2026-08-10T08:00:00+08:00", "被闸门拦截");
        insert_wakeup_call(&db, "w1", "2026-08-09T09:00:00+08:00", 200_000);

        let out = coalesced_wakeup(&db, "2026-08-10T08:00:00+08:00", 7, 100_000).unwrap();
        assert!(out.is_none(), "超预算不应唤醒");

        // 提醒保持 pending，未被消费
        let conn = db.conn();
        let status: String = conn
            .query_row("SELECT status FROM reminders WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn coalesced_wakeup_none_when_no_due() {
        let db = test_db();
        insert_reminder(&db, "2026-08-11T08:00:00+08:00", "未来提醒");
        let out = coalesced_wakeup(&db, "2026-08-10T08:00:00+08:00", 7, 100_000).unwrap();
        assert!(out.is_none(), "未到期不应唤醒");
    }

    #[test]
    fn due_wakeup_does_not_consume_reminders() {
        // 审计 A2：查询态不消费——交付失败后提醒保持 pending，下轮可重试，
        // 不再出现「先 mark_fired 后广播，广播失败即永久丢失」。
        let db = test_db();
        let id = insert_reminder(&db, "2026-08-10T08:00:00+08:00", "到期提醒");
        let now = "2026-08-10T08:30:00+08:00";

        let w = due_wakeup(&db, now, 7, 100_000).unwrap().expect("有到期提醒");
        assert_eq!(w.trigger_count, 1);
        assert_eq!(w.reminder_ids, vec![id]);

        // 未消费：due_reminders 仍返回该提醒（可再次唤醒）
        let due = due_reminders(&db, now).unwrap();
        assert_eq!(due.len(), 1, "查询不消费，提醒仍是 pending");

        // 交付成功后再消费 → 不再返回
        mark_fired(&db, &w.reminder_ids, now).unwrap();
        assert!(due_wakeup(&db, now, 7, 100_000).unwrap().is_none(), "消费后不再唤醒");
    }

    #[test]
    fn due_wakeup_respects_budget_gate() {
        // 审计 A2：闸门拦截时查询同样不消费（与 coalesced_wakeup 语义一致）
        let db = test_db();
        let id = insert_reminder(&db, "2026-08-10T08:00:00+08:00", "被闸门拦截");
        insert_wakeup_call(&db, "w1", "2026-08-09T09:00:00+08:00", 200_000);

        let out = due_wakeup(&db, "2026-08-10T08:00:00+08:00", 7, 100_000).unwrap();
        assert!(out.is_none(), "超预算不应返回唤醒");

        let conn = db.conn();
        let status: String = conn
            .query_row("SELECT status FROM reminders WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "pending");
    }
}
