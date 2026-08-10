//! 数据行模型：serde 结构 ↔ SQLite 行映射。
//!
//! 字段命名对齐 Node 版 row 对象的 snake_case 列名（JS 版直接暴露 DB 列名）。
//! JSON 数组列（entities/tags/links/topic/signature/conclusions 等）在 Rust 侧用
//! `Vec<String>` 表达，读写时自动 JSON 编解码。

use chrono::Utc;
use rusqlite::Row;
use serde::{Deserialize, Serialize};

/// 规范化会话参与方 ID（对齐 `src/db/utils.js`）：
/// - `ID:` 前缀且后接纯数字（大小写不敏感，`/^ID:\d+$/i`）→ `ID:{数字}`
/// - 纯数字 → `ID:{数字}`
/// - 其余原样返回（含空值；`ID:abc` 这类非数字 ID 不重写，避免跨写法合并）
pub fn normalize_conversation_party_id<S: AsRef<str>>(id: Option<S>) -> Option<String> {
    let text = id.as_ref().map(|s| s.as_ref().trim().to_string());
    let text = text?;
    if text.is_empty() {
        return Some(text);
    }
    let is_id_digits = text
        .get(..3)
        .is_some_and(|head| head.eq_ignore_ascii_case("id:"))
        && text.len() > 3 // `\d+` 要求至少 1 位数字（`ID:` 本身不算）
        && text[3..].chars().all(|c| c.is_ascii_digit());
    if is_id_digits {
        return Some(format!("ID:{}", &text[3..]));
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        return Some(format!("ID:{text}"));
    }
    Some(text)
}

/// 当前 UTC ISO-8601 时间戳（毫秒精度，与 `new Date().toISOString()` 一致）。
pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// JSON 数组列 ↔ `Vec<String>`。
fn decode_json_vec(v: Option<&str>) -> Vec<String> {
    match v {
        None | Some("") | Some("[]") => Vec::new(),
        Some(raw) => serde_json::from_str(raw).unwrap_or_default(),
    }
}

/// JSON 对象/标量列（未知结构，原样保留字符串）。
fn as_text_or_empty(v: Option<&str>) -> String {
    v.unwrap_or("").to_string()
}

// ─────────────────────────────────────────────────────────────
// conversations
// ─────────────────────────────────────────────────────────────

/// 一条对话记录（`conversations` 行）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conversation {
    pub id: i64,
    pub role: String, // 'user' | 'jarvis'
    pub from_id: String,
    pub to_id: Option<String>,
    pub content: String,
    pub channel: String,
    pub external_party_id: String,
    pub focus_absorbed: bool,
    pub focus_topic: String,
    pub open_question: bool,
    pub thread_id: String,
    pub delivery_status: String,
    pub timestamp: String,
    pub created_at: String,
}

impl Conversation {
    /// 从查询行读取（列名顺序与 SELECT * 一致）。
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Conversation {
            id: row.get("id")?,
            role: row.get("role")?,
            from_id: row.get("from_id")?,
            to_id: row.get("to_id")?,
            content: row.get("content")?,
            channel: row.get("channel")?,
            external_party_id: row.get("external_party_id")?,
            focus_absorbed: row.get::<_, i64>("focus_absorbed")? != 0,
            focus_topic: row.get("focus_topic")?,
            open_question: row.get::<_, i64>("open_question")? != 0,
            thread_id: row.get("thread_id")?,
            delivery_status: row.get("delivery_status")?,
            timestamp: row.get("timestamp")?,
            created_at: row.get("created_at")?,
        })
    }
}

/// 插入 conversations 的入参（id/created_at 由 DB 生成）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewConversation {
    pub role: String,
    pub from_id: String,
    pub to_id: Option<String>,
    pub content: String,
    pub timestamp: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub external_party_id: String,
    #[serde(default)]
    pub focus_topic: String,
    #[serde(default)]
    pub open_question: bool,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub delivery_status: String,
}

// ─────────────────────────────────────────────────────────────
// memories
// ─────────────────────────────────────────────────────────────

