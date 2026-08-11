//! matters 仓库：多Agent事项账本数据层（PHILOSOPHY_MULTI_AGENT_MATTER.md 落地）。
//!
//! 核心语义：
//! - 事项 = 差距（期望态 vs 当前态），`acceptance_criteria` 是验收标准（verifiable 字段）；
//! - 三主体分离：`creator_id`（发起）/ `executor_id`（执行）/ `verifier_id`（验证，≠执行）；
//! - 四种死法：completed / cancelled / shelved / expired，终态登记 `death_reason`；
//! - `parent_id` 支持分解：子事项必须声明可加性关系（`additivity_decl`）；
//! - 命题2/3/6 数据：`intent_original` 意图锚点、`signals` 信号台账（JSON）、
//!   `delegation_*` 五决策点委托位（默认 0 = 人类保留）。
//!
//! 状态转移与委托/声明的合法性校验在 `crate::matter` 业务层，本仓库只做存取。

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
    /// 命题6：五个决策点的委托位（默认 false = 人类保留全部决策）
    pub delegation_choose: bool,
    pub delegation_path: bool,
    pub delegation_execute: bool,
    pub delegation_verify: bool,
    pub delegation_terminate: bool,
    /// 命题2：意图原句锚点（收敛对照"我理解为X做成了Y"）
    pub intent_original: String,
    /// 命题3：分解可加性声明（all_completed | any_completed；子事项必填）
    pub additivity_decl: String,
    /// 命题2/3：信号台账（JSON 数组 [{ts,kind,detail}]）
    pub signals: String,
    /// 命题4/7：执行者自证标记（verifier 缺省=执行者时完成 → true，可信等级降级）
    pub self_verified: bool,
}

/// 投影列清单（get / scan_active / list_children 共用，顺序与 row_from 一一对应）。
const MATTER_COLS: &str = "id, title, expectation, current_state, gap_desc, acceptance_criteria,
        status, creator_id, executor_id, verifier_id, parent_id, evidence,
        death_reason, started_at, finished_at, created_at, updated_at,
        delegation_choose, delegation_path, delegation_execute,
        delegation_verify, delegation_terminate,
        intent_original, additivity_decl, signals, self_verified";

/// 新建事项（status='open'）。返回 id。
/// 业务校验（验收标准非空、验证者≠执行者、子项可加性声明）由 `crate::matter::create` 完成，本函数只落库。
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
    intent_original: &str,
    additivity_decl: &str,
) -> Result<i64> {
    let conn = db.conn();
    conn.execute(
        "INSERT INTO matters
           (title, expectation, current_state, gap_desc, acceptance_criteria,
            status, creator_id, executor_id, verifier_id, parent_id,
            intent_original, additivity_decl)
         VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            title,
            expectation,
            current_state,
            gap_desc,
            acceptance_criteria,
            creator_id,
            executor_id,
            verifier_id,
            parent_id,
            intent_original,
            additivity_decl
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 按 id 取一行（无则 None）。
pub fn get(db: &Db, id: i64) -> Result<Option<MatterRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(&format!("SELECT {MATTER_COLS} FROM matters WHERE id = ?1"))?;
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

/// 设置某个决策点的委托位（命题6）。point 白名单（choose/path/execute/verify/terminate），
/// 未知值拒绝；授权主体校验由业务层 `crate::matter::delegate` 完成。
pub fn set_delegation(db: &Db, id: i64, point: &str, allowed: bool) -> Result<()> {
    let column = match point {
        "choose" => "delegation_choose",
        "path" => "delegation_path",
        "execute" => "delegation_execute",
        "verify" => "delegation_verify",
        "terminate" => "delegation_terminate",
        other => {
            return Err(crate::error::CoreError::Validation(format!(
                "未知决策点: {other}"
            )))
        }
    };
    let sql = format!(
        "UPDATE matters SET {column} = ?2, updated_at = datetime('now') WHERE id = ?1"
    );
    let conn = db.conn();
    conn.execute(&sql, rusqlite::params![id, allowed as i64])?;
    Ok(())
}

/// 覆盖信号台账（命题2/3）。调用方负责 JSON 序列化与追加语义。
pub fn set_signals(db: &Db, id: i64, signals: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE matters SET signals = ?2, updated_at = datetime('now') WHERE id = ?1",
        rusqlite::params![id, signals],
    )?;
    Ok(())
}

/// 执行者自证标记落库（命题4/7）：self_verified=true 表示验证者缺省为执行者。
pub fn set_self_verified(db: &Db, id: i64, v: bool) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "UPDATE matters SET self_verified = ?2, updated_at = datetime('now') WHERE id = ?1",
        rusqlite::params![id, v as i64],
    )?;
    Ok(())
}

