//! turn_state 仓库：显式 Turn 状态机数据层（Phase 1）。
//!
//! 计划（bailongma-multiagent-enhancement Phase 1「显式 Turn 状态机」）：
//! - 每个 user/tick turn 在 `turn_state` 表占一行，状态全程落库：
//!   received → running → waiting_approval → completed / failed / cancelled；
//! - 启动时 [`scan_unfinished`] 扫出未终态 turn，按 `recover_policy` 恢复或标记失败；
//! - `idempotency_key` 唯一（部分索引）：同一逻辑轮重试复用同一行，防重复执行。
//!
//! 状态转移的合法性校验在 `crate::turn` 状态机层（纯函数），本仓库只做存取。

use crate::db::Db;
use crate::error::Result;

/// turn_state 行投影。
#[derive(Debug, Clone)]
pub struct TurnStateRow {
    pub turn_id: i64,
    pub state: String,
    pub round: i64,
    pub attempt: i64,
    pub idempotency_key: String,
    pub conversation_id: Option<i64>,
    pub channel: String,
    pub from_id: String,
    pub input_snapshot: String,
    pub trace_id: String,
    pub last_error: String,
    pub recover_policy: String,
    pub started_at: String,
    pub finished_at: Option<String>,
}

/// 新建一个 turn（state='received'，attempt=1）。返回 turn_id。
#[allow(clippy::too_many_arguments)]
pub fn create_turn(
    db: &Db,
    started_at: &str,
    idempotency_key: &str,
    channel: &str,
    from_id: &str,
    input_snapshot: &str,
    conversation_id: Option<i64>,
    recover_policy: &str,
) -> Result<i64> {
    let conn = db.conn();
    // M22（审计修复）：idempotency_key 部分唯一索引（WHERE idempotency_key != ''）。
    // 并发/在途重复同 key 时不再报错（旧「降级继续」会导致双执行），而是 ON CONFLICT
    // DO NOTHING 并返回 0，由调用方按「已存在」重查该行处理。
    let n = conn.execute(
        "INSERT INTO turn_state
           (state, round, attempt, idempotency_key, conversation_id, channel, from_id,
            input_snapshot, recover_policy, started_at, updated_at)
         VALUES ('received', 0, 1, ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                 strftime('%Y-%m-%dT%H:%M:%fZ','now'))
         ON CONFLICT(idempotency_key) WHERE idempotency_key != '' DO NOTHING",
        rusqlite::params![
            idempotency_key,
            conversation_id,
            channel,
            from_id,
            input_snapshot,
            recover_policy,
            started_at
        ],
    )?;
    if n == 0 {
        return Ok(0); // 冲突：同 idempotency_key 已存在
    }
    Ok(conn.last_insert_rowid())
}

/// 更新状态（updated_at 自动刷新）。转移合法性由 `crate::turn` 层校验后调用。
pub fn set_state(db: &Db, turn_id: i64, state: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE turn_state SET state = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE turn_id = ?1",
        rusqlite::params![turn_id, state],
    )?;
    Ok(())
}

/// 更新工具循环轮次（tool_loop 每轮结束调用）。
pub fn set_round(db: &Db, turn_id: i64, round: i64) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE turn_state SET round = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE turn_id = ?1",
        rusqlite::params![turn_id, round],
    )?;
    Ok(())
}

/// 记录错误信息（不改变状态；错误后的终态由调用方决定）。
pub fn set_error(db: &Db, turn_id: i64, error: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE turn_state SET last_error = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE turn_id = ?1",
        rusqlite::params![turn_id, error],
    )?;
    Ok(())
}

/// 重试轮次 +1（恢复策略 retry 时调用；调用方随后 set_state('running')）。返回新 attempt。
pub fn bump_attempt(db: &Db, turn_id: i64) -> Result<i64> {
    let conn = db.conn();
    conn.execute(
        "UPDATE turn_state SET attempt = attempt + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE turn_id = ?1",
        rusqlite::params![turn_id],
    )?;
    let attempt: i64 = conn.query_row(
        "SELECT attempt FROM turn_state WHERE turn_id = ?1",
        [turn_id],
        |r| r.get(0),
    )?;
    Ok(attempt)
}

