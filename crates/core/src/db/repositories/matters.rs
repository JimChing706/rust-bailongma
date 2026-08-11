//! matters 仓库：多Agent事项账本数据层（PHILOSOPHY_MULTI_AGENT_MATTER.md 落地）。
//!
//! 核心语义：
//! - 事项 = 差距（期望态 vs 当前态），`acceptance_criteria` 是验收标准（verifiable 字段）；
//! - 三主体分离：`creator_id`（发起）/ `executor_id`（执行）/ `verifier_id`（验证，≠执行）；
//! - 四种死法：completed / cancelled / shelved / expired，终态登记 `death_reason`；
//! - `parent_id` 支持分解：子事项可独立验证才可拆（分解可加性在 `crate::matter` 层判定）。
//!
//! 状态转移的合法性校验在 `crate::matter` 业务层（对齐 turn/turn_state 分工），本仓库只做存取。

use crate::db::Db;
use crate::error::Result;

/// matters 行投影。
#[derive(Debug, Clone)]
pub struct MatterRow {
    pub id: i64,
    pub title: String,
    pub expectation: String,
    pub current_state: String,
    pub gap_desc: String,
    pub acceptance_criteria: String,
    pub status: String,
    pub creator_id: String,
    pub executor_id: Option<String>,
    pub verifier_id: Option<String>,
    pub parent_id: Option<i64>,
    pub evidence: String,
    pub death_reason: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 新建事项（status='open'）。返回 id。
/// 业务校验（验收标准非空、验证者≠执行者）由 `crate::matter::create` 完成，本函数只落库。
#[allow(clippy::too_many_arguments)]
pub fn create(
    db: &Db,
    title: &str,
    expectation: &str,
    current_state: &str,
    gap_desc: &str,
    acceptance_criteria: &str,
    creator_id: &str,
    executor_id: Option<&str>,
    verifier_id: Option<&str>,
    parent_id: Option<i64>,
) -> Result<i64> {
    let conn = db.conn();
    conn.execute(
        "INSERT INTO matters
           (title, expectation, current_state, gap_desc, acceptance_criteria,
            status, creator_id, executor_id, verifier_id, parent_id)
         VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7, ?8, ?9)",
        rusqlite::params![
            title,
            expectation,
            current_state,
            gap_desc,
            acceptance_criteria,
            creator_id,
            executor_id,
            verifier_id,
            parent_id
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 按 id 取一行（无则 None）。
pub fn get(db: &Db, id: i64) -> Result<Option<MatterRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, title, expectation, current_state, gap_desc, acceptance_criteria,
                status, creator_id, executor_id, verifier_id, parent_id, evidence,
                death_reason, started_at, finished_at, created_at, updated_at
         FROM matters WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map([id], row_from)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// 更新状态（updated_at 自动刷新；转移合法性由 `crate::matter` 层校验后调用）。
pub fn set_status(db: &Db, id: i64, status: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE matters SET status = ?2, updated_at = datetime('now') WHERE id = ?1",
        rusqlite::params![id, status],
    )?;
    Ok(())
}

/// 终态落库：状态 + 死因 + 完成时刻（completed/cancelled/shelved/expired 均走此路）。
pub fn mark_finished(db: &Db, id: i64, status: &str, death_reason: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE matters SET status = ?2, death_reason = ?3, finished_at = datetime('now'),
                updated_at = datetime('now')
         WHERE id = ?1",
        rusqlite::params![id, status, death_reason],
    )?;
    Ok(())
}

/// 开始执行：登记 started_at（open → in_progress 由业务层判定后调用）。
pub fn mark_started(db: &Db, id: i64) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE matters SET status = 'in_progress', started_at = datetime('now'),
                updated_at = datetime('now')
         WHERE id = ?1",
        rusqlite::params![id],
    )?;
    Ok(())
}

/// 提交验证证据（in_progress → awaiting_verification 由业务层判定后调用）。
pub fn set_evidence(db: &Db, id: i64, evidence: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE matters SET status = 'awaiting_verification', evidence = ?2,
                updated_at = datetime('now')
         WHERE id = ?1",
        rusqlite::params![id, evidence],
    )?;
    Ok(())
}

