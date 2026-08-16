//! 自我进化（对齐 `src/memory/self-evolution.js`）。
//!
//! 自我进化循环把可复用的流程（procedure）、约束（constraint）、失败教训
//! （failure_lesson）与策略（policy）记忆沉淀为长期策略记忆；本模块只做
//! 「记录 + 快照 + prompt 渲染」，不直接改写代码或权限。
//!
//! 状态存 config 表 `self_evolution_state_v1`（JSON，对齐 Node `STATE_KEY`）：
//! `{ version, enabled, total_events, learned_count, last_at, recent[<=24] }`。
//!
//! 未迁：`emitEvent('self_evolution', …)`（事件总线属后续里程碑）；
//! 其余语义（normalize / 去重 / 截断 / 7 天窗口 / 渲染文案）逐字对齐 Node。

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::db::models::Memory;
use crate::db::repositories::config::{get_config, set_config};
use crate::db::repositories::memories::get_by_mem_id;
use crate::db::Db;
use crate::error::Result;

pub const STATE_KEY: &str = "self_evolution_state_v1";
const STATE_VERSION: i64 = 1;
const MAX_RECENT: usize = 24;
/// prompt 渲染里只展示最近 7 天内的更新（对齐 PROMPT_MAX_AGE_MS）。
const PROMPT_MAX_AGE_DAYS: i64 = 7;

const ACTIONABLE_TAGS: &[&str] = &[
    "kind:procedure",
    "kind:constraint",
    "kind:failure_lesson",
    "kind:policy",
];

/// 记忆 mem_id 前缀命中即视为可进化的策略记忆（对齐 ACTIONABLE_MEM_ID_RE）。
fn actionable_mem_id(mem_id: &str) -> Option<String> {
    let lower = mem_id.to_lowercase();
    for prefix in ["procedure", "constraint", "policy", "lesson", "rule"] {
        if lower.starts_with(&format!("{prefix}_")) {
            return Some(prefix.to_string());
        }
    }
    None
}

// ── 状态模型（对齐 Node defaultState / normalizeState / saveState） ─────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionEntry {
    pub mem_id: String,
    pub kind: String,
    pub action: String,
    pub title: String,
    pub content: String,
    pub salience: i64,
    pub tags: Vec<String>,
    pub learned_at: String,
}

impl Default for EvolutionEntry {
    fn default() -> Self {
        Self {
            mem_id: String::new(),
            kind: "policy".to_string(),
            action: "observed".to_string(),
            title: String::new(),
            content: String::new(),
            salience: 3,
            tags: Vec::new(),
            learned_at: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionState {
    pub version: i64,
    pub enabled: bool,
    pub total_events: i64,
    pub learned_count: i64,
    pub last_at: Option<String>,
    pub recent: Vec<EvolutionEntry>,
}

impl Default for EvolutionState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            enabled: true,
            total_events: 0,
            learned_count: 0,
            last_at: None,
            recent: Vec::new(),
        }
    }
}

/// 读取并规整状态（坏数据 / 缺字段回落默认；对齐 normalizeState）。
pub fn get_self_evolution_state(db: &Db) -> EvolutionState {
    let raw = match get_config(db, STATE_KEY) {
        Ok(Some(v)) => v,
        _ => return EvolutionState::default(),
    };
    normalize_state(&raw)
}