/// matter_events 事件行映射（四态死亡 + 状态转移留痕，命题4/7）。
#[derive(Debug, Clone)]
pub struct MatterEvent {
    pub id: i64,
    pub matter_id: i64,
    pub event_type: String,
    pub from_status: String,
    pub to_status: String,
    pub reason: String,
    pub actor: String,
    pub created_at: String,
}

/// 追加一条事件流水（completed/cancelled/shelved/expired/状态转移等）。
pub fn insert_event(
    db: &Db,
    matter_id: i64,
    event_type: &str,
    from_status: &str,
    to_status: &str,
    reason: &str,
    actor: &str,
) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "INSERT INTO matter_events
           (matter_id, event_type, from_status, to_status, reason, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![matter_id, event_type, from_status, to_status, reason, actor],
    )?;
    Ok(())
}

/// 取某事项的全部事件流水（按 id 升序）。
pub fn list_events(db: &Db, matter_id: i64) -> Result<Vec<MatterEvent>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, matter_id, event_type, from_status, to_status, reason, actor, created_at
         FROM matter_events WHERE matter_id = ?1 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([matter_id], |r| {
        Ok(MatterEvent {
            id: r.get(0)?,
            matter_id: r.get(1)?,
            event_type: r.get(2)?,
            from_status: r.get(3)?,
            to_status: r.get(4)?,
            reason: r.get(5)?,
            actor: r.get(6)?,
            created_at: r.get(7)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// SQLite 当前 UTC 时间（"YYYY-MM-DD HH:MM:SS"），供信号台账打时间戳。
pub fn now_utc(db: &Db) -> Result<String> {
    let conn = db.conn();
    let s: String = conn.query_row("SELECT datetime('now')", [], |r| r.get(0))?;
    Ok(s)
}

/// 扫描非终态事项（活债 = 未关闭事项），按 created_at 升序。
/// 幽灵检测：调用方以 stale_before 过滤「长期未动」的行。
pub fn scan_active(db: &Db) -> Result<Vec<MatterRow>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(&format!(
        "SELECT {MATTER_COLS}
         FROM matters
         WHERE status NOT IN ('completed', 'cancelled', 'shelved', 'expired')
         ORDER BY created_at ASC"
    ))?;
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
    let mut stmt = conn.prepare(&format!(
        "SELECT {MATTER_COLS}
         FROM matters WHERE parent_id = ?1 ORDER BY created_at ASC"
    ))?;
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
        delegation_choose: r.get(17)?,
        delegation_path: r.get(18)?,
        delegation_execute: r.get(19)?,
        delegation_verify: r.get(20)?,
        delegation_terminate: r.get(21)?,
        intent_original: r.get(22)?,
        additivity_decl: r.get(23)?,
        signals: r.get(24)?,
        self_verified: r.get(25)?,
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
            "让协作者能独立跑工具循环",
            "",
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
        assert_eq!(m.intent_original, "让协作者能独立跑工具循环");
        // 委托位默认全 false（人类保留）
        assert!(!m.delegation_choose && !m.delegation_path && !m.delegation_execute
            && !m.delegation_verify && !m.delegation_terminate);
        assert_eq!(m.signals, "[]");
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
            "",
            "",
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
            "",
            "all_completed",
        )
        .unwrap();
        let kids = list_children(&db, parent).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].id, child);
    }

    #[test]
    fn delegation_and_signals_roundtrip() {
        let db = test_db();
        let id = seed(&db);
        // 白名单外的决策点拒绝
        let err = set_delegation(&db, id, "hack", true).unwrap_err();
        assert!(err.to_string().contains("未知决策点"));
        // 授权/收回
        set_delegation(&db, id, "execute", true).unwrap();
        assert!(get(&db, id).unwrap().unwrap().delegation_execute);
        set_delegation(&db, id, "execute", false).unwrap();
        assert!(!get(&db, id).unwrap().unwrap().delegation_execute);
        // 信号台账
        set_signals(&db, id, r#"[{"ts":"2026-08-11 00:00:00","kind":"intent_drift","detail":"x"}]"#)
            .unwrap();
        assert!(get(&db, id).unwrap().unwrap().signals.contains("intent_drift"));
        assert!(!now_utc(&db).unwrap().is_empty());
    }
}
