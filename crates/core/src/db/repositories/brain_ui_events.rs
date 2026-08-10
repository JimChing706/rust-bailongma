//! brain_ui_events 仓库（对齐 `src/db/repositories/brain-ui-events.js`）。
//!
//! 观测历史：SSE 事件在 L1/L2 路径上的持久化记录，供 brain-ui 回放。
//! 写库是 best-effort —— 失败绝不影响事件总线与意识循环。
//! 与 Node 版一致：写入前对 payload 做脱敏（敏感 key/secret 值替换）+ 有界（超限删除）。

use rusqlite::params;
use serde_json::Value;

use crate::db::Db;
use crate::error::Result;

/// 事件表保留上限（对齐 EVENT_LIMIT=800）
const EVENT_LIMIT: i64 = 800;
/// 序列化后的 payload 上限（对齐 PAYLOAD_LIMIT=6000）
const PAYLOAD_LIMIT: usize = 6000;
/// 敏感字段 key（对齐 SENSITIVE_KEY_RE，小写比较）
const SENSITIVE_KEY_PARTS: &[&str] = &[
    "apikey",
    "api_key",
    "api-key",
    "accesskey",
    "access_key",
    "access-key",
    "secret",
    "token",
    "password",
    "authorization",
    "bearer",
];

/// 一条 brain-ui 观测事件（`brain_ui_events` 行）。
#[derive(Debug, Clone, PartialEq)]
pub struct BrainUiEvent {
    pub id: i64,
    pub timestamp: String,
    pub path: String,
    pub event_type: String,
    pub payload: Value,
}

/// key 是否命中敏感名单（对齐 SENSITIVE_KEY_RE 的 i 标志：不区分大小写）。
fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEY_PARTS.iter().any(|p| lower.contains(p))
}

/// 替换 `sk-/ak-/rk-/pk-` 开头、12..180 位 `[A-Za-z0-9_.-]` 的密钥值为 `[redacted]`。
/// 对齐 SECRET_VALUE_RE（大小写敏感，无 i 标志）。
fn redact_secret_values(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let prefix = match bytes[i..].get(..3) {
            Some([c, b'k', b'-']) if *c == b's' || *c == b'a' || *c == b'r' || *c == b'p' => {
                (*c, 3)
            }
            _ => (0, 0),
        };
        if prefix.1 == 0 {
            // 逐字符拷贝（保持 UTF-8 正确：非 ASCII 时按完整字符复制）
            let ch_len = utf8_char_len(bytes[i]);
            out.push_str(&s[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        // 扫描后续 [A-Za-z0-9_.-] 数量（12..180）
        let mut j = i + 3;
        let mut count = 0usize;
        while j < bytes.len() && is_secret_char(bytes[j]) {
            j += 1;
            count += 1;
        }
        if (12..=180).contains(&count) {
            out.push_str("[redacted]");
            i = j;
        } else {
            out.push_str(&s[i..i + 3]);
            i += 3;
        }
    }
    out
}

fn is_secret_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
}

/// 完整字符长度（UTF-8 首字节推断）。
fn utf8_char_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// 递归脱敏（对齐 scrubValue）：
/// - 字符串：secret 值替换 + 截断 1500
/// - 数字/布尔/null：原样
/// - 深度 ≥4：`[nested]`
/// - 数组：前 20 项递归；对象：前 32 个键递归，敏感键值替换为 `[redacted]`
/// - 其它类型：字符串化截断 300
fn scrub_value(value: &Value, depth: usize) -> Value {
    match value {
        Value::String(s) => Value::String(redact_secret_values(s).chars().take(1500).collect()),
        Value::Number(_) | Value::Bool(_) | Value::Null => value.clone(),
        _ if depth >= 4 => Value::String("[nested]".into()),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(|v| scrub_value(v, depth + 1))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map.iter().take(32) {
                let val = if is_sensitive_key(k) {
                    Value::String("[redacted]".into())
                } else {
                    scrub_value(v, depth + 1)
                };
                out.insert(k.clone(), val);
            }
            Value::Object(out)
        }
    }
}

/// 取值为字符串内容（对齐 Node `String(x || '')`）：String 取原串，其它类型字符串化。
fn as_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 脱敏后序列化；超长时压缩为 {name, ok, args, result, error, mode}，
/// 仍超长再降级 {name, ok, result}（对齐 serializePayload）。
fn serialize_payload(payload: &Value) -> String {
    let scrubbed = scrub_value(payload, 0);
    let json = serde_json::to_string(&scrubbed).unwrap_or_else(|_| "{}".into());
    if json.len() <= PAYLOAD_LIMIT {
        return json;
    }
    let get = |k: &str| scrubbed.get(k).cloned().unwrap_or(Value::Null);
    let truncate = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let compact = serde_json::json!({
        "name": get("name"),
        "ok": get("ok"),
        "args": scrub_value(&get("args"), 3),
        "result": truncate(&as_string(&get("result")), 1800),
        "error": truncate(&as_string(&get("error")), 500),
        "mode": get("mode"),
    });
    let json = serde_json::to_string(&compact).unwrap_or_else(|_| "{}".into());
    if json.len() <= PAYLOAD_LIMIT {
        return json;
    }
    serde_json::json!({
        "name": get("name"),
        "ok": get("ok"),
        "result": truncate(&as_string(&get("result")), 1000),
    })
    .to_string()
}

