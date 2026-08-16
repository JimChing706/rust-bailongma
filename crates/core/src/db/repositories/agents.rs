//! known_agents 仓库 + 委托权限 config（对齐 `src/agents/registry.js` 的 DB 半）。
//!
//! 纯数据访问与 config 读写；`detectAgents`（本地扫描，`agents/detector.js`）与
//! prompt 块生成（[`crate::memory::agent_registry`]）分属后续/纯函数层，不在此混入。

use crate::db::models::{now_iso, KnownAgent, NewKnownAgent};
use crate::db::Db;
use crate::error::Result;

const CONFIG_KEY_ASKED: &str = "agent_delegation_asked";
const CONFIG_KEY_ALLOWED: &str = "agent_delegation_allowed";

/// 读取可用 Agent（available=1，按 id 升序；对齐 `getAvailableAgents`）。
pub fn get_available_agents(db: &Db) -> Result<Vec<KnownAgent>> {
    let conn = db.conn();
    let mut stmt =
        conn.prepare("SELECT * FROM known_agents WHERE available = 1 ORDER BY id ASC")?;
    let rows = stmt.query_map([], KnownAgent::from_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 读取全部 Agent（含不可用；available 降序 + id 升序；对齐 `getAllAgents`）。
pub fn get_all_agents(db: &Db) -> Result<Vec<KnownAgent>> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM known_agents ORDER BY available DESC, id ASC")?;
    let rows = stmt.query_map([], KnownAgent::from_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 按 id 获取单个 Agent（对齐 `getAgentById`）。
pub fn get_agent_by_id(db: &Db, id: &str) -> Result<Option<KnownAgent>> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM known_agents WHERE id = ?1")?;
    let mut rows = stmt.query_map(rusqlite::params![id], KnownAgent::from_row)?;
    Ok(rows.next().transpose()?)
}

/// 批量 upsert 一批 Agent 探测结果（对齐 `saveAgents`：INSERT ... ON CONFLICT(id) UPDATE，
/// 全部字段以新值为准，`updated_at` 由 SQLite 取 UTC-Z 毫秒）。
pub fn upsert_agents(db: &Db, agents: &[NewKnownAgent]) -> Result<()> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        r#"
        INSERT INTO known_agents
          (id, name, description, available, version, invoke_type, invoke_cmd,
           invoke_args, notes, docs_url, docs_search_query, detected_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
        ON CONFLICT(id) DO UPDATE SET
          name              = excluded.name,
          description       = excluded.description,
          available         = excluded.available,
          version           = excluded.version,
          invoke_type       = excluded.invoke_type,
          invoke_cmd        = excluded.invoke_cmd,
          invoke_args       = excluded.invoke_args,
          notes             = excluded.notes,
          docs_url          = excluded.docs_url,
          docs_search_query = excluded.docs_search_query,
          detected_at       = excluded.detected_at,
          updated_at        = strftime('%Y-%m-%dT%H:%M:%fZ','now')
        "#,
    )?;
    for a in agents {
        stmt.execute(rusqlite::params![
            a.id,
            a.name,
            a.description,
            if a.available { 1 } else { 0 },
            a.version,
            a.invoke_type,
            a.invoke_cmd,
            serde_json::to_string(&a.invoke_args).unwrap_or_else(|_| "[]".into()),
            a.notes,
            a.docs_url,
            a.docs_search_query,
            a.detected_at.as_deref().unwrap_or(&now_iso()),
        ])?;
    }
    Ok(())
}

// ── 委托权限管理（config 表；对齐 registry.js 的 5 个函数） ──────────────────

fn get_config(db: &Db, key: &str) -> Result<Option<String>> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT value FROM config WHERE key = ?1")?;
    let mut rows = stmt.query_map(rusqlite::params![key], |row| row.get::<_, String>(0))?;
    Ok(rows.next().transpose()?)
}

fn set_config(db: &Db, key: &str, value: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute(
        "INSERT OR REPLACE INTO config (key, value, updated_at) VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// 是否已向主模型递过一次性 Agent 发现（对齐 `hasDelegationBeenAsked`）。
pub fn has_delegation_been_asked(db: &Db) -> Result<bool> {
    Ok(get_config(db, CONFIG_KEY_ASKED)?.as_deref() == Some("true"))
}

/// 用户是否已授权委托（对齐 `isDelegationAllowed`）。
pub fn is_delegation_allowed(db: &Db) -> Result<bool> {
    Ok(get_config(db, CONFIG_KEY_ALLOWED)?.as_deref() == Some("true"))
}

/// 标记一次性发现已递（对齐 `markDelegationAsked`）。
pub fn mark_delegation_asked(db: &Db) -> Result<()> {
    set_config(db, CONFIG_KEY_ASKED, "true")
}

/// 授予委托权限（对齐 `grantDelegation`）。
pub fn grant_delegation(db: &Db) -> Result<()> {
    set_config(db, CONFIG_KEY_ALLOWED, "true")
}

/// 撤销委托权限（对齐 `revokeDelegation`）。
pub fn revoke_delegation(db: &Db) -> Result<()> {
    set_config(db, CONFIG_KEY_ALLOWED, "false")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        open_database(path).unwrap()
    }

    fn sample_agent(id: &str, available: bool) -> NewKnownAgent {
        NewKnownAgent {
            id: id.into(),
            name: format!("agent-{id}"),
            description: format!("desc {id}"),
            available,
            version: Some("1.0.0".into()),
            invoke_type: Some("cli".into()),
            invoke_cmd: Some("claude".into()),
            invoke_args: vec!["--dangerously-skip-permissions".into()],
            notes: String::new(),
            docs_url: None,
            docs_search_query: None,
            detected_at: None,
        }
    }

    #[test]
    fn upsert_then_read_roundtrip() {
        let db = test_db();
        upsert_agents(&db, &[sample_agent("claude-code", true)]).unwrap();
        let available = get_available_agents(&db).unwrap();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].name, "agent-claude-code");
        assert_eq!(
            available[0].invoke_args,
            vec!["--dangerously-skip-permissions"]
        );

        // 第二次 upsert（同 id）→ 覆盖更新而非新增
        upsert_agents(&db, &[sample_agent("claude-code", false)]).unwrap();
        assert!(get_all_agents(&db).unwrap().len() == 1);
        assert!(get_available_agents(&db).unwrap().is_empty());
    }

    #[test]
    fn available_filter_and_order() {
        let db = test_db();
        upsert_agents(
            &db,
            &[
                sample_agent("b", true),
                sample_agent("a", false),
                sample_agent("c", true),
            ],
        )
        .unwrap();
        let available = get_available_agents(&db).unwrap();
        assert_eq!(
            available.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        let all = get_all_agents(&db).unwrap();
        // available 降序 → a(0) 在后
        assert_eq!(
            all.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
    }

    #[test]
    fn delegation_config_flags() {
        let db = test_db();
        assert!(!is_delegation_allowed(&db).unwrap());
        assert!(!has_delegation_been_asked(&db).unwrap());

        grant_delegation(&db).unwrap();
        assert!(is_delegation_allowed(&db).unwrap());

        revoke_delegation(&db).unwrap();
        assert!(!is_delegation_allowed(&db).unwrap());

        mark_delegation_asked(&db).unwrap();
        assert!(has_delegation_been_asked(&db).unwrap());
    }
}
