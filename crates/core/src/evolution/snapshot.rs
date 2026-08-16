//! 快照回滚（P4 自动迭代三件套之一）。
//!
//! 基于 rusqlite backup API（workspace 已开 `backup` feature）做**在线快照**：
//! 自动迭代应用变更前把整库备份到独立快照文件；变更失败或验证不过时
//! 从快照文件反向恢复，实现「任何自动动作可一键回滚」（评审 §5.2 修订 4）。
//!
//! 特性：
//! - 快照不锁库（SQLite online backup，源库可继续读写）
//! - 快照文件独立于主库，恢复是整库覆盖，不做增量合并（自动迭代语义要求整库一致回滚）
//! - `verify()` 提供快照可用性校验（表数），防「快照文件损坏但没发现」

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::backup::Backup;

use crate::db::Db;
use crate::error::{CoreError, Result};

/// 一次数据库快照的句柄。
#[derive(Debug, Clone)]
pub struct DbSnapshot {
    /// 快照文件路径。
    pub path: PathBuf,
    /// 快照标签（自动迭代轮次/用途），已做文件名安全化。
    pub tag: String,
    /// 创建时间（unix 毫秒）。
    pub created_ms: u64,
}

impl DbSnapshot {
    /// 创建在线快照：把 `db` 整库备份到 `dir/snap_<tag>_<ms>.db`。
    pub fn create(db: &Db, dir: &Path, tag: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let created_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| CoreError::State(format!("系统时间异常: {e}")))?
            .as_millis() as u64;
        // 标签文件名安全化：只保留字母数字与 -_，其余替换为 _
        let safe_tag: String = tag
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = dir.join(format!("snap_{safe_tag}_{created_ms}.db"));
        {
            let conn = db.conn();
            let mut dest = rusqlite::Connection::open(&path)?;
            let backup = Backup::new(&conn, &mut dest)?;
            backup.run_to_completion(64, Duration::from_millis(10), None)?;
        }
        Ok(Self {
            path,
            tag: safe_tag,
            created_ms,
        })
    }

    /// 从快照恢复：整库覆盖当前连接（回滚到快照点）。
    pub fn restore(&self, db: &Db) -> Result<()> {
        if !self.path.exists() {
            return Err(CoreError::State(format!(
                "快照文件不存在，无法回滚: {}",
                self.path.display()
            )));
        }
        let src = rusqlite::Connection::open(&self.path)?;
        let mut conn = db.conn();
        let backup = Backup::new(&src, &mut conn)?;
        backup.run_to_completion(64, Duration::from_millis(10), None)?;
        Ok(())
    }

    /// 校验快照可用性：打开快照文件并统计表数量（>0 视为可用）。
    pub fn verify(&self) -> Result<usize> {
        let conn = rusqlite::Connection::open(&self.path)?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// 删除快照文件（收敛成功后由调用方决定是否清理）。
    pub fn cleanup(&self) -> Result<()> {
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::conversations::insert;

    fn test_db() -> Db {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("blm_snap_test_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        Db::open(dir.join("test.db")).unwrap()
    }

    fn count_conversations(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn snapshot_restore_roundtrip() {
        let db = test_db();
        insert(&db, "user", "ID:000001", "初始消息").unwrap();
        let dir = std::env::temp_dir().join(format!("blm_snap_files_{}", std::process::id()));
        let snap = DbSnapshot::create(&db, &dir, "test_round").unwrap();
        assert!(snap.path.exists());
        assert!(snap.verify().unwrap() > 0);

        // 快照后写入更多数据
        insert(&db, "user", "ID:000001", "快照后的消息").unwrap();
        assert_eq!(count_conversations(&db), 2);

        // 回滚 → 回到快照点
        snap.restore(&db).unwrap();
        assert_eq!(count_conversations(&db), 1);
        let content: String = db
            .conn()
            .query_row("SELECT content FROM conversations ORDER BY id", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(content, "初始消息");

        snap.cleanup().unwrap();
        assert!(!snap.path.exists());
    }

    #[test]
    fn restore_missing_snapshot_errors() {
        let db = test_db();
        let dir = std::env::temp_dir().join(format!("blm_snap_missing_{}", std::process::id()));
        let snap = DbSnapshot::create(&db, &dir, "will_delete").unwrap();
        snap.cleanup().unwrap(); // 删除快照文件 → restore 必须报错（不能静默建空库）
        assert!(snap.restore(&db).is_err());
    }

    #[test]
    fn tag_is_filename_safe() {
        let db = test_db();
        let dir = std::env::temp_dir().join(format!("blm_snap_tag_{}", std::process::id()));
        let snap = DbSnapshot::create(&db, &dir, "迭代/轮次:1").unwrap();
        assert!(snap
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("snap_"));
        assert!(
            !snap.path.to_string_lossy().contains('/')
                || !snap.path.to_string_lossy().contains(':')
        );
        snap.cleanup().unwrap();
    }
}
