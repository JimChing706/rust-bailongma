//! threads / commitments / thread_state / focus_stack 仓库
//! （对齐 `src/db/repositories/thread-state.js`）。

use std::time::{Duration, SystemTime};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::models::{Thread, ThreadPatch};
use crate::db::Db;
use crate::error::Result;

/// 线索加载窗口：7 天（对齐 THREAD_LOAD_WINDOW_MS）。
const THREAD_LOAD_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// focus_stack 重启恢复窗口：24 小时（对齐 FOCUS_STACK_RESTORE_TTL_MS）。
const FOCUS_STACK_RESTORE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// 线索状态整态（对齐 `loadThreadState` 的返回结构）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadState {
    pub threads: Vec<Thread>,
    pub foreground_id: Option<String>,
    pub commitments: Vec<Commitment>,
}

/// 承诺条目（`commitments` 行）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Commitment {
    pub id: String,
    pub thread_id: String,
    pub text: String,
    pub status: String,
    pub channel: String,
    pub created_at: String,
    pub closed_at: Option<String>,
}

/// 焦点帧（`focus_stack` 行）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FocusFrame {
    pub topic: Vec<String>,
    pub started_at: String,
    pub started_at_tick: i64,
    pub last_seen_tick: i64,
    pub hit_count: i64,
    pub conclusions: Vec<String>,
}

fn row_to_commitment(row: &rusqlite::Row<'_>) -> rusqlite::Result<Commitment> {
    Ok(Commitment {
        id: row.get("id")?,
        thread_id: row.get("thread_id")?,
        text: row.get("text")?,
        status: row.get("status")?,
        channel: row.get("channel")?,
        created_at: row.get("created_at")?,
        closed_at: row.get("closed_at")?,
    })
}

fn row_to_focus_frame(row: &rusqlite::Row<'_>) -> rusqlite::Result<FocusFrame> {
    let topic_raw: String = row.get("topic")?;
    let concl_raw: String = row.get("conclusions")?;
    Ok(FocusFrame {
        topic: serde_json::from_str(&topic_raw).unwrap_or_default(),
        started_at: row.get("started_at")?,
        started_at_tick: row.get("started_at_tick")?,
        last_seen_tick: row.get("last_seen_tick")?,
        hit_count: row.get("hit_count")?,
        conclusions: serde_json::from_str(&concl_raw).unwrap_or_default(),
    })
}

/// 解析毫秒时间戳：兼容 ISO-8601（带时区）与历史 space-naive 格式（`YYYY-MM-DD HH:MM:SS`，无时区、按 UTC）。
fn parse_iso_ms(ts: &str) -> Option<u128> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.timestamp_millis() as u128);
    }
    if let Ok(nd) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S") {
        return Some(nd.and_utc().timestamp_millis() as u128);
    }
    None
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────
// threads 整态保存/加载
// ─────────────────────────────────────────────────────────────