fn normalize_state(raw: &str) -> EvolutionState {
    let parsed: Option<serde_json::Value> = serde_json::from_str(raw).ok();
    let v = match parsed {
        Some(ref v) if v.is_object() => v.clone(),
        _ => return EvolutionState::default(),
    };
    let recent: Vec<EvolutionEntry> = v
        .get("recent")
        .and_then(|r| serde_json::from_value::<Vec<EvolutionEntry>>(r.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|e: &EvolutionEntry| !e.mem_id.is_empty())
        .take(MAX_RECENT)
        .collect();
    EvolutionState {
        version: STATE_VERSION,
        enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
        total_events: v
            .get("total_events")
            .and_then(|x| x.as_i64())
            .unwrap_or(0)
            .max(0),
        learned_count: v
            .get("learned_count")
            .and_then(|x| x.as_i64())
            .unwrap_or(0)
            .max(0),
        last_at: v
            .get("last_at")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        recent,
    }
}

fn save_state(db: &Db, state: &EvolutionState) -> EvolutionState {
    let mut normalized =
        normalize_state(&serde_json::to_string(state).unwrap_or_else(|_| "{}".to_string()));
    normalized.recent.truncate(MAX_RECENT);
    let _ = set_config(
        db,
        STATE_KEY,
        &serde_json::to_string(&normalized).unwrap_or_default(),
    );
    normalized
}

/// 快照（对齐 getSelfEvolutionSnapshot；recent 截取 max_recent）。
pub fn get_self_evolution_snapshot(db: &Db, max_recent: usize) -> EvolutionState {
    let mut state = get_self_evolution_state(db);
    let n = max_recent.clamp(0, MAX_RECENT);
    state.recent.truncate(n);
    state
}

/// 重置为默认状态（对齐 resetSelfEvolutionState）。
pub fn reset_self_evolution_state(db: &Db) -> EvolutionState {
    save_state(db, &EvolutionState::default())
}

// ── 判定与记录（对齐 isSelfEvolutionMemory / memoryToEntry / record…） ──────

fn truncate(text: &str, max: usize) -> String {
    let value: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() > max {
        let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        value
    }
}

/// 记忆是否属于可进化的策略记忆（kind 标签 / self_constraint 事件 / mem_id 前缀）。
pub fn is_self_evolution_memory(m: &Memory) -> bool {
    if m.tags.iter().any(|t| ACTIONABLE_TAGS.contains(&t.as_str())) {
        return true;
    }
    if m.event_type == "self_constraint" {
        return true;
    }
    m.mem_id.as_deref().and_then(actionable_mem_id).is_some()
}

fn tag_kind(tags: &[String]) -> Option<String> {
    tags.iter()
        .find(|t| t.starts_with("kind:"))
        .map(|t| t.trim_start_matches("kind:").to_string())
}

fn memory_to_entry(m: &Memory) -> EvolutionEntry {
    let kind = tag_kind(&m.tags)
        .pipe_or(|| {
            if m.event_type == "self_constraint" {
                Some("constraint".to_string())
            } else {
                None
            }
        })
        .pipe_or(|| m.mem_id.as_deref().and_then(actionable_mem_id))
        .unwrap_or_else(|| "policy".to_string());
    let mem_id = m.mem_id.clone().unwrap_or_else(|| format!("row:{}", m.id));
    EvolutionEntry {
        mem_id,
        kind,
        action: "observed".to_string(),
        title: truncate(
            if !m.title.is_empty() {
                &m.title
            } else {
                &m.content
            },
            96,
        ),
        content: truncate(&m.content, 240),
        salience: m.salience,
        tags: m.tags.clone(),
        learned_at: Utc::now().to_rfc3339(),
    }
}

/// 简化链式 Option 组合（等价 Node `a || b || c` 的取第一个 Some）。
trait PipeOr<T> {
    fn pipe_or<F: FnOnce() -> Option<T>>(self, f: F) -> Option<T>;
}

impl<T> PipeOr<T> for Option<T> {
    fn pipe_or<F: FnOnce() -> Option<T>>(self, f: F) -> Option<T> {
        match self {
            Some(v) => Some(v),
            None => f(),
        }
    }
}

/// 从（检索/事件来源的）记忆列表记录进化项；返回本轮新学习的条目。
/// 对齐 recordSelfEvolutionFromMemories：按 mem_id 去重、查库补全、recent 合并去重、
/// total_events 累加、last_at 刷新。emitEvent 未迁（事件总线后续里程碑）。
pub fn record_self_evolution_from_memories(
    db: &Db,
    memories: &[Memory],
) -> Result<Vec<EvolutionEntry>> {
    if memories.is_empty() {
        return Ok(Vec::new());
    }
    let mut state = get_self_evolution_state(db);
    if !state.enabled {
        return Ok(Vec::new());
    }

    let mut learned: Vec<EvolutionEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in memories {
        // 对齐 Node：无 mem_id 的条目直接跳过（没有可引用的标识）
        let Some(mem_id) = item.mem_id.clone() else {
            continue;
        };
        if !seen.insert(mem_id.clone()) {
            continue;
        }
        // 查库补全（对齐 getMemoryByMemId 兜底；检索对象通常已完整，查不到回退原对象）
        let memory = match get_by_mem_id(db, &mem_id)? {
            Some(m) => m,
            None => item.clone(),
        };
        if !is_self_evolution_memory(&memory) {
            continue;
        }
        learned.push(memory_to_entry(&memory));
    }

    if learned.is_empty() {
        return Ok(Vec::new());
    }

    // 合并旧 recent（按 mem_id 去重，新条目优先），按 learned_at 倒序，截 24
    let mut by_id: std::collections::HashMap<String, EvolutionEntry> =
        std::collections::HashMap::new();
    for e in learned.iter() {
        by_id.insert(e.mem_id.clone(), e.clone());
    }
    for e in state.recent.iter() {
        by_id.entry(e.mem_id.clone()).or_insert_with(|| e.clone());
    }
    let mut next_recent: Vec<EvolutionEntry> = by_id.into_values().collect();
    next_recent.sort_by(|a, b| b.learned_at.cmp(&a.learned_at));
    next_recent.truncate(MAX_RECENT);

    state.total_events += learned.len() as i64;
    state.learned_count = next_recent.len() as i64;
    state.last_at = Some(Utc::now().to_rfc3339());
    state.recent = next_recent;
    save_state(db, &state);

    Ok(learned)
}

// ── prompt 渲染（对齐 formatSelfEvolutionForPrompt） ────────────────────────

/// 渲染自我进化上下文文本；无更新 / 禁用 / 全超窗 → 空串。
/// `max_recent` 对齐 Node：TICK 轮 3、用户轮 5（clamp 1..=8）。
pub fn format_self_evolution_for_prompt(db: &Db, max_recent: usize) -> String {
    let state = get_self_evolution_state(db);
    if !state.enabled || state.recent.is_empty() {
        return String::new();
    }
    let cutoff = Utc::now() - Duration::days(PROMPT_MAX_AGE_DAYS);
    let n = max_recent.clamp(1, 8);
    let recent: Vec<&EvolutionEntry> = state
        .recent
        .iter()
        .filter(|e| match DateTime::parse_from_rfc3339(&e.learned_at) {
            Ok(t) => t.with_timezone(&Utc) >= cutoff,
            Err(_) => true,
        })
        .take(n)
        .collect();
    if recent.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    for e in recent {
        let title = if e.title.is_empty() {
            String::new()
        } else {
            format!("{}: ", e.title)
        };
        lines.push(format!(
            "- [{}] {}: {}{}",
            e.kind, e.mem_id, title, e.content
        ));
    }

    [
        "Self-evolution loop is active. It stores reusable procedures, constraints, and failure lessons as long-term policy memories. It does not rewrite source code or change permissions by itself.".to_string(),
        "Recent behavior updates:".to_string(),
        lines.join("\n"),
        "Use this as provenance. Turn-specific guidance still comes from <active-policies> when a learned policy matches the current situation.".to_string(),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        open_database(dir.path().join("t.db")).unwrap()
    }

    fn mem(mem_id: &str, event_type: &str, content: &str, tags: &[&str]) -> Memory {
        Memory {
            id: 0,
            event_type: event_type.to_string(),
            content: content.to_string(),
            detail: String::new(),
            title: content.to_string(),
            mem_id: Some(mem_id.to_string()),
            entities: Vec::new(),
            concepts: Vec::new(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            links: Vec::new(),
            salience: 3,
            source_ref: None,
            timestamp: String::new(),
            parent_id: None,
            embedding: None,
            visibility: true,
            hidden_at: None,
            merged_into: None,
            embedding_dim: None,
            embedding_model: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn default_state_when_empty() {
        let db = test_db();
        let s = get_self_evolution_state(&db);
        assert!(s.enabled);
        assert_eq!(s.version, 1);
        assert_eq!(s.total_events, 0);
        assert!(s.recent.is_empty());
    }

    #[test]
    fn classification_rules() {
        assert!(is_self_evolution_memory(&mem(
            "m1",
            "memory",
            "x",
            &["kind:procedure"]
        )));
        assert!(is_self_evolution_memory(&mem(
            "m2",
            "self_constraint",
            "x",
            &[]
        )));
        assert!(is_self_evolution_memory(&mem(
            "policy_abc",
            "memory",
            "x",
            &[]
        )));
        assert!(!is_self_evolution_memory(&mem(
            "m3",
            "memory",
            "x",
            &["kind:chat"]
        )));
        assert!(!is_self_evolution_memory(&mem(
            "other_x",
            "memory",
            "x",
            &[]
        )));
    }

    #[test]
    fn record_dedup_and_counts() {
        let db = test_db();
        let a = mem(
            "policy_deploy",
            "memory",
            "部署前先跑 lint",
            &["kind:policy"],
        );
        let b = mem("m_note", "memory", "普通对话", &["kind:chat"]);
        let out = record_self_evolution_from_memories(&db, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(out.len(), 1, "只有策略记忆被记录");
        assert_eq!(out[0].mem_id, "policy_deploy");
        assert_eq!(out[0].kind, "policy");

        let s = get_self_evolution_state(&db);
        assert_eq!(s.total_events, 1);
        assert_eq!(s.learned_count, 1);
        assert!(s.last_at.is_some());

        // 重复记录同一 mem_id → 不重复计数
        let out2 = record_self_evolution_from_memories(&db, &[a]).unwrap();
        assert_eq!(out2.len(), 1, "新记忆仍算一次事件");
        let s2 = get_self_evolution_state(&db);
        assert_eq!(s2.total_events, 2);
        assert_eq!(s2.recent.len(), 1, "recent 按 mem_id 去重");
    }

    #[test]
    fn disabled_state_skips_recording() {
        let db = test_db();
        let mut state = get_self_evolution_state(&db);
        state.enabled = false;
        save_state(&db, &state);
        let out = record_self_evolution_from_memories(
            &db,
            &[mem("policy_x", "memory", "x", &["kind:policy"])],
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn format_prompt_shows_recent() {
        let db = test_db();
        // 空状态 → 空串
        assert_eq!(format_self_evolution_for_prompt(&db, 3), "");
        record_self_evolution_from_memories(
            &db,
            &[mem(
                "procedure_release",
                "memory",
                "发版先跑回归",
                &["kind:procedure"],
            )],
        )
        .unwrap();
        let text = format_self_evolution_for_prompt(&db, 3);
        assert!(text.starts_with("Self-evolution loop is active"));
        assert!(text.contains("[procedure] procedure_release"));
        assert!(text.contains("发版先跑回归"));
        assert!(text.contains("Recent behavior updates:"));
        assert!(text.contains("Use this as provenance"));
    }

    #[test]
    fn format_clamps_max_recent() {
        let db = test_db();
        let mems: Vec<Memory> = (0..5)
            .map(|i| mem(&format!("policy_{i}"), "memory", "x", &["kind:policy"]))
            .collect();
        record_self_evolution_from_memories(&db, &mems).unwrap();
        let text = format_self_evolution_for_prompt(&db, 2);
        let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("- [")).collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn snapshot_truncates_recent() {
        let db = test_db();
        let mems: Vec<Memory> = (0..5)
            .map(|i| mem(&format!("policy_{i}"), "memory", "x", &["kind:policy"]))
            .collect();
        record_self_evolution_from_memories(&db, &mems).unwrap();
        let snap = get_self_evolution_snapshot(&db, 2);
        assert_eq!(snap.recent.len(), 2);
        // 完整状态仍有 5 条
        assert_eq!(get_self_evolution_state(&db).recent.len(), 5);
    }

    #[test]
    fn reset_restores_defaults() {
        let db = test_db();
        record_self_evolution_from_memories(
            &db,
            &[mem("policy_x", "memory", "x", &["kind:policy"])],
        )
        .unwrap();
        let s = reset_self_evolution_state(&db);
        assert_eq!(s.total_events, 0);
        assert!(s.recent.is_empty());
        assert_eq!(get_self_evolution_state(&db).total_events, 0);
    }
}