/// 一条记忆（`memories` 行）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Memory {
    pub id: i64,
    pub event_type: String,
    pub content: String,
    pub detail: String,
    pub title: String,
    pub mem_id: Option<String>,
    pub entities: Vec<String>,
    pub concepts: Vec<String>,
    pub tags: Vec<String>,
    pub links: Vec<String>,
    pub salience: i64,
    pub source_ref: Option<String>,
    pub timestamp: String,
    pub parent_id: Option<i64>,
    /// 原始 BLOB（Float32 LE 字节序，M4 起提供 f32 包装层）。
    pub embedding: Option<Vec<u8>>,
    pub visibility: bool,
    pub hidden_at: Option<String>,
    pub merged_into: Option<String>,
    pub embedding_dim: Option<i64>,
    pub embedding_model: Option<String>,
    pub created_at: String,
}

impl Memory {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let entities_raw = row.get::<_, Option<String>>("entities")?;
        let concepts_raw = row.get::<_, Option<String>>("concepts")?;
        let tags_raw = row.get::<_, Option<String>>("tags")?;
        let links_raw = row.get::<_, Option<String>>("links")?;
        let embedding = row.get::<_, Option<Vec<u8>>>("embedding")?;
        let _ = &embedding; // 保留原始字节
        Ok(Memory {
            id: row.get("id")?,
            event_type: row.get("event_type")?,
            content: row.get("content")?,
            detail: row.get("detail")?,
            title: row.get("title")?,
            mem_id: row.get("mem_id")?,
            entities: decode_json_vec(entities_raw.as_deref()),
            concepts: decode_json_vec(concepts_raw.as_deref()),
            tags: decode_json_vec(tags_raw.as_deref()),
            links: decode_json_vec(links_raw.as_deref()),
            salience: row.get("salience")?,
            source_ref: row.get("source_ref")?,
            timestamp: row.get("timestamp")?,
            parent_id: row.get("parent_id")?,
            embedding,
            visibility: row.get::<_, i64>("visibility")? != 0,
            hidden_at: row.get("hidden_at")?,
            merged_into: row.get("merged_into")?,
            embedding_dim: row.get("embedding_dim")?,
            embedding_model: row.get("embedding_model")?,
            created_at: row.get("created_at")?,
        })
    }

    /// 构造用于 FTS 的搜索词（原样文本，由调用方决定是否加引号）。
    pub fn visible_clause() -> &'static str {
        "visibility = 1"
    }
}

/// 插入 memories 的入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMemory {
    pub event_type: String,
    pub content: String,
    pub detail: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub mem_id: Option<String>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub concepts: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub salience: i64,
    #[serde(default)]
    pub source_ref: Option<String>,
    pub timestamp: String,
    #[serde(default)]
    pub parent_id: Option<i64>,
    #[serde(default)]
    pub embedding: Option<Vec<u8>>,
    #[serde(default)]
    pub embedding_dim: Option<i64>,
    #[serde(default)]
    pub embedding_model: Option<String>,
}

// ─────────────────────────────────────────────────────────────
// threads
// ─────────────────────────────────────────────────────────────

/// 一条线索（`threads` 行）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Thread {
    pub id: String,
    pub topic: Vec<String>,
    pub signature: Vec<String>,
    pub label: String,
    pub summary: String,
    pub conclusions: Vec<String>,
    pub status: String, // 'open' | 'closed' | 'merged'
    pub created_at: String,
    pub last_event_at: String,
    pub last_event_tick: i64,
    pub hit_count: i64,
    pub last_summary_at: String,
    pub updated_at: String,
}