/// 保存线索整态（对齐 `saveThreadState`）：
/// threads/commitments upsert + foregroundId 指针 + mergedAwayIds 关闭。
/// `merged_away_ids` 为 None 时不处理合并关闭。
pub fn save_thread_state(
    db: &Db,
    threads: &[ThreadPatch],
    commitments: &[Commitment],
    foreground_id: Option<&str>,
    merged_away_ids: Option<&[String]>,
) -> Result<()> {
    db.transaction(|tx| {
        {
            let mut upsert_thread = tx.prepare(
                r#"
                INSERT INTO threads
                  (id, topic, signature, label, summary, conclusions, status,
                   created_at, last_event_at, last_event_tick, hit_count, last_summary_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                ON CONFLICT(id) DO UPDATE SET
                  topic = excluded.topic, signature = excluded.signature, label = excluded.label,
                  summary = excluded.summary, conclusions = excluded.conclusions, status = excluded.status,
                  last_event_at = excluded.last_event_at, last_event_tick = excluded.last_event_tick,
                  hit_count = excluded.hit_count, last_summary_at = excluded.last_summary_at,
                  updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                "#,
            )?;
            for t in threads {
                let sig = if t.signature.is_empty() { &t.topic } else { &t.signature };
                upsert_thread.execute(params![
                    t.id,
                    serde_json::to_string(&t.topic).unwrap_or_else(|_| "[]".into()),
                    serde_json::to_string(sig).unwrap_or_else(|_| "[]".into()),
                    t.label,
                    t.summary,
                    serde_json::to_string(&t.conclusions).unwrap_or_else(|_| "[]".into()),
                    t.status,
                    t.created_at,
                    t.last_event_at,
                    t.last_event_tick,
                    t.hit_count,
                    t.last_summary_at,
                ])?;
            }
        }
        {
            let mut upsert_commitment = tx.prepare(
                r#"
                INSERT INTO commitments (id, thread_id, text, status, channel, created_at, closed_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(id) DO UPDATE SET
                  thread_id = excluded.thread_id, text = excluded.text, status = excluded.status,
                  channel = excluded.channel, closed_at = excluded.closed_at
                "#,
            )?;
            for c in commitments {
                upsert_commitment.execute(params![
                    c.id, c.thread_id, c.text, c.status, c.channel, c.created_at, c.closed_at
                ])?;
            }
        }
        tx.execute(
            "INSERT INTO thread_state (key, value) VALUES ('foregroundId', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![foreground_id.unwrap_or("")],
        )?;
        if let Some(ids) = merged_away_ids {
            let mut close = tx.prepare(
                "UPDATE threads SET status = 'merged', updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?1",
            )?;
            for id in ids {
                close.execute(params![id])?;
            }
        }
        Ok(())
    })
}

/// 内存线索 → 写库入参（updated_at 由 SQLite 生成 UTC-Z 毫秒，丢弃内存值）。
impl From<&Thread> for ThreadPatch {
    fn from(t: &Thread) -> Self {
        ThreadPatch {
            id: t.id.clone(),
            topic: t.topic.clone(),
            signature: t.signature.clone(),
            label: t.label.clone(),
            summary: t.summary.clone(),
            conclusions: t.conclusions.clone(),
            status: t.status.clone(),
            created_at: t.created_at.clone(),
            last_event_at: t.last_event_at.clone(),
            last_event_tick: t.last_event_tick,
            hit_count: t.hit_count,
            last_summary_at: t.last_summary_at.clone(),
        }
    }
}

/// 持久化钩子：把内存中的线索整态一键落库（threads/commitments upsert +
/// foregroundId 指针 + mergedAwayIds 软关闭）。对齐 Node 侧 `saveThreadState(state.threadState)`，
/// 供上层（主循环 / task 管理器）在状态变化后调用。
///
/// `merged_away_ids`：合并中被并掉的线索 id（db 行置 `status='merged'`）；消费后由调用方清空，
/// 避免每次保存重复 UPDATE（对齐 index.js 的 `ts.mergedAwayIds = []`）。
pub fn save_state(db: &Db, ts: &ThreadState, merged_away_ids: Option<&[String]>) -> Result<()> {
    let patches: Vec<ThreadPatch> = ts.threads.iter().map(ThreadPatch::from).collect();
    save_thread_state(
        db,
        &patches,
        &ts.commitments,
        ts.foreground_id.as_deref(),
        merged_away_ids,
    )
}

/// 加载线索整态（对齐 `loadThreadState`）：
/// - 只取 open 承诺钉住的线程 + 7 天内活跃的线程；
/// - foregroundId 指向不存在的线程时置空。
pub fn load_thread_state(db: &Db) -> Result<Option<ThreadState>> {
    let conn = db.conn();

    // 空表（无任何线索行）→ None，触发上层 focus_stack 迁移；有行但全被窗口过滤 → Some(空)。
    let has_any: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM threads)", [], |r| {
        r.get(0)
    })?;
    if !has_any {
        return Ok(None);
    }

    let commitments: Vec<Commitment> = {
        let mut stmt = conn.prepare("SELECT * FROM commitments WHERE status = 'open'")?;
        let rows = stmt.query_map([], row_to_commitment)?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r?);
        }
        v
    };

    // L17（波 3）：把「开放承诺钉住 + 7 天窗口」过滤下推到 SQL，不再全表载入内存
    // 再 Rust 过滤（审计 L17「threads 全表载入内存再过滤」）。last_event_at 为
    // 固定毫秒 Z（now_iso 写侧），与 cutoff 的 Z 下界同格式，可直接字典序比较。
    let cutoff_ms = now_ms().saturating_sub(THREAD_LOAD_WINDOW.as_millis());
    let cutoff_iso = crate::db::models::epoch_ms_to_utc_z(cutoff_ms as i64);
    let mut stmt = conn.prepare(
        "SELECT * FROM threads
         WHERE id IN (SELECT thread_id FROM commitments WHERE status = 'open')
            OR last_event_at >= ?1",
    )?;
    let rows = stmt.query_map(params![cutoff_iso], Thread::from_row)?;
    let mut threads = Vec::new();
    for r in rows {
        threads.push(r?);
    }

    let foreground_id: Option<String> = conn
        .query_row(
            "SELECT value FROM thread_state WHERE key = 'foregroundId'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok();
    let foreground_id = match foreground_id {
        Some(id) if threads.iter().any(|t: &Thread| t.id == id) => Some(id),
        _ => None,
    };

    Ok(Some(ThreadState {
        threads,
        foreground_id,
        commitments,
    }))
}

