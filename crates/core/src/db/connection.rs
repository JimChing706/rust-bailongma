//! SQLite 连接管理。
//!
//! 对齐 Node 版 `src/db/connection.js`：
//! - `journal_mode = WAL`（读并发 + 崩溃安全）
//! - 打开时执行幂等 schema 迁移
//! - 追加 `busy_timeout` 与 `foreign_keys`（better-sqlite3 默认 busy_timeout=5000ms、外键开）

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::error::Result;

use super::schema;

/// SQLite 连接包装：`rusqlite::Connection` 不是 `Sync`，用 `Mutex` 串行化访问。
/// 业务上写操作少、都是短事务，单连接 + 互斥足够（对齐 better-sqlite3 的单连接模型）。
/// `Arc` 使 `Db` 可 `Clone`（供 axum `State` 与各仓库共享同一连接）。
#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl Db {
    /// 打开（或创建）数据库并执行幂等迁移。
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        open_database(path)
    }

    /// 数据库文件路径（诊断用）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 获取连接（内部串行化）。仓库层通过此方法执行 SQL。
    /// Mutex 中毒（持有锁的线程 panic）时恢复锁内数据继续使用，而不是连锁 panic：
    /// SQLite 连接本身在 panic 后通常仍可用，让单个线程的 panic 不至于拖垮整个进程/请求。
    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 在事务中执行闭包（自动 ROLLBACK on error / COMMIT on success）。
    pub fn transaction<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    {
        let mut guard = self.conn();
        let tx = guard.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

/// 打开数据库：设置 PRAGMA → 幂等迁移。
pub fn open_database<P: AsRef<Path>>(path: P) -> Result<Db> {
    let path = path.as_ref().to_path_buf();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&path)?;
    // 与 better-sqlite3 默认值对齐：busy_timeout=5000ms、外键约束开启、WAL。
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    schema::initialize(&conn)?;
    Ok(Db {
        conn: Arc::new(Mutex::new(conn)),
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_wal_mode_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_database(dir.path().join("test.db")).unwrap();
        let mode: String = db
            .conn()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        assert!(db.path().exists());
    }

    #[test]
    fn reopen_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("test.db");
        {
            let db = open_database(&p).unwrap();
            db.conn()
                .execute("INSERT INTO config (key, value) VALUES ('k', 'v')", [])
                .unwrap();
        }
        // 关闭后重开：迁移幂等 + 数据仍在
        let db2 = open_database(&p).unwrap();
        let v: String = db2
            .conn()
            .query_row("SELECT value FROM config WHERE key = 'k'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "v");
    }
}
