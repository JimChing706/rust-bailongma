//! conversations 仓库：插入 / 查询 / 投递状态更新（对齐 `src/db.js`）。

use crate::db::models::{normalize_conversation_party_id, now_iso, Conversation, NewConversation};
use crate::db::Db;
use crate::error::Result;

/// 写入一条对话记录（对齐 `insertConversation`）。
///
/// 与 Node 版差异：`focus_topic` / `thread_id` 不做进程内隐式默认
/// （M2 runtime 层再接入 currentFocusTopic/currentThreadId 状态），
/// 由调用方显式传入；空值时存空串，与 JS 版写入结果一致。
pub fn insert_conversation(db: &Db, msg: &NewConversation) -> Result<i64> {
    let from_id = normalize_conversation_party_id(Some(&msg.from_id)).unwrap_or_default();
    let to_id = normalize_conversation_party_id(msg.to_id.as_deref());
    let conn = db.conn();
    conn.execute(
        r#"
        INSERT INTO conversations
          (role, from_id, to_id, content, timestamp, channel,
           external_party_id, focus_topic, open_question, thread_id, delivery_status)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        "#,
        rusqlite::params![
            msg.role,
            from_id,
            to_id,
            msg.content,
            msg.timestamp,
            msg.channel,
            msg.external_party_id,
            msg.focus_topic,
            if msg.open_question { 1 } else { 0 },
            msg.thread_id,
            msg.delivery_status,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 便捷版：带默认值的单条插入（timestamp 缺省取当前 UTC ISO）。
pub fn insert(db: &Db, role: &str, from_id: &str, content: &str) -> Result<i64> {
    insert_conversation(
        db,
        &NewConversation {
            role: role.into(),
            from_id: from_id.into(),
            to_id: None,
            content: content.into(),
            timestamp: now_iso(),
            channel: String::new(),
            external_party_id: String::new(),
            focus_topic: String::new(),
            open_question: false,
            thread_id: String::new(),
            delivery_status: String::new(),
        },
    )
}

/// 最近 N 条会话（时间倒序，对齐常见渲染需求）。
pub fn recent(db: &Db, limit: u32) -> Result<Vec<Conversation>> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM conversations ORDER BY id DESC LIMIT ?")?;
    let rows = stmt.query_map(rusqlite::params![limit], Conversation::from_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 更新投递状态（仅允许 pending/delivered/failed，其余视为清空；对齐 `updateConversationDeliveryStatus`）。
pub fn update_delivery_status(db: &Db, id: i64, status: &str) -> Result<u64> {
    let normalized = match status {
        "pending" | "delivered" | "failed" => status,
        _ => "",
    };
    let conn = db.conn();
    let n = conn.execute(
        "UPDATE conversations SET delivery_status = ?1 WHERE id = ?2",
        rusqlite::params![normalized, id],
    )?;
    Ok(n as u64)
}

/// 按 from_id 取最近会话（用于 inbound 上下文）。
pub fn recent_by_from(db: &Db, from_id: &str, limit: u32) -> Result<Vec<Conversation>> {
    let normalized = normalize_conversation_party_id(Some(from_id)).unwrap_or_default();
    let conn = db.conn();
    let mut stmt =
        conn.prepare("SELECT * FROM conversations WHERE from_id = ?1 ORDER BY id DESC LIMIT ?2")?;
    let rows = stmt.query_map(rusqlite::params![normalized, limit], |row| {
        Conversation::from_row(row)
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 与某实体的近期双向对话（`from_id = ? OR to_id = ?`，`since_ms` 毫秒时间窗内，
/// 按时间倒序取最近 limit 条再反转为时间正序；对齐 getRecentConversation）。
///
/// 时间过滤用 `strftime('%s', timestamp)` 转 unixepoch 比较，避开 `+08:00` / `Z`
/// 两种格式字符串字典序的时区陷阱（与 memories 日期窗口同一处理）。
pub fn recent_by_party(
    db: &Db,
    party: &str,
    limit: u32,
    since_ms: i64,
) -> Result<Vec<Conversation>> {
    let normalized = normalize_conversation_party_id(Some(party)).unwrap_or_default();
    let conn = db.conn();
    let since_secs = since_ms / 1000;
    let mut stmt = conn.prepare(
        "SELECT * FROM conversations
         WHERE (from_id = ?1 OR to_id = ?1)
           AND strftime('%s', timestamp) >= ?2
         ORDER BY timestamp DESC, id DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(rusqlite::params![normalized, since_secs, limit], |row| {
        Conversation::from_row(row)
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    out.reverse(); // 时间正序
    Ok(out)
}

/// 全局近期对话时间线（TICK/heartbeat 场景：无明确发送者时仍可注入最近聊天上下文；
/// 对齐 getRecentConversationTimeline）。
pub fn recent_timeline(db: &Db, limit: u32, since_ms: i64) -> Result<Vec<Conversation>> {
    let conn = db.conn();
    let since_secs = since_ms / 1000;
    let mut stmt = conn.prepare(
        "SELECT * FROM conversations
         WHERE strftime('%s', timestamp) >= ?1
         ORDER BY timestamp DESC, id DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![since_secs, limit], |row| {
        Conversation::from_row(row)
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    out.reverse(); // 时间正序
    Ok(out)
}

/// 按 id 读取单条。
pub fn get_by_id(db: &Db, id: i64) -> Result<Option<Conversation>> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM conversations WHERE id = ?1")?;
    let mut rows = stmt.query_map(rusqlite::params![id], Conversation::from_row)?;
    Ok(rows.next().transpose()?)
}

/// 未接茬的已投递出站消息（幂等心跳边界，对齐 `findUnansweredDeliveredOutbound`）。
/// 返回最近一条未被用户新消息接茬的 delivered 出站消息。
/// 注意：子查询不区分 channel —— 用户在该 from_id 下任意 channel 的最近发言都算接茬
/// （对齐 Node db.js:879，避免跨 channel 的重复投递）。
pub fn find_unanswered_delivered_outbound(
    db: &Db,
    to_id: &str,
    content: &str,
    channel: &str,
    external_party_id: &str,
) -> Result<Option<Conversation>> {
    let normalized = normalize_conversation_party_id(Some(to_id)).unwrap_or_default();
    if normalized.is_empty() || content.trim().is_empty() {
        return Ok(None);
    }
    let conn = db.conn();
    let mut stmt = conn.prepare(
        r#"
        SELECT *
        FROM conversations
        WHERE role = 'jarvis'
          AND to_id = ?1
          AND content = ?2
          AND channel = ?3
          AND external_party_id = ?4
          AND delivery_status = 'delivered'
          AND id > COALESCE((
            SELECT MAX(id) FROM conversations
            WHERE role = 'user' AND from_id = ?1
          ), 0)
        ORDER BY id DESC
        LIMIT 1
        "#,
    )?;
    let mut rows = stmt.query_map(
        rusqlite::params![normalized, content, channel, external_party_id],
        Conversation::from_row,
    )?;
    Ok(rows.next().transpose()?)
}

/// 给本轮触发判定的 user 消息回填 focus_topic / thread_id（对齐 `updateUserMessageFocusTopic`）。
///
/// 对齐 Node 语义：
/// - 用户消息在 pushMessage 阶段已写库（当时 focus_topic 为空，归属判定还没跑）；
/// - runTurn 归属判定后用 (from_id, timestamp) 精确定位该行回填，本轮焦点判断才是权威；
/// - 不做「focus_topic 必须为空」约束——即使外部预填了值，也要被本轮结果覆盖。
///
/// `thread_id` 传 `Some` 时一并回填，`None` 时只回填 focus_topic。
/// 返回受影响行数（0 = 未命中该 (from_id, timestamp) 行）。
pub fn update_user_message_focus_topic(
    db: &Db,
    from_id: &str,
    timestamp: &str,
    topic: &str,
    thread_id: Option<&str>,
) -> Result<u64> {
    let normalized = normalize_conversation_party_id(Some(from_id)).unwrap_or_default();
    if normalized.is_empty() || timestamp.is_empty() {
        return Ok(0);
    }
    let conn = db.conn();
    let changes = match thread_id {
        Some(tid) => conn.execute(
            r#"
            UPDATE conversations
            SET focus_topic = ?1, thread_id = ?2
            WHERE role = 'user' AND from_id = ?3 AND timestamp = ?4
            "#,
            rusqlite::params![topic, tid, normalized, timestamp],
        )?,
        None => conn.execute(
            r#"
            UPDATE conversations
            SET focus_topic = ?1
            WHERE role = 'user' AND from_id = ?2 AND timestamp = ?3
            "#,
            rusqlite::params![topic, normalized, timestamp],
        )?,
    };
    Ok(changes as u64)
}

/// 线索合并修正（分类器事后仲裁"其实是同一条线索"）：把 source 线索的对话过户给 target。
/// 对齐 `reassignConversationsThread`：合并而非删除——行还在，只是归属修正。
pub fn reassign_conversations_thread(
    db: &Db,
    source_thread_id: &str,
    target_thread_id: &str,
) -> Result<u64> {
    if source_thread_id.is_empty() || target_thread_id.is_empty() {
        return Ok(0);
    }
    let conn = db.conn();
    let changes = conn.execute(
        r#"
        UPDATE conversations SET thread_id = ?1 WHERE thread_id = ?2
        "#,
        rusqlite::params![target_thread_id, source_thread_id],
    )?;
    Ok(changes as u64)
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

    #[test]
    fn insert_and_recent_roundtrip() {
        let db = test_db();
        let id = insert(&db, "user", "ID:000001", "你好，小白龙").unwrap();
        let id2 = insert(&db, "jarvis", "jarvis", "你好呀").unwrap();
        assert!(id > 0);
        assert!(id2 > id);

        let recent = recent(&db, 10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, id2);
        assert_eq!(recent[0].content, "你好呀");

        let got = get_by_id(&db, id).unwrap().unwrap();
        assert_eq!(got.from_id, "ID:000001");
        assert_eq!(got.role, "user");
        assert!(!got.created_at.is_empty());
    }

    #[test]
    fn party_id_is_normalized_on_insert() {
        let db = test_db();
        insert_conversation(
            &db,
            &NewConversation {
                role: "user".into(),
                from_id: "12345".into(),
                to_id: None,
                content: "数字 ID".into(),
                timestamp: now_iso(),
                channel: "TUI".into(),
                external_party_id: String::new(),
                focus_topic: String::new(),
                open_question: false,
                thread_id: String::new(),
                delivery_status: String::new(),
            },
        )
        .unwrap();
        let rows = recent_by_from(&db, "12345", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].from_id, "ID:12345");
    }

    #[test]
    fn delivery_status_validation() {
        let db = test_db();
        let id = insert(&db, "jarvis", "jarvis", "ok").unwrap();
        update_delivery_status(&db, id, "delivered").unwrap();
        let got = get_by_id(&db, id).unwrap().unwrap();
        assert_eq!(got.delivery_status, "delivered");
        // 非法值 → 清空
        update_delivery_status(&db, id, "hacked").unwrap();
        let got = get_by_id(&db, id).unwrap().unwrap();
        assert_eq!(got.delivery_status, "");
    }

    #[test]
    fn update_user_message_focus_topic_backfills_row() {
        let db = test_db();
        // pushMessage 阶段写库：focus_topic 为空
        let id = insert_conversation(
            &db,
            &NewConversation {
                role: "user".into(),
                from_id: "ID:000001".into(),
                to_id: None,
                content: "帮我把前端轮子重做一遍".into(),
                timestamp: "2026-08-09 10:00:00".into(),
                channel: "TUI".into(),
                external_party_id: String::new(),
                focus_topic: String::new(),
                open_question: false,
                thread_id: String::new(),
                delivery_status: String::new(),
            },
        )
        .unwrap();

        // 归属判定后回填（含 thread_id）
        let changed = update_user_message_focus_topic(
            &db,
            "ID:000001",
            "2026-08-09 10:00:00",
            "重做前端轮子",
            Some("th_abc"),
        )
        .unwrap();
        assert_eq!(changed, 1);

        let got = get_by_id(&db, id).unwrap().unwrap();
        assert_eq!(got.focus_topic, "重做前端轮子");
        assert_eq!(got.thread_id, "th_abc");

        // 不存在的 (from_id, timestamp) → 0 行
        let missed =
            update_user_message_focus_topic(&db, "ID:000001", "1999-01-01 00:00:00", "x", None)
                .unwrap();
        assert_eq!(missed, 0);
    }

    #[test]
    fn update_user_message_focus_topic_without_thread_keeps_thread_id() {
        let db = test_db();
        insert_conversation(
            &db,
            &NewConversation {
                role: "user".into(),
                from_id: "ID:000001".into(),
                to_id: None,
                content: "只回填话题".into(),
                timestamp: "2026-08-09 11:00:00".into(),
                channel: "TUI".into(),
                external_party_id: String::new(),
                focus_topic: String::new(),
                open_question: false,
                thread_id: "th_old".into(),
                delivery_status: String::new(),
            },
        )
        .unwrap();
        let changed = update_user_message_focus_topic(
            &db,
            "ID:000001",
            "2026-08-09 11:00:00",
            "新话题",
            None,
        )
        .unwrap();
        assert_eq!(changed, 1);
        let rows = recent_by_from(&db, "ID:000001", 10).unwrap();
        assert_eq!(rows[0].focus_topic, "新话题");
        // thread_id 不被触碰
        assert_eq!(rows[0].thread_id, "th_old");
    }

    #[test]
    fn reassign_conversations_thread_moves_ownership() {
        let db = test_db();
        for i in 0..3 {
            insert_conversation(
                &db,
                &NewConversation {
                    role: "user".into(),
                    from_id: "ID:000001".into(),
                    to_id: None,
                    content: format!("消息 {i}"),
                    timestamp: now_iso(),
                    channel: "TUI".into(),
                    external_party_id: String::new(),
                    focus_topic: String::new(),
                    open_question: false,
                    thread_id: "th_source".into(),
                    delivery_status: String::new(),
                },
            )
            .unwrap();
        }
        let changed = reassign_conversations_thread(&db, "th_source", "th_target").unwrap();
        assert_eq!(changed, 3);

        let conn = db.conn();
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM conversations WHERE thread_id = ?1")
            .unwrap();
        let source_count: u32 = stmt
            .query_row(rusqlite::params!["th_source"], |r| r.get(0))
            .unwrap();
        assert_eq!(source_count, 0);
        let target_count: u32 = stmt
            .query_row(rusqlite::params!["th_target"], |r| r.get(0))
            .unwrap();
        assert_eq!(target_count, 3);

        // 空参数 → 不动
        assert_eq!(reassign_conversations_thread(&db, "", "x").unwrap(), 0);
    }

    #[test]
    fn find_unanswered_delivered_outbound_reads_full_row() {
        // 回归：SELECT 列必须覆盖 Conversation::from_row 的全部列，否则这里会 Err
        let db = test_db();
        insert_conversation(
            &db,
            &NewConversation {
                role: "jarvis".into(),
                from_id: "jarvis".into(),
                to_id: Some("ID:000001".into()),
                content: "你好".into(),
                timestamp: now_iso(),
                channel: "wechat".into(),
                external_party_id: "wx:abc".into(),
                focus_topic: String::new(),
                open_question: false,
                thread_id: String::new(),
                delivery_status: "delivered".into(),
            },
        )
        .unwrap();

        let found = find_unanswered_delivered_outbound(
            &db,
            "ID:000001",
            "你好",
            "wechat",
            "wx:abc",
        )
        .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().content, "你好");
    }
}