// ─────────────────────────────────────────────────────────────
// focus_stack 保存/加载
// ─────────────────────────────────────────────────────────────

/// 保存焦点栈：整栈原子替换（DELETE 全表 + 批量 INSERT，对齐 `saveFocusStack`）。
pub fn save_focus_stack(db: &Db, frames: &[FocusFrame]) -> Result<()> {
    db.transaction(|tx| {
        tx.execute("DELETE FROM focus_stack", [])?;
        let mut insert = tx.prepare(
            r#"
            INSERT INTO focus_stack
              (depth, topic, started_at, started_at_tick, last_seen_tick, hit_count, conclusions, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            "#,
        )?;
        for (i, f) in frames.iter().enumerate() {
            insert.execute(params![
                i as i64,
                serde_json::to_string(&f.topic).unwrap_or_else(|_| "[]".into()),
                f.started_at,
                f.started_at_tick,
                f.last_seen_tick,
                f.hit_count,
                serde_json::to_string(&f.conclusions).unwrap_or_else(|_| "[]".into()),
            ])?;
        }
        Ok(())
    })
}

/// 加载焦点栈；超过 24h 未更新的栈视为陈旧，清空并返回空（对齐 `loadFocusStack`）。
pub fn load_focus_stack(db: &Db) -> Result<Vec<FocusFrame>> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM focus_stack ORDER BY depth ASC")?;
    let rows = stmt.query_map([], row_to_focus_frame)?;
    let mut frames = Vec::new();
    for r in rows {
        frames.push(r?);
    }
    if frames.is_empty() {
        return Ok(frames);
    }
    // 最新 updated_at / started_at 超过 TTL → 视为陈旧，丢弃
    let newest_ms = {
        let mut stmt =
            conn.prepare("SELECT updated_at, started_at FROM focus_stack ORDER BY depth ASC")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut newest = 0u128;
        for r in rows {
            let (u, s) = r?;
            if let Some(ts) = parse_iso_ms(&u).or_else(|| parse_iso_ms(&s)) {
                newest = newest.max(ts);
            }
        }
        newest
    };
    if newest_ms > 0 && now_ms().saturating_sub(newest_ms) > FOCUS_STACK_RESTORE_TTL.as_millis() {
        conn.execute("DELETE FROM focus_stack", [])?;
        return Ok(Vec::new());
    }
    Ok(frames)
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

    /// 相对当前时间的 UTC ISO 时间戳（now − n 天，毫秒精度）。
    /// `load_thread_state` 有 7 天读窗口，硬编码日期会在时钟跨过边界后 flaky。
    fn iso_days_ago(days: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn patch(id: &str, topic: &[&str]) -> ThreadPatch {
        ThreadPatch {
            id: id.into(),
            topic: topic.iter().map(|s| s.to_string()).collect(),
            signature: Vec::new(),
            label: String::new(),
            summary: String::new(),
            conclusions: Vec::new(),
            status: "open".into(),
            created_at: iso_days_ago(2),
            last_event_at: iso_days_ago(1),
            last_event_tick: 1,
            hit_count: 1,
            last_summary_at: String::new(),
        }
    }

    /// 内存态线索（ThreadState.threads 元素；updated_at 内存里不维护）。
    fn thread(id: &str, topic: &[&str]) -> Thread {
        let p = patch(id, topic);
        Thread {
            id: p.id,
            topic: p.topic,
            signature: p.signature,
            label: p.label,
            summary: p.summary,
            conclusions: p.conclusions,
            status: p.status,
            created_at: p.created_at,
            last_event_at: p.last_event_at,
            last_event_tick: p.last_event_tick,
            hit_count: p.hit_count,
            last_summary_at: p.last_summary_at,
            updated_at: String::new(),
        }
    }

    #[test]
    fn thread_state_roundtrip() {
        let db = test_db();
        let t = patch("th_abc", &["测试", "线程"]);
        let c = Commitment {
            id: "cm_1".into(),
            thread_id: "th_abc".into(),
            text: "好的我去做".into(),
            status: "open".into(),
            channel: "TUI".into(),
            created_at: "2026-08-08T00:00:00.000Z".into(),
            closed_at: None,
        };
        save_thread_state(&db, &[t], &[c], Some("th_abc"), None).unwrap();

        let st = load_thread_state(&db).unwrap().unwrap();
        assert_eq!(st.threads.len(), 1);
        assert_eq!(
            st.threads[0].topic,
            vec!["测试".to_string(), "线程".to_string()]
        );
        // signature 缺省回落到 topic
        assert_eq!(
            st.threads[0].signature,
            vec!["测试".to_string(), "线程".to_string()]
        );
        assert_eq!(st.foreground_id.as_deref(), Some("th_abc"));
        assert_eq!(st.commitments.len(), 1);
        assert_eq!(st.commitments[0].text, "好的我去做");

        // 关闭承诺 + 指针失效
        save_thread_state(&db, &[], &[], None, None).unwrap();
        let st = load_thread_state(&db).unwrap().unwrap();
        assert!(st.foreground_id.is_none());
    }

    #[test]
    fn save_state_hook_persists_memory_state() {
        let db = test_db();
        // 首次保存：th_hook + th_gone 都在库
        let t = thread("th_hook", &["钩子", "保存"]);
        let gone = thread("th_gone", &["并入", "前身"]);
        let c = Commitment {
            id: "cm_hook".into(),
            thread_id: "th_hook".into(),
            text: "持久化测试".into(),
            status: "open".into(),
            channel: "wechat".into(),
            created_at: "2026-08-08T00:00:00.000Z".into(),
            closed_at: None,
        };
        let ts = ThreadState {
            threads: vec![t, gone],
            foreground_id: Some("th_hook".into()),
            commitments: vec![c],
        };
        // 一键保存钩子
        save_state(&db, &ts, None).unwrap();
        let st = load_thread_state(&db).unwrap().unwrap();
        assert_eq!(st.threads.len(), 2);
        assert_eq!(st.foreground_id.as_deref(), Some("th_hook"));
        assert_eq!(st.commitments.len(), 1);

        // merged_away_ids：合并后内存只剩 th_hook，db 里 th_gone 软关闭为 merged
        let slim = ThreadState {
            threads: vec![thread("th_hook", &["钩子", "保存"])],
            foreground_id: Some("th_hook".into()),
            commitments: Vec::new(),
        };
        save_state(&db, &slim, Some(&["th_gone".to_string()])).unwrap();
        let status: String = db
            .conn()
            .query_row("SELECT status FROM threads WHERE id = 'th_gone'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "merged");
        // 重复保存不重复关闭（merged_away 已消费），行状态保持不变
        save_state(&db, &slim, None).unwrap();
        let status: String = db
            .conn()
            .query_row("SELECT status FROM threads WHERE id = 'th_gone'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "merged");
    }

    #[test]
    fn merged_away_closes_thread() {
        let db = test_db();
        let mut t2 = patch("th_b", &["乙"]);
        // th_b 超过 7 天窗口 → load 时会被过滤（对齐 Node 版读时选择）
        t2.last_event_at = iso_days_ago(8);
        save_thread_state(&db, &[patch("th_a", &["甲"]), t2], &[], None, None).unwrap();
        save_thread_state(
            &db,
            &[patch("th_a", &["甲"])],
            &[],
            None,
            Some(&["th_b".to_string()]),
        )
        .unwrap();
        let st = load_thread_state(&db).unwrap().unwrap();
        assert_eq!(st.threads.len(), 1);
        assert_eq!(st.threads[0].id, "th_a");
        // 已关闭的线程仍留在仓库（软关闭），但已退出读窗口
        let conn = db.conn();
        let status: String = conn
            .query_row("SELECT status FROM threads WHERE id = 'th_b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "merged");
    }

    #[test]
    fn focus_stack_roundtrip_and_stale_drop() {
        let db = test_db();
        let f = FocusFrame {
            topic: vec!["股票".into(), "行情".into()],
            started_at: crate::db::models::now_iso(),
            started_at_tick: 0,
            last_seen_tick: 3,
            hit_count: 2,
            conclusions: vec!["已回填".into()],
        };
        save_focus_stack(&db, &[f]).unwrap();
        let loaded = load_focus_stack(&db).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].topic,
            vec!["股票".to_string(), "行情".to_string()]
        );
        assert_eq!(loaded[0].conclusions, vec!["已回填".to_string()]);

        // 陈旧栈（把 updated_at 改到 2 天前）→ 加载为空且被清空
        db.conn()
            .execute(
                "UPDATE focus_stack SET updated_at = datetime('now', '-2 days')",
                [],
            )
            .unwrap();
        assert!(load_focus_stack(&db).unwrap().is_empty());
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM focus_stack", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