/// 终态落库（completed / failed / cancelled + finished_at）。
pub fn mark_finished(db: &Db, turn_id: i64, state: &str, finished_at: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE turn_state SET state = ?2, finished_at = ?3, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE turn_id = ?1",
        rusqlite::params![turn_id, state, finished_at],
    )?;
    Ok(())
}

/// 按 turn_id 取一行（无则 None）。
pub fn get_turn(db: &Db, turn_id: i64) -> Result<Option<TurnStateRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT turn_id, state, round, attempt, idempotency_key, conversation_id, channel,
                from_id, input_snapshot, trace_id, last_error, recover_policy, started_at, finished_at
         FROM turn_state WHERE turn_id = ?1",
    )?;
    let mut rows = stmt.query_map([turn_id], row_from)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// 按幂等键取一行（无则 None）。A4（审计修复）：消息级幂等入口校验用——
/// 客户端/网桥携带的 idempotency_key 命中终态行时，入口直接返回已处理结果，
/// 避免重复消息重复执行（重复发消息/重复扣费）。
pub fn find_by_idempotency_key(db: &Db, key: &str) -> Result<Option<TurnStateRow>> {
    if key.is_empty() {
        return Ok(None);
    }
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT turn_id, state, round, attempt, idempotency_key, conversation_id, channel,
                from_id, input_snapshot, trace_id, last_error, recover_policy, started_at, finished_at
         FROM turn_state WHERE idempotency_key = ?1 LIMIT 1",
    )?;
    let mut rows = stmt.query_map([key], row_from)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// 扫描未终态 turn（state NOT IN 终态集合），按 started_at 升序。