impl Thread {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let topic_raw = row.get::<_, Option<String>>("topic")?;
        let sig_raw = row.get::<_, Option<String>>("signature")?;
        let concl_raw = row.get::<_, Option<String>>("conclusions")?;
        Ok(Thread {
            id: row.get("id")?,
            topic: decode_json_vec(topic_raw.as_deref()),
            signature: decode_json_vec(sig_raw.as_deref()),
            label: as_text_or_empty(row.get::<_, Option<String>>("label")?.as_deref()),
            summary: as_text_or_empty(row.get::<_, Option<String>>("summary")?.as_deref()),
            conclusions: decode_json_vec(concl_raw.as_deref()),
            status: row.get("status")?,
            created_at: row.get("created_at")?,
            last_event_at: row.get("last_event_at")?,
            last_event_tick: row.get("last_event_tick")?,
            hit_count: row.get("hit_count")?,
            last_summary_at: row.get("last_summary_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    /// 生成线程 id（`th_` + 短随机串，对齐 Node 版格式；生成算法 M4 再对齐）。
    pub fn new_id() -> String {
        let raw = uuid::Uuid::new_v4().simple().to_string();
        format!("th_{}_{}", &raw[..8], &raw[8..13])
    }
}

/// 写入 threads 的入参（updated_at 由 SQLite 生成）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadPatch {
    pub id: String,
    pub topic: Vec<String>,
    pub signature: Vec<String>,
    pub label: String,
    pub summary: String,
    pub conclusions: Vec<String>,
    pub status: String,
    pub created_at: String,
    pub last_event_at: String,
    pub last_event_tick: i64,
    pub hit_count: i64,
    pub last_summary_at: String,
}

// ─────────────────────────────────────────────────────────────
// known_agents
// ─────────────────────────────────────────────────────────────

/// 一个被探测到的本地 AI Agent（`known_agents` 行；对齐 `src/agents/registry.js`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnownAgent {
    pub id: String,
    pub name: String,
    pub description: String,
    pub available: bool,
    pub version: Option<String>,
    pub invoke_type: Option<String>,
    pub invoke_cmd: Option<String>,
    /// 调用参数 JSON 数组（Node 侧 invokeArgs）。
    pub invoke_args: Vec<String>,
    pub notes: String,
    pub docs_url: Option<String>,
    pub docs_search_query: Option<String>,
    pub detected_at: String,
    pub updated_at: String,
}

impl KnownAgent {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let args_raw = row.get::<_, Option<String>>("invoke_args")?;
        Ok(KnownAgent {
            id: row.get("id")?,
            name: row.get("name")?,
            description: row.get("description")?,
            available: row.get::<_, i64>("available")? != 0,
            version: row.get("version")?,
            invoke_type: row.get("invoke_type")?,
            invoke_cmd: row.get("invoke_cmd")?,
            invoke_args: decode_json_vec(args_raw.as_deref()),
            notes: row.get("notes")?,
            docs_url: row.get("docs_url")?,
            docs_search_query: row.get("docs_search_query")?,
            detected_at: row.get("detected_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

/// 写入 known_agents 的入参（对齐 `saveAgents` 的输入对象；id 由 DB 主键决定）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewKnownAgent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub invoke_type: Option<String>,
    #[serde(default)]
    pub invoke_cmd: Option<String>,
    #[serde(default)]
    pub invoke_args: Vec<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub docs_url: Option<String>,
    #[serde(default)]
    pub docs_search_query: Option<String>,
    /// 缺省取当前 UTC ISO-8601（对齐 `new Date().toISOString()`）。
    #[serde(default)]
    pub detected_at: Option<String>,
}

// ─────────────────────────────────────────────────────────────
// 共享工具
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_party_id_matches_node() {
        assert_eq!(normalize_conversation_party_id(None::<&str>), None);
        assert_eq!(
            normalize_conversation_party_id(Some("id:000001")),
            Some("ID:000001".to_string())
        );
        assert_eq!(
            normalize_conversation_party_id(Some("ID:42")),
            Some("ID:42".to_string())
        );
        assert_eq!(
            normalize_conversation_party_id(Some("  7  ")),
            Some("ID:7".to_string())
        );
        assert_eq!(
            normalize_conversation_party_id(Some("wechat:clawbot:abc")),
            Some("wechat:clawbot:abc".to_string())
        );
        // 非数字 ID 前缀：Node `/^ID:\d+$/i` 不匹配 → 原样返回（不重写大小写）
        assert_eq!(
            normalize_conversation_party_id(Some("id:abc")),
            Some("id:abc".to_string())
        );
        assert_eq!(
            normalize_conversation_party_id(Some("ID:abc")),
            Some("ID:abc".to_string())
        );
        // `ID:` 后无数字（仅前缀）→ 原样
        assert_eq!(
            normalize_conversation_party_id(Some("ID:")),
            Some("ID:".to_string())
        );
        assert_eq!(
            normalize_conversation_party_id(Some("")),
            Some("".to_string())
        );
    }

    #[test]
    fn now_iso_has_millis_and_z() {
        let ts = now_iso();
        assert!(ts.ends_with('Z'), "应为 UTC 后缀: {ts}");
        assert!(ts.len() >= 24, "应含毫秒: {ts}");
    }

    #[test]
    fn thread_id_shape() {
        let id = Thread::new_id();
        assert!(id.starts_with("th_"), "id: {id}");
        assert_eq!(id.len(), 17); // "th_" + 8 + "_" + 5
    }
}
