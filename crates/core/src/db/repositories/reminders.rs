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
/// 审计 D1 修复：不再对 ISO 文本做字典序比较（`Z` 与 `+08:00` 混排时字典序
/// 与真实时间序不一致），改用 SQLite `datetime()` 归一化到 UTC 后比较——
/// 支持 `...Z`、`...+08:00`、空格或 T 分隔等 ISO 8601 变体；不可解析的脏数据
/// `datetime()` 返回 NULL → 比较为 NULL → 不匹配（fail-safe，不误唤醒）。
pub fn due_reminders(db: &Db, now: &str) -> Result<Vec<ReminderRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, user_id, due_at, task, system_message, source
         FROM reminders
         WHERE status = 'pending' AND datetime(due_at) <= datetime(?1)
         ORDER BY datetime(due_at) ASC",
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
        "INSERT INTO reminders (user_id, due_at, task, system_message, status, source)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
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
    fn due_reminders_normalizes_mixed_z_and_offset() {
        // 审计 D1 回归：Z（UTC）与 +08:00 混排时按真实时间序比较，而非字典序。
        // 2026-08-10T00:00:00Z == 2026-08-10T08:00:00+08:00（同一时刻）。
        // 若用字典序比较，'Z' < '+'（0x5A < 0x2B 不成立，实际 '+'=0x2B < 'Z'=0x5A），
        // 文本序会得出完全错误的先后。
        let db = test_db();
        // 同一时刻的三条（三种书写方式）
        insert_reminder(&db, "2026-08-10T00:00:00Z", "UTC写法", "pending");
        insert_reminder(&db, "2026-08-10T08:00:00+08:00", "东八写法", "pending");
        // 晚于该时刻 1 小时的提醒（+09:00 偏移的同一时刻等价写法也算到期）
        insert_reminder(&db, "2026-08-10T01:00:00+09:00", "晚1h", "pending");
        // 早于该时刻的提醒
        insert_reminder(&db, "2026-08-09T23:30:00Z", "早0.5h", "pending");

        let due = due_reminders(&db, "2026-08-10T00:30:00Z").unwrap();
        // 到期：d（早）、a、b、c（2026-08-10T00:00:00+08:00 实际是 08-10T00:00Z，晚于 00:30Z？——不对，需复核）
        // 2026-08-10T01:00:00+09:00 = 2026-08-09T16:00:00Z，早于 now。重新核算：
        //   a: 2026-08-10T00:00:00Z（= now）
        //   b: 2026-08-10T08:00:00+08:00 = 2026-08-10T00:00:00Z（= now）
        //   c: 2026-08-10T01:00:00+09:00 = 2026-08-09T16:00:00Z（早于 now）
        //   d: 2026-08-09T23:30:00Z（早于 now）
        // 四条全部 <= now（含等号边界）→ 全到期，且顺序按 UTC 升序：c(16:00) < d(23:30) < a=b(00:00)
        assert_eq!(due.len(), 4, "四条均到期（<= 含边界）");
        assert_eq!(
            due[0].task, "晚1h",
            "UTC 最早：+09:00 对应 UTC 前一日 16:00"
        );
        assert_eq!(due[1].task, "早0.5h", "其次 23:30Z");
        // a 与 b 为同一 UTC 时刻，两者相对顺序不保证（无稳定排序），仅断言集合
        let mut tail: Vec<&str> = due[2..4].iter().map(|r| r.task.as_str()).collect();
        tail.sort_unstable();
        assert_eq!(tail, vec!["UTC写法", "东八写法"]);
        // 边界外：now 之前的时刻不应出现 UTC 后时刻
        let not_yet = insert_reminder(&db, "2026-08-10T02:00:00+00:00", "未到期", "pending");
        let due2 = due_reminders(&db, "2026-08-10T01:00:00Z").unwrap();
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
