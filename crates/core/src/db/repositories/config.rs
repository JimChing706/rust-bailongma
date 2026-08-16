//! 通用配置键值仓库（对齐 Node `getConfig` / `setConfig`，config 表）。
//!
//! 供天气（user_location）、自我进化（self_evolution_state_v1）等子系统
//! 存取小而杂的状态；Agent 委托权限的读写仍在 [`super::agents`]（带专用语义）。

use crate::db::Db;
use crate::error::Result;

const UPSERT_SQL: &str = r#"
INSERT INTO config (key, value) VALUES (?1, ?2)
ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
"#;

/// 读取单个配置项（不存在时返回 None）。
pub fn get_config(db: &Db, key: &str) -> Result<Option<String>> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT value FROM config WHERE key = ?1")?;
    let mut rows = stmt.query_map(rusqlite::params![key], |row| row.get::<_, String>(0))?;
    Ok(rows.next().transpose()?)
}

/// 写入配置项（upsert：存在则更新 value + updated_at）。
pub fn set_config(db: &Db, key: &str, value: &str) -> Result<()> {
    let conn = db.conn();
    conn.execute(UPSERT_SQL, rusqlite::params![key, value])?;
    Ok(())
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
    fn get_missing_returns_none() {
        let db = test_db();
        assert!(get_config(&db, "nope").unwrap().is_none());
    }

    #[test]
    fn set_then_get_roundtrip() {
        let db = test_db();
        set_config(&db, "user_location", "上海").unwrap();
        assert_eq!(
            get_config(&db, "user_location").unwrap().as_deref(),
            Some("上海")
        );
        // upsert 覆盖
        set_config(&db, "user_location", "北京").unwrap();
        assert_eq!(
            get_config(&db, "user_location").unwrap().as_deref(),
            Some("北京")
        );
    }
}
