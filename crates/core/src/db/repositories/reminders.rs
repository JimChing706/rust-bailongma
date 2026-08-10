//! reminders 仓库：到期提醒访问器（P1-1 唤醒闭环第一步）。
//!
//! 对齐 `src/db.js` 的 reminders 表语义：
//! - `status`：'pending' | 'fired' | 'cancelled'
//! - `due_at` 为 ISO 字符串（同格式可直接字典序比较）
//! - 唤醒轮只消费 pending 且已到期的行，消费后标记 fired 防止重复唤醒

use crate::db::Db;
use crate::error::Result;

/// 到期提醒行（唤醒轮消费的最小投影）。
#[derive(Debug, Clone)]
pub struct ReminderRow {
    pub id: i64,
    pub user_id: String,
    pub due_at: String,
    pub task: String,
    pub system_message: String,
    pub source: String,
}

/// 查询到期且未触发的提醒（status='pending' AND due_at <= now），按 due_at 升序。
pub fn due_reminders(db: &Db, now: &str) -> Result<Vec<ReminderRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, user_id, due_at, task, system_message, source
         FROM reminders
         WHERE status = 'pending' AND due_at <= ?1
         ORDER BY due_at ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![now], |r| {
        Ok(ReminderRow {
            id: r.get(0)?,
            user_id: r.get(1)?,
            due_at: r.get(2)?,
            task: r.get(3)?,
            system_message: r.get(4)?,
            source: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 将一批提醒标记为已触发（status='fired'，fired_at 写入 now）。
/// 返回实际更新的行数。
pub fn mark_fired(db: &Db, ids: &[i64], now: &str) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "UPDATE reminders SET status = 'fired', fired_at = ?1 WHERE id IN ({placeholders})"
    );
    let conn = db.conn();
    let mut params = Vec::with_capacity(ids.len() + 1);
    params.push(now.to_string());
    params.extend(ids.iter().map(|i| i.to_string()));
    let n = conn.execute(&sql, rusqlite::params_from_iter(params.iter()))?;
    Ok(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        open_database(dir.path().join("t.db")).unwrap()
    }

    fn insert_reminder(db: &Db, due_at: &str, task: &str, status: &str) -> i64 {
        let conn = db.conn();
        conn.execute(
            "INSERT INTO reminders (user_id, due_at, task, system_message, status, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["ID:000001", due_at, task, format!("sys:{task}"), status, "test"],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn due_reminders_only_pending_and_due() {
        let db = test_db();
        let early = insert_reminder(&db, "2026-08-10T06:00:00+08:00", "早提醒", "pending");
        let future = insert_reminder(&db, "2026-08-11T08:00:00+08:00", "未来提醒", "pending");
        let fired = insert_reminder(&db, "2026-08-09T08:00:00+08:00", "已触发", "fired");
        let cancelled = insert_reminder(&db, "2026-08-09T09:00:00+08:00", "已取消", "cancelled");
        let _ = (future, fired, cancelled);

        let due = due_reminders(&db, "2026-08-10T07:00:00+08:00").unwrap();
        assert_eq!(due.len(), 1, "只应返回 pending 且到期的一条");
        assert_eq!(due[0].id, early);
        assert_eq!(due[0].task, "早提醒");
        assert_eq!(due[0].system_message, "sys:早提醒");
    }

    #[test]
    fn mark_fired_updates_status_and_fired_at() {
        let db = test_db();
        let a = insert_reminder(&db, "2026-08-10T08:00:00+08:00", "A", "pending");
        let b = insert_reminder(&db, "2026-08-10T08:05:00+08:00", "B", "pending");
        let n = mark_fired(&db, &[a, b], "2026-08-10T09:00:00+08:00").unwrap();
        assert_eq!(n, 2);

        let due = due_reminders(&db, "2026-08-10T23:00:00+08:00").unwrap();
        assert!(due.is_empty(), "标记后不应再被访问器返回");

        let conn = db.conn();
        let fired_at: String = conn
            .query_row(
                "SELECT fired_at FROM reminders WHERE id = ?1",
                [a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fired_at, "2026-08-10T09:00:00+08:00");
    }

    #[test]
    fn mark_fired_empty_noop() {
        let db = test_db();
        assert_eq!(mark_fired(&db, &[], "2026-08-10T09:00:00+08:00").unwrap(), 0);
    }
}