/// 写入观测事件（对齐 insertBrainUiEvent）。失败静默忽略。
/// - type 截断 80 字符，空 type 不写
/// - payload 脱敏 + 超长压缩
/// - 每次写入后清理超限旧行（有界，EVENT_LIMIT=800）
pub fn insert_brain_ui_event(
    db: &Db,
    timestamp: &str,
    path: &str,
    event_type: &str,
    payload: &Value,
) {
    let r#type: String = event_type.chars().take(80).collect();
    if r#type.is_empty() {
        return;
    }
    let normalized_path = if path == "l1" { "l1" } else { "l2" };
    let payload_json = serialize_payload(payload);
    let res = (|| -> Result<()> {
        let conn = db.conn();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO brain_ui_events (timestamp, path, event_type, payload_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![timestamp, normalized_path, r#type, payload_json],
        )?;
        // 有界：删除超出 EVENT_LIMIT 的最旧行（对齐 Node DELETE ... OFFSET ?）
        tx.execute(
            r#"
            DELETE FROM brain_ui_events
            WHERE id <= COALESCE(
              (SELECT id FROM brain_ui_events ORDER BY id DESC LIMIT 1 OFFSET ?1), 0)
            "#,
            params![EVENT_LIMIT],
        )?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(e) = res {
        tracing::warn!("[brain-ui-history] persist failed (best-effort): {e}");
    }
}

/// 取观测历史（对齐 getBrainUiEventHistory：path=all|l1|l2，默认 l2）。
/// 按 path 过滤 + 降序取 limit 条，返回升序（Node 版内部分组）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BrainUiHistory {
    pub events: Vec<BrainUiEvent>,
}

pub fn get_brain_ui_event_history(db: &Db, path: &str, limit: usize) -> Result<BrainUiHistory> {
    let path_clause = match path {
        "all" => "1=1",
        "l1" => "path = 'l1'",
        _ => "path = 'l2'",
    };
    let sql = format!(
        "SELECT id, timestamp, path, event_type, payload_json FROM brain_ui_events
         WHERE {path_clause}
         ORDER BY id DESC LIMIT ?1"
    );
    let conn = db.conn();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![limit as i64])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        let payload: String = row.get("payload_json")?;
        events.push(BrainUiEvent {
            id: row.get("id")?,
            timestamp: row.get("timestamp")?,
            path: row.get("path")?,
            event_type: row.get("event_type")?,
            payload: serde_json::from_str(&payload)
                .unwrap_or_else(|_| Value::Object(Default::default())),
        });
    }
    // 升序返回（时间正序），与 Node 一致
    events.reverse();
    Ok(BrainUiHistory { events })
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
    fn insert_and_history_by_path() {
        let db = test_db();
        insert_brain_ui_event(
            &db,
            "2026-08-09T00:00:00Z",
            "l1",
            "message_received",
            &json!({"id": 1}),
        );
        insert_brain_ui_event(&db, "2026-08-09T00:00:01Z", "l2", "tick", &json!({"n": 1}));
        insert_brain_ui_event(
            &db,
            "2026-08-09T00:00:02Z",
            "l2",
            "response",
            &json!({"t": "ok"}),
        );

        let all = get_brain_ui_event_history(&db, "all", 100).unwrap();
        assert_eq!(all.events.len(), 3);
        // 升序
        assert_eq!(all.events[0].event_type, "message_received");
        assert_eq!(all.events[2].event_type, "response");

        let l2 = get_brain_ui_event_history(&db, "l2", 100).unwrap();
        assert_eq!(l2.events.len(), 2);
        assert!(l2.events.iter().all(|e| e.path == "l2"));

        let l1 = get_brain_ui_event_history(&db, "l1", 100).unwrap();
        assert_eq!(l1.events.len(), 1);
        assert_eq!(l1.events[0].payload["id"], 1);
    }

    #[test]
    fn history_respects_limit() {
        let db = test_db();
        for i in 0..5 {
            insert_brain_ui_event(&db, "2026-08-09T00:00:00Z", "l2", "tick", &json!({"i": i}));
        }
        let limited = get_brain_ui_event_history(&db, "all", 2).unwrap();
        // 取最新 2 条，升序 → 第 3、4 条
        assert_eq!(limited.events.len(), 2);
        assert_eq!(limited.events[0].payload["i"], 3);
        assert_eq!(limited.events[1].payload["i"], 4);
    }

    #[test]
    fn payload_scrubs_sensitive_fields() {
        let db = test_db();
        insert_brain_ui_event(
            &db,
            "2026-08-09T00:00:00Z",
            "l2",
            "tool_call",
            &json!({
                "name": "web_search",
                "api_key": "sk-abcdef1234567890abcdef",
                "args": { "token": "12345", "q": "ok" },
                "password": "hunter2",
                "safe": "保留字段",
                "nested": { "deep": { "deeper": { "deepest": { "inner": "x" } } } }
            }),
        );
        let all = get_brain_ui_event_history(&db, "all", 10).unwrap();
        let ev = &all.events[0];
        // 敏感键 → [redacted]
        assert_eq!(ev.payload["api_key"], "[redacted]");
        assert_eq!(ev.payload["password"], "[redacted]");
        assert_eq!(ev.payload["args"]["token"], "[redacted]");
        // secret 值替换（非敏感键下）
        assert_eq!(ev.payload["args"]["q"], "ok");
        assert_eq!(ev.payload["safe"], "保留字段");
        // 深度 ≥4 → [nested]
        assert_eq!(
            ev.payload["nested"]["deep"]["deeper"]["deepest"],
            "[nested]"
        );
        // 明文字符串里的 sk- 密钥值也被替换
        insert_brain_ui_event(
            &db,
            "2026-08-09T00:00:01Z",
            "l2",
            "response",
            &json!({ "result": "key is sk-abcdef1234567890abcdef now" }),
        );
        let all = get_brain_ui_event_history(&db, "all", 10).unwrap();
        let resp = all
            .events
            .iter()
            .find(|e| e.event_type == "response")
            .expect("response 事件存在");
        assert!(!resp.payload["result"]
            .as_str()
            .unwrap()
            .contains("sk-abcdef"));
        assert!(resp.payload["result"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));
    }

    #[test]
    fn event_table_is_bounded_and_type_truncated() {
        let db = test_db();
        // type 超长 → 截断 80
        let long_type = "x".repeat(200);
        insert_brain_ui_event(&db, "2026-08-09T00:00:00Z", "l1", &long_type, &json!({}));
        let all = get_brain_ui_event_history(&db, "all", 10).unwrap();
        assert_eq!(all.events[0].event_type.len(), 80);

        // 空 type 不写
        insert_brain_ui_event(&db, "2026-08-09T00:00:00Z", "l2", "", &json!({}));
        assert_eq!(
            get_brain_ui_event_history(&db, "all", 100)
                .unwrap()
                .events
                .len(),
            1
        );

        // path 非 l1 → 归一到 l2
        insert_brain_ui_event(&db, "2026-08-09T00:00:00Z", "l3", "x", &json!({}));
        let l2 = get_brain_ui_event_history(&db, "l2", 100).unwrap();
        assert!(l2.events.iter().all(|e| e.path == "l2"));

        // 有界：写入超过 EVENT_LIMIT 后只保留最新 800 条
        let db2 = test_db();
        for i in 0..(EVENT_LIMIT + 25) {
            insert_brain_ui_event(&db2, "2026-08-09T00:00:00Z", "l2", "tick", &json!({"i": i}));
        }
        let all = get_brain_ui_event_history(&db2, "all", 2000).unwrap();
        assert_eq!(all.events.len(), EVENT_LIMIT as usize);
        // 保留的是最新一批（id 升序首条 = i=25 对应行）
        assert_eq!(all.events[0].payload["i"], 25);
        assert_eq!(
            all.events[all.events.len() - 1].payload["i"],
            EVENT_LIMIT + 24
        );
    }

    #[test]
    fn oversized_payload_is_compacted() {
        let db = test_db();
        let big = "y".repeat(10_000);
        insert_brain_ui_event(
            &db,
            "2026-08-09T00:00:00Z",
            "l2",
            "tool_call",
            &json!({
                "name": "exec_command",
                "ok": false,
                "args": { "cmd": &big },
                "result": &big,
                "error": "boom",
                "mode": "cmd"
            }),
        );
        let all = get_brain_ui_event_history(&db, "all", 10).unwrap();
        let ev = &all.events[0];
        // 压缩路径保留 name/ok/result/error；result 源串已被 scrub 截到 1500
        //（对齐 Node：scrubValue 截 1500 → slice(0, 1800) 无效果）
        assert_eq!(ev.payload["name"], "exec_command");
        assert_eq!(ev.payload["ok"], false);
        assert_eq!(ev.payload["result"].as_str().unwrap().len(), 1500);
        assert_eq!(ev.payload["error"], "boom");
        assert!(ev.payload["mode"].is_null() || ev.payload["mode"].as_str().is_some());
    }
}