/// 启动恢复用：对返回的每一行按 recover_policy 决策（`crate::turn::decide_recovery`）。
pub fn scan_unfinished(db: &Db) -> Result<Vec<TurnStateRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT turn_id, state, round, attempt, idempotency_key, conversation_id, channel,
                from_id, input_snapshot, trace_id, last_error, recover_policy, started_at, finished_at
         FROM turn_state
         WHERE state NOT IN ('completed', 'failed', 'cancelled')
         ORDER BY started_at ASC",
    )?;
    let rows = stmt.query_map([], row_from)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<TurnStateRow> {
    Ok(TurnStateRow {
        turn_id: r.get(0)?,
        state: r.get(1)?,
        round: r.get(2)?,
        attempt: r.get(3)?,
        idempotency_key: r.get(4)?,
        conversation_id: r.get(5)?,
        channel: r.get(6)?,
        from_id: r.get(7)?,
        input_snapshot: r.get(8)?,
        trace_id: r.get(9)?,
        last_error: r.get(10)?,
        recover_policy: r.get(11)?,
        started_at: r.get(12)?,
        finished_at: r.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        open_database(dir.path().join("t.db")).unwrap()
    }

    #[test]
    fn create_and_transition_roundtrip() {
        let db = test_db();
        let id = create_turn(
            &db,
            "2026-08-10T18:00:00+08:00",
            "key-1",
            "TUI",
            "ID:000001",
            "hello",
            Some(42),
            "retry",
        )
        .unwrap();
        assert!(id > 0);

        let t = get_turn(&db, id).unwrap().unwrap();
        assert_eq!(t.state, "received");
        assert_eq!(t.idempotency_key, "key-1");
        assert_eq!(t.conversation_id, Some(42));
        assert_eq!(t.recover_policy, "retry");

        set_state(&db, id, "running").unwrap();
        set_round(&db, id, 3).unwrap();
        set_error(&db, id, "boom").unwrap();
        let t = get_turn(&db, id).unwrap().unwrap();
        assert_eq!(t.state, "running");
        assert_eq!(t.round, 3);
        assert_eq!(t.last_error, "boom");

        mark_finished(&db, id, "completed", "2026-08-10T18:01:00+08:00").unwrap();
        let t = get_turn(&db, id).unwrap().unwrap();
        assert_eq!(t.state, "completed");
        assert_eq!(t.finished_at.as_deref(), Some("2026-08-10T18:01:00+08:00"));
    }

    #[test]
    fn create_turn_conflict_returns_zero() {
        // M22（审计修复）：同 idempotency_key 二次建行应返回 0（ON CONFLICT DO NOTHING），
        // 供调用方按「已存在」处理，而非报错「降级继续」导致重复执行。
        let db = test_db();
        let first = create_turn(
            &db,
            "2026-08-10T18:00:00+08:00",
            "dup-key",
            "TUI",
            "ID:000001",
            "hello",
            Some(1),
            "retry",
        )
        .unwrap();
        assert!(first > 0);

        let second = create_turn(
            &db,
            "2026-08-10T18:00:01+08:00",
            "dup-key",
            "TUI",
            "ID:000001",
            "hello-again",
            Some(2),
            "retry",
        )
        .unwrap();
        assert_eq!(second, 0, "同 key 二次建行应返回 0 而非报错");

        // 查回仍是首行（未被覆盖）
        let row = find_by_idempotency_key(&db, "dup-key").unwrap().unwrap();
        assert_eq!(row.turn_id, first);
        assert_eq!(row.input_snapshot, "hello");
    }

    #[test]
    fn scan_unfinished_returns_only_active() {
        let db = test_db();
        let a = create_turn(
            &db,
            "2026-08-10T18:00:00+08:00",
            "k-a",
            "TUI",
            "ID:000001",
            "a",
            None,
            "retry",
        )
        .unwrap();
        let b = create_turn(
            &db,
            "2026-08-10T18:01:00+08:00",
            "k-b",
            "TUI",
            "ID:000001",
            "b",
            None,
            "mark_failed",
        )
        .unwrap();
        let c = create_turn(
            &db,
            "2026-08-10T18:02:00+08:00",
            "k-c",
            "TUI",
            "ID:000001",
            "c",
            None,
            "retry",
        )
        .unwrap();
        set_state(&db, a, "running").unwrap();
        mark_finished(&db, b, "completed", "2026-08-10T18:03:00+08:00").unwrap();
        mark_finished(&db, c, "cancelled", "2026-08-10T18:03:00+08:00").unwrap();

        let unfinished = scan_unfinished(&db).unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].turn_id, a);
        assert_eq!(unfinished[0].state, "running");
    }

    #[test]
    fn bump_attempt_increments() {
        let db = test_db();
        let id = create_turn(
            &db,
            "2026-08-10T18:00:00+08:00",
            "k-d",
            "TUI",
            "ID:000001",
            "d",
            None,
            "retry",
        )
        .unwrap();
        assert_eq!(bump_attempt(&db, id).unwrap(), 2);
        assert_eq!(bump_attempt(&db, id).unwrap(), 3);
        let t = get_turn(&db, id).unwrap().unwrap();
        assert_eq!(t.attempt, 3);
    }

    #[test]
    fn find_by_idempotency_key_roundtrip() {
        // A4（审计修复）：入口幂等校验的数据层支撑
        let db = test_db();
        let id = create_turn(
            &db,
            "2026-08-10T18:00:00+08:00",
            "k-e",
            "TUI",
            "ID:000001",
            "e",
            None,
            "retry",
        )
        .unwrap();
        mark_finished(&db, id, "completed", "2026-08-10T18:01:00+08:00").unwrap();

        let hit = find_by_idempotency_key(&db, "k-e").unwrap().unwrap();
        assert_eq!(hit.turn_id, id);
        assert_eq!(hit.state, "completed");

        assert!(find_by_idempotency_key(&db, "k-absent").unwrap().is_none());
        assert!(find_by_idempotency_key(&db, "").unwrap().is_none());
    }
}
