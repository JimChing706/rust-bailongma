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
///
/// L17（波 1）：due_at 经迁移 + set_reminder 写入侧归一，恒为 UTC `Z` 格式，故直接字符串
/// 比较（字典序 = 时间序，走 idx_reminders_due_at 索引）。`now` 先归一为 Z；不可解析的脏
/// 数据（如 "不是日期"）字典序大于合法时间戳 → 不匹配（fail-safe，不误唤醒）。
pub fn due_reminders(db: &Db, now: &str) -> Result<Vec<ReminderRow>> {
    let now_z = crate::db::models::normalize_to_utc_z(now).unwrap_or_else(|| now.to_string());
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, user_id, due_at, task, system_message, source
         FROM reminders
         WHERE status = 'pending' AND due_at <= ?1
         ORDER BY due_at ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![now_z], |r| {
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

/// 创建一条提醒（status='pending'）。`due_at` 由调用方保证为可解析的 ISO 8601
/// （工具层 `set_reminder` 已归一为 UTC RFC3339；`source` 标识创建来源）。
/// 返回新行 id。
pub fn insert_reminder(
    db: &Db,
    user_id: &str,
    due_at: &str,
    task: &str,
    source: &str,
) -> Result<i64> {
    let conn = db.conn();
    conn.execute(
        "INSERT INTO reminders (user_id, due_at, task, system_message, status, source, created_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        rusqlite::params![user_id, due_at, task, format!("sys:{task}"), source],
    )?;
    Ok(conn.last_insert_rowid())
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
        // L17：对齐生产写入侧（set_reminder 归一 Z），测试数据也归一，直接比较查询才正确
        let due_at = crate::db::models::normalize_to_utc_z(due_at).unwrap_or_else(|| due_at.to_string());
        let conn = db.conn();
        conn.execute(
            "INSERT INTO reminders (user_id, due_at, task, system_message, status, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "ID:000001",
                due_at,
                task,
                format!("sys:{task}"),
                status,
                "test"
            ],
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
            .query_row("SELECT fired_at FROM reminders WHERE id = ?1", [a], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fired_at, "2026-08-10T09:00:00+08:00");
    }

    #[test]
    fn mark_fired_empty_noop() {
        let db = test_db();
        assert_eq!(
            mark_fired(&db, &[], "2026-08-10T09:00:00+08:00").unwrap(),
            0
        );
    }

    #[test]
    fn due_reminders_compares_directly_on_normalized_utc_z() {
        // L17（波 1）：due_at 归一为 UTC Z 后直接字符串比较（走索引）。now 传带偏移 ISO
        // 应先归一为 Z 再比较（同一时刻 +08:00 == Z），按 UTC 升序返回。
        let db = test_db();
        insert_reminder(&db, "2026-08-09T23:30:00Z", "早", "pending");
        insert_reminder(&db, "2026-08-10T00:00:00Z", "恰整点", "pending");

        // now = 2026-08-10T08:30:00+08:00 == 2026-08-10T00:30:00Z
        let due = due_reminders(&db, "2026-08-10T08:30:00+08:00").unwrap();
        assert_eq!(due.len(), 2, "两条明显早于 now");
        assert_eq!(due[0].task, "早");
        assert_eq!(due[1].task, "恰整点");

        // 未到期的不出现
        let not_yet = insert_reminder(&db, "2026-08-10T01:00:00Z", "未到期", "pending");
        let due2 = due_reminders(&db, "2026-08-10T08:30:00+08:00").unwrap();
        assert!(!due2.iter().any(|r| r.id == not_yet));
    }

    #[test]
    fn due_reminders_ignores_invalid_dates_fail_safe() {
        // 审计 D1 fail-safe：脏数据（无法解析的日期）不匹配、不误唤醒。
        let db = test_db();
        insert_reminder(&db, "不是日期", "脏数据", "pending");
        insert_reminder(&db, "2026-13-99T99:99:99", "超范围", "pending");
        let due = due_reminders(&db, "2026-08-10T00:00:00Z").unwrap();
        assert!(due.is_empty(), "无效日期一律不匹配");
    }
}
