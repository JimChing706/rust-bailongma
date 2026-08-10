//! ui_signals 仓库（对齐 `src/db.js` 的 insertUISignal / getUnconsumedUISignals / markUISignalsConsumed）。
//!
//! UI 意图信号：scene 等通道上行（点击 / 拖拽 / 表单提交等）落库为信号，
//! 供意识循环消费（M4+ 接入调度），未消费信号在窗口内可被拉取。

use rusqlite::params;
use serde_json::Value;

use crate::db::Db;
use crate::error::Result;

/// 一条 UI 信号（`ui_signals` 行）。
#[derive(Debug, Clone, PartialEq)]
pub struct UiSignal {
    pub id: i64,
    pub r#type: String,
    pub target: Option<String>,
    pub payload: Value,
    pub ts: i64,
}

/// 插入 UI 信号，返回自增 id（对齐 insertUISignal）。
pub fn insert_ui_signal(
    db: &Db,
    r#type: &str,
    target: Option<&str>,
    payload: &Value,
    ts: i64,
) -> Result<i64> {
    let conn = db.conn();
    conn.execute(
        "INSERT INTO ui_signals (type, target, payload, ts) VALUES (?1, ?2, ?3, ?4)",
        params![
            r#type,
            target,
            serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()),
            ts
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 窗口内的未消费信号（对齐 getUnconsumedUISignals：consumed=0 且 ts >= now-windowMs）。
pub fn get_unconsumed_ui_signals(db: &Db, window_ms: i64) -> Result<Vec<UiSignal>> {
    let since = now_epoch_ms() - window_ms;
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, type, target, payload, ts FROM ui_signals
         WHERE consumed = 0 AND ts >= ?1
         ORDER BY ts ASC",
    )?;
    let rows = stmt.query_map(params![since], row_to_ui_signal)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// 标记信号已消费（对齐 markUISignalsConsumed）。
pub fn mark_ui_signals_consumed(db: &Db, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("UPDATE ui_signals SET consumed = 1 WHERE id IN ({placeholders})");
    let mut params: Vec<rusqlite::types::Value> = ids
        .iter()
        .map(|&i| rusqlite::types::Value::Integer(i))
        .collect();
    let conn = db.conn();
    conn.execute(&sql, rusqlite::params_from_iter(params.drain(..)))?;
    Ok(())
}

/// 当前 Unix 毫秒时间戳（对齐 Date.now()）。
pub fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn row_to_ui_signal(row: &rusqlite::Row<'_>) -> rusqlite::Result<UiSignal> {
    let payload: String = row.get("payload")?;
    Ok(UiSignal {
        id: row.get("id")?,
        r#type: row.get("type")?,
        target: row.get("target")?,
        payload: serde_json::from_str(&payload)
            .unwrap_or_else(|_| Value::Object(Default::default())),
        ts: row.get("ts")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn test_db() -> Db {
        let dir = tempdir().unwrap();
        Db::open(dir.path().join("test.db")).unwrap()
    }

    #[test]
    fn insert_and_fetch_unconsumed() {
        let db = test_db();
        let now = now_epoch_ms();
        let id = insert_ui_signal(
            &db,
            "scene.intent.select",
            Some("security-confirm-1"),
            &json!({"value": "confirm"}),
            now,
        )
        .unwrap();
        assert!(id > 0);
        let sigs = get_unconsumed_ui_signals(&db, 60_000).unwrap();
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].r#type, "scene.intent.select");
        assert_eq!(sigs[0].target.as_deref(), Some("security-confirm-1"));
        assert_eq!(sigs[0].payload["value"], "confirm");
        // 消费后不可见
        mark_ui_signals_consumed(&db, &[id]).unwrap();
        assert!(get_unconsumed_ui_signals(&db, 60_000).unwrap().is_empty());
    }

    #[test]
    fn window_filters_old_signals() {
        let db = test_db();
        // ts 在 100 秒前 → 60s 窗口内查不到
        let old_ts = now_epoch_ms() - 100_000;
        insert_ui_signal(&db, "old", None, &json!({}), old_ts).unwrap();
        assert!(get_unconsumed_ui_signals(&db, 60_000).unwrap().is_empty());
        // 放宽窗口则可见
        assert_eq!(get_unconsumed_ui_signals(&db, 120_000).unwrap().len(), 1);
    }
}