/// 扫描非终态事项（活债 = 未关闭事项），按 created_at 升序。
/// 幽灵检测：调用方以 stale_before 过滤「长期未动」的行。
pub fn scan_active(db: &Db) -> Result<Vec<MatterRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, title, expectation, current_state, gap_desc, acceptance_criteria,
                status, creator_id, executor_id, verifier_id, parent_id, evidence,
                death_reason, started_at, finished_at, created_at, updated_at
         FROM matters
         WHERE status NOT IN ('completed', 'cancelled', 'shelved', 'expired')
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], row_from)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 取某事项的全部子事项（分解检查用），按 created_at 升序。
pub fn list_children(db: &Db, parent_id: i64) -> Result<Vec<MatterRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, title, expectation, current_state, gap_desc, acceptance_criteria,
                status, creator_id, executor_id, verifier_id, parent_id, evidence,
                death_reason, started_at, finished_at, created_at, updated_at
         FROM matters WHERE parent_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([parent_id], row_from)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<MatterRow> {
    Ok(MatterRow {
        id: r.get(0)?,
        title: r.get(1)?,
        expectation: r.get(2)?,
        current_state: r.get(3)?,
        gap_desc: r.get(4)?,
        acceptance_criteria: r.get(5)?,
        status: r.get(6)?,
        creator_id: r.get(7)?,
        executor_id: r.get(8)?,
        verifier_id: r.get(9)?,
        parent_id: r.get(10)?,
        evidence: r.get(11)?,
        death_reason: r.get(12)?,
        started_at: r.get(13)?,
        finished_at: r.get(14)?,
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
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

    fn seed(db: &Db) -> i64 {
        create(
            db,
            "接入协作者",
            "协作者能独立跑工具循环",
            "仅骨架",
            "当前协作者没有 LLM 轮",
            "协作者在测试环境完成一次真实工具调用并留痕",
            "ID:000001",
            Some("codex"),
            Some("jarvis"),
            None,
        )
        .unwrap()
    }

    #[test]
    fn create_and_fetch_roundtrip() {
        let db = test_db();
        let id = seed(&db);
        assert!(id > 0);

        let m = get(&db, id).unwrap().unwrap();
        assert_eq!(m.title, "接入协作者");
        assert_eq!(m.status, "open");
        assert_eq!(m.creator_id, "ID:000001");
        assert_eq!(m.executor_id.as_deref(), Some("codex"));
        assert_eq!(m.verifier_id.as_deref(), Some("jarvis"));
        assert!(m.parent_id.is_none());
        assert!(m.evidence.is_empty());
        assert!(m.death_reason.is_empty());
    }

    #[test]
    fn lifecycle_transitions_persist() {
        let db = test_db();
        let id = seed(&db);

        // open → in_progress
        mark_started(&db, id).unwrap();
        assert_eq!(get(&db, id).unwrap().unwrap().status, "in_progress");
        assert!(get(&db, id).unwrap().unwrap().started_at.is_some());

        // in_progress → awaiting_verification（带证据）
        set_evidence(&db, id, "工具调用 trace 已落库 llm_tool_calls").unwrap();
        let m = get(&db, id).unwrap().unwrap();
        assert_eq!(m.status, "awaiting_verification");
        assert_eq!(m.evidence, "工具调用 trace 已落库 llm_tool_calls");

        // awaiting_verification → completed（登记死因）
        mark_finished(&db, id, "completed", "completed").unwrap();
        let m = get(&db, id).unwrap().unwrap();
        assert_eq!(m.status, "completed");
        assert_eq!(m.death_reason, "completed");
        assert!(m.finished_at.is_some());
    }

    #[test]
    fn scan_active_excludes_terminal() {
        let db = test_db();
        let a = seed(&db);
        let b = create(
            &db,
            "b",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            None,
            None,
            None,
        )
        .unwrap();
        mark_finished(&db, a, "cancelled", "cancelled").unwrap();

        let active = scan_active(&db).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, b);
    }

    #[test]
    fn children_are_listable() {
        let db = test_db();
        let parent = seed(&db);
        let child = create(
            &db,
            "子事项",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            None,
            None,
            Some(parent),
        )
        .unwrap();
        let kids = list_children(&db, parent).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].id, child);
    }
}
