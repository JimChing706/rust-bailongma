//! 注入器 · 检索/排序/选择层（对齐 `src/memory/injector-retrieval.js`）。
//!
//! 从 injector 编排层拆出的记忆检索原语：消息解析、FTS5 + 向量召回、
//! 重要性重排、「少即是强」选择器、时间词轮廓召回、概念追加召回。
//! 所有 DB 访问走 `crate::db::repositories::memories`，向量能力走 `crate::embedding::Embedder`
//! （当前 `NoopEmbedder` 未配置 → 行为完全退化为 FTS5-only，与 Node 版一致）。

use chrono::{DateTime, Local, NaiveDateTime};

use crate::db::repositories::memories::{
    get_by_date_range, search_by_embedding, search_scored, DateRangeOrder, DateRangeQuery,
    ScoredMemory,
};
use crate::db::Db;
use crate::embedding::Embedder;
use crate::error::Result;

use super::keywords::extract_keywords;
use super::temporal::{parse_temporal_hints, TemporalHint};

// ---------------------------------------------------------------------------
// 消息格式解析
// ---------------------------------------------------------------------------

/// 解析注入器输入头。
///
/// 格式：`[ID:xxxxxx] 2026-04-13 10:00:00 [渠道] 内容` 或 `TICK 2026-04-13-10:00:00`
/// （对齐 parseMessageInput）。`sender_id` 只取 canonicalId —— 外部渠道的
/// `[canonicalId via wechat:clawbot:...]` 复合串拆出 ` via ` 前的部分，
/// 否则会污染 from_id 使 `getRecentConversation` 查空。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedMessage {
    pub is_tick: bool,
    pub sender_id: Option<String>,
    pub message_body: String,
}

pub fn parse_message_input(message: &str) -> ParsedMessage {
    let trimmed = message.trim();
    // L7（审计修复）：TICK 前缀正则每次调用重编译，热路径上每轮一次；改 OnceLock 编译一次。
    static TICK_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    if TICK_RE
        .get_or_init(|| regex::Regex::new(r"^TICK\s").expect("static regex"))
        .is_match(trimmed)
    {
        return ParsedMessage {
            is_tick: true,
            sender_id: None,
            message_body: String::new(),
        };
    }
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // 对齐 Node parseMessageInput（message-input.js）：`[\d\-T:+]+` 兼容
        // `2026-04-13-10:00:00` 与 `2026-04-11T15:32:00+08:00`
        regex::Regex::new(r"(?s)^\[([^\]]+)\]\s*[\d\-T:+]+\s*\[[^\]]*\]\s*(.*)$")
            .expect("static regex")
    });
    let raw_id = re
        .captures(trimmed)
        .and_then(|c| c.get(1).map(|m| m.as_str().trim().to_string()));
    // 只取 canonicalId：`[canonicalId via externalPartyId]` 复合串拆出 ` via ` 前的部分
    let sender_id = raw_id.map(|id| {
        id.split_whitespace()
            .take_while(|w| !w.eq_ignore_ascii_case("via"))
            .collect::<Vec<_>>()
            .join(" ")
    });
    let message_body = match re.captures(trimmed) {
        Some(c) => c
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default(),
        None => trimmed.to_string(),
    };
    ParsedMessage {
        is_tick: false,
        sender_id,
        message_body,
    }
}

// ---------------------------------------------------------------------------
// 重要性重排（桶内）
// ---------------------------------------------------------------------------

/// 365 天的陈旧阈值（毫秒）。
const STALE_MS: i64 = 365 * 24 * 3600 * 1000;

/// 解析记忆时间戳为 epoch 毫秒（兼容 `+08:00` 偏移 ISO 与空格分隔的本地格式）。
fn timestamp_epoch_ms(ts: &str) -> Option<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S") {
        return Some(dt.and_utc().timestamp_millis());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp_millis());
    }
    None
}

/// 陈旧（距今 > 365 天）；时间戳无法解析视为不陈旧（对齐 isStale）。
fn is_stale(ts: &str, now_ms: i64) -> bool {
    match timestamp_epoch_ms(ts) {
        Some(t) => now_ms.saturating_sub(t) > STALE_MS,
        None => false,
    }
}

/// 桶内重排：salience >= 4 的提到前面（按 salience 高到低），
/// 同 boost 组内陈旧（>365 天）下沉，其余维持原顺序（stable sort）。
/// 对齐 rerankByImportance。
pub fn rerank_by_importance(memories: &[ScoredMemory]) -> Vec<ScoredMemory> {
    if memories.is_empty() {
        return Vec::new();
    }
    let now_ms = Local::now().timestamp_millis();
    let boost = |m: &ScoredMemory| -> i64 {
        let s = m.memory.salience;
        if s >= 4 {
            s
        } else {
            0
        }
    };
    let mut out: Vec<ScoredMemory> = memories.to_vec();
    out.sort_by(|a, b| {
        let ba = boost(a);
        let bb = boost(b);
        bb.partial_cmp(&ba)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let sa = is_stale(&a.memory.timestamp, now_ms);
                let sb = is_stale(&b.memory.timestamp, now_ms);
                (sa as u8).cmp(&(sb as u8))
            })
    });
    out
}

// ---------------------------------------------------------------------------
// 「少即是强」选择器
// ---------------------------------------------------------------------------

/// 动态上下文记忆池选择参数（对齐 selectContextMemories options）。
#[derive(Debug, Clone, Copy)]
pub struct SelectOptions {
    pub cap: usize,
    pub anchor_lane: usize,
    /// 相关度地板（bm25 越小越相关）；`None` = 关闭（Phase 1 默认）。
    /// 无得分候选（向量 / LIKE / entity 召回）一律豁免。
    pub fts_floor: Option<f64>,
}

impl Default for SelectOptions {
    fn default() -> Self {
        SelectOptions {
            cap: 12,
            anchor_lane: 2,
            fts_floor: None,
        }
    }
}

/// 「少即是强」选择器（对齐 selectContextMemories）：
/// 保留 candidates 既有相关度序（不再按 salience 整体重排）；给高 salience 锚
/// （>= 4）留 anchor_lane 条窄保留道，替换 cap 内末尾弱位；fts_floor 过滤弱命中。
pub fn select_context_memories(
    candidates: &[ScoredMemory],
    opts: &SelectOptions,
) -> Vec<ScoredMemory> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let floored: Vec<ScoredMemory> = match opts.fts_floor {
        None => candidates.to_vec(),
        Some(floor) => candidates
            .iter()
            .filter(|m| match m.fts_score {
                Some(s) if s.is_finite() => s <= floor,
                _ => true, // 无得分豁免（对齐 !isFinite 豁免）
            })
            .cloned()
            .collect(),
    };
    if floored.len() <= opts.cap {
        return floored;
    }
    let in_cap = floored[..opts.cap].to_vec();
    let overflow = &floored[opts.cap..];
    let anchors: Vec<ScoredMemory> = overflow
        .iter()
        .filter(|m| m.memory.salience >= 4)
        .take(opts.anchor_lane)
        .cloned()
        .collect();
    if anchors.is_empty() {
        return in_cap;
    }
    let keep_len = opts.cap.saturating_sub(anchors.len());
    let mut out: Vec<ScoredMemory> = in_cap.into_iter().take(keep_len).collect();
    out.extend(anchors);
    out
}

// ---------------------------------------------------------------------------
// 相关记忆搜索（focus + context + 向量兜底）
// ---------------------------------------------------------------------------

/// searchRelevantMemories 参数（对齐解构默认值）。
#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub focus_limit: usize,
    pub context_limit: usize,
    pub focus_keywords: usize,
    pub context_keywords: usize,
    pub per_keyword: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            focus_limit: 12,
            context_limit: 8,
            focus_keywords: 8,
            context_keywords: 10,
            per_keyword: 3,
        }
    }
}

/// 向量召回参数：搜索条数与相似度过滤阈值（对齐 searchRelevantMemories 内部）。
const VEC_APPEND_LIMIT: usize = 10;
const VEC_SCORE_FLOOR: f32 = 0.5;

/// 相关记忆搜索：focus 优先（FTS5 关键词 + 向量兜底），context 补充。
/// focus_text 为空直接返回空，不用 context 兜底（对齐 Node 行为）。
pub async fn search_relevant_memories(
    db: &Db,
    embedder: &dyn Embedder,
    focus_text: &str,
    context_text: &str,
    opts: &SearchOptions,
) -> Vec<ScoredMemory> {
    if focus_text.is_empty() {
        return Vec::new();
    }
    let focus_kws = extract_keywords(focus_text, opts.focus_keywords);
    if focus_kws.is_empty() {
        return Vec::new();
    }

    // focus 桶：逐关键词 FTS 召回，id 去重
    let mut seen = std::collections::HashSet::new();
    let mut focus_hits: Vec<ScoredMemory> = Vec::new();
    for keyword in &focus_kws {
        if let Ok(hits) = search_scored(db, keyword, opts.per_keyword as u32) {
            for m in hits {
                if seen.insert(m.memory.id) {
                    focus_hits.push(m);
                }
            }
        }
        if focus_hits.len() >= opts.focus_limit {
            break;
        }
    }
    let focus_capped: Vec<ScoredMemory> = focus_hits.into_iter().take(opts.focus_limit).collect();

    // context 桶：排除 focus 关键词，排除 focus 已命中 id
    let mut seen_all: std::collections::HashSet<i64> =
        focus_capped.iter().map(|m| m.memory.id).collect();
    let mut context_hits: Vec<ScoredMemory> = Vec::new();
    if !context_text.is_empty() && opts.context_limit > 0 {
        let focus_kw_set: std::collections::HashSet<String> = focus_kws.iter().cloned().collect();
        let context_kws_raw = extract_keywords(context_text, opts.context_keywords);
        let ctx_per_keyword = usize::max(1, opts.per_keyword - 1);
        for keyword in context_kws_raw {
            if focus_kw_set.contains(&keyword) {
                continue;
            }
            if let Ok(hits) = search_scored(db, &keyword, ctx_per_keyword as u32) {
                for m in hits {
                    if seen_all.insert(m.memory.id) {
                        context_hits.push(m);
                    }
                }
            }
            if context_hits.len() >= opts.context_limit {
                break;
            }
        }
    }
    let context_capped: Vec<ScoredMemory> =
        context_hits.into_iter().take(opts.context_limit).collect();

    // 向量召回兜底：focus 算 embedding → 语义相似 top-N，追加到 focus 桶末尾。
    // 未配置 embedder（返回 None）→ 静默跳过，行为等同 FTS5-only。
    let mut vec_appended: Vec<ScoredMemory> = Vec::new();
    if let Some(query_emb) = embedder.compute(focus_text, true) {
        if let Ok(vec_hits) = search_by_embedding(
            db,
            &query_emb,
            usize::min(opts.focus_limit, VEC_APPEND_LIMIT) as u32,
        ) {
            let existing: std::collections::HashSet<i64> = focus_capped
                .iter()
                .chain(context_capped.iter())
                .map(|m| m.memory.id)
                .collect();
            vec_appended = vec_hits
                .into_iter()
                .filter(|m| {
                    !existing.contains(&m.memory.id)
                        && m.vec_score.is_some_and(|s| s > VEC_SCORE_FLOOR)
                })
                .collect();
        }
    }

    let focus_ranked = rerank_by_importance(&focus_capped);
    let context_ranked = rerank_by_importance(&context_capped);
    let vec_ranked = rerank_by_importance(&vec_appended);
    let mut merged: Vec<ScoredMemory> = Vec::with_capacity(opts.focus_limit + opts.context_limit);
    merged.extend(focus_ranked);
    merged.extend(vec_ranked);
    merged.extend(context_ranked);
    merged.truncate(opts.focus_limit + opts.context_limit);
    merged
}

// ---------------------------------------------------------------------------
// 去重 / 时间词轮廓召回 / 概念追加召回
// ---------------------------------------------------------------------------

/// 多路记忆合并去重（按 id，对齐 deduplicateMemories）。
pub fn deduplicate_memories(arrays: &[Vec<ScoredMemory>]) -> Vec<ScoredMemory> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in arrays.iter().flatten() {
        if seen.insert(m.memory.id) {
            out.push(m.clone());
        }
    }
    out
}

/// 时间词召回的一个桶（对齐 gatherTemporalRecall bucket）。
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalBucket {
    pub label: String,
    /// YYYY-MM-DD（区间起点）
    pub date: String,
    pub memories: Vec<ScoredMemory>,
}

/// 时间词触发的自动注入：把"昨天/前天/今天"映射成日期窗口，拉 `focus_conclusion`
/// 形成轮廓注入。只在含时间词的 message body 上触发；召回为空返回 None
/// （对齐 gatherTemporalRecall：返回 null 时整个 <temporal-recall> 块不出现）。
pub fn gather_temporal_recall(db: &Db, message_body: &str) -> Result<Option<Vec<TemporalBucket>>> {
    if message_body.is_empty() {
        return Ok(None);
    }
    let hints: Vec<TemporalHint> = parse_temporal_hints(message_body, Local::now());
    if hints.is_empty() {
        return Ok(None);
    }
    let mut buckets: Vec<TemporalBucket> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    for hint in &hints {
        let memories = get_by_date_range(
            db,
            &hint.from,
            &hint.to,
            &DateRangeQuery {
                types: Some(&["focus_conclusion"]),
                min_salience: None,
                limit: 5,
                order_by: Some(DateRangeOrder::SalienceDescTimestampAsc),
            },
        )?;
        // 跨区间去重（日期窗口不重叠通常不会发生，保险起见）
        let filtered: Vec<ScoredMemory> = memories
            .into_iter()
            .filter_map(|memory| {
                if seen_ids.insert(memory.id) {
                    Some(ScoredMemory {
                        memory,
                        fts_score: None,
                        vec_score: None,
                    })
                } else {
                    None
                }
            })
            .collect();
        if filtered.is_empty() {
            continue;
        }
        buckets.push(TemporalBucket {
            label: hint.label.clone(),
            date: hint.from.chars().take(10).collect(),
            memories: filtered,
        });
    }
    if buckets.is_empty() {
        return Ok(None);
    }
    Ok(Some(buckets))
}

/// 概念追加召回（对齐 searchAdditionalMemories）：按概念逐词 FTS 搜索，
/// 排除已召回 id，最多 limit 条。
pub fn search_additional_memories(
    db: &Db,
    concepts: &[String],
    exclude_ids: &std::collections::HashSet<i64>,
    limit: usize,
) -> Vec<ScoredMemory> {
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for concept in concepts {
        let Ok(hits) = search_scored(db, concept, 3) else {
            continue;
        };
        for memory in hits {
            if exclude_ids.contains(&memory.memory.id) {
                continue;
            }
            if !seen.insert(memory.memory.id) {
                continue;
            }
            results.push(memory);
            if results.len() >= limit {
                return results;
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{now_iso, NewMemory};
    use crate::db::{open_database, repositories::memories::insert_memory};
    use crate::embedding::NoopEmbedder;
    use chrono::{Duration, Local};

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.db");
        open_database(path).unwrap()
    }

    fn scored(id: i64, salience: i64, timestamp: &str, fts_score: Option<f64>) -> ScoredMemory {
        ScoredMemory {
            memory: crate::db::models::Memory {
                id,
                event_type: "fact".into(),
                content: format!("mem {id}"),
                detail: String::new(),
                title: String::new(),
                mem_id: Some(format!("m{id}")),
                entities: Vec::new(),
                concepts: Vec::new(),
                tags: Vec::new(),
                links: Vec::new(),
                salience,
                source_ref: None,
                timestamp: timestamp.to_string(),
                parent_id: None,
                embedding: None,
                visibility: true,
                hidden_at: None,
                merged_into: None,
                embedding_dim: None,
                embedding_model: None,
                created_at: String::new(),
            },
            fts_score,
            vec_score: None,
        }
    }

    fn insert_raw(db: &Db, event_type: &str, content: &str, ts: &str, sal: i64) {
        // L17：对齐生产写入侧（now_iso 归一 Z），测试数据也归一，日期范围直接比较才正确
        let ts = crate::db::models::normalize_to_utc_z(ts).unwrap_or_else(|| ts.to_string());
        insert_memory(
            db,
            &NewMemory {
                event_type: event_type.into(),
                content: content.into(),
                detail: content.into(),
                title: String::new(),
                mem_id: None,
                entities: Vec::new(),
                concepts: Vec::new(),
                tags: Vec::new(),
                links: Vec::new(),
                salience: sal,
                source_ref: None,
                timestamp: ts.into(),
                parent_id: None,
                embedding: None,
                embedding_dim: None,
                embedding_model: None,
            },
        )
        .unwrap();
    }

    // ------------------------------------------------------------------
    // parse_message_input（对齐 parseMessageInput）
    // ------------------------------------------------------------------

    #[test]
    fn parses_tick() {
        let p = parse_message_input("TICK 2026-04-13-10:00:00");
        assert!(p.is_tick);
        assert_eq!(p.sender_id, None);
        assert_eq!(p.message_body, "");
    }

    #[test]
    fn parses_full_header() {
        // 对齐 Node 正则：时间串为连续 `[\d\-T:]+`（queue.js 用 `-` 连接日期时间）
        let p = parse_message_input("[ID:000001] 2026-04-13-10:00:00 [wechat] 你好世界");
        assert!(!p.is_tick);
        assert_eq!(p.sender_id.as_deref(), Some("ID:000001"));
        assert_eq!(p.message_body, "你好世界");
    }

    #[test]
    fn strips_via_composite_id() {
        // 外部渠道：`[canonicalId via externalPartyId]`，只取 canonicalId
        let p = parse_message_input(
            "[id:000001 via wechat:clawbot:abc123] 2026-04-13-10:00:00 [wechat] 早上好",
        );
        assert_eq!(p.sender_id.as_deref(), Some("id:000001"));
        assert_eq!(p.message_body, "早上好");
    }

    #[test]
    fn parses_unheaded_message() {
        let p = parse_message_input("  没有头的普通消息  ");
        assert!(!p.is_tick);
        assert_eq!(p.sender_id, None);
        assert_eq!(p.message_body, "没有头的普通消息");
    }

    // ------------------------------------------------------------------
    // rerank_by_importance（对齐 rerankByImportance）
    // ------------------------------------------------------------------

    #[test]
    fn rerank_boosts_high_salience() {
        let now = Local::now().to_rfc3339();
        let items = vec![
            scored(1, 3, &now, None),
            scored(2, 5, &now, None),
            scored(3, 1, &now, None),
            scored(4, 4, &now, None),
        ];
        let ranked = rerank_by_importance(&items);
        let ids: Vec<i64> = ranked.iter().map(|m| m.memory.id).collect();
        // boost 组：5, 4 在前（按 salience 降序）；0 组保持原序 3, 1
        assert_eq!(ids, vec![2, 4, 1, 3]);
    }

    #[test]
    fn rerank_sinks_stale_within_group() {
        let now = Local::now();
        let old = (now - Duration::days(400)).to_rfc3339();
        let recent = now.to_rfc3339();
        // 同 boost（salience 都 < 4 → 0 组）：stale 下沉，其余维持原顺序
        let items = vec![
            scored(1, 3, &old, None), // stale
            scored(2, 3, &recent, None),
            scored(3, 3, &recent, None),
            scored(4, 3, &old, None), // stale
        ];
        let ranked = rerank_by_importance(&items);
        let ids: Vec<i64> = ranked.iter().map(|m| m.memory.id).collect();
        assert_eq!(ids, vec![2, 3, 1, 4]); // 新鲜在前（原序），陈旧在后（原序）
    }

    #[test]
    fn rerank_keeps_original_order_on_ties() {
        let now = Local::now().to_rfc3339();
        let items = vec![
            scored(1, 3, &now, None),
            scored(2, 3, &now, None),
            scored(3, 3, &now, None),
        ];
        let ranked = rerank_by_importance(&items);
        let ids: Vec<i64> = ranked.iter().map(|m| m.memory.id).collect();
        assert_eq!(ids, vec![1, 2, 3]); // stable
    }

    // ------------------------------------------------------------------
    // select_context_memories（对齐 selectContextMemories）
    // ------------------------------------------------------------------

    #[test]
    fn select_caps_at_limit() {
        let now = Local::now().to_rfc3339();
        let items: Vec<ScoredMemory> = (1..=5).map(|i| scored(i, 3, &now, None)).collect();
        let out = select_context_memories(
            &items,
            &SelectOptions {
                cap: 3,
                ..Default::default()
            },
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].memory.id, 1);
    }

    #[test]
    fn select_rescues_anchors_from_overflow() {
        let now = Local::now().to_rfc3339();
        // cap=3，但 overflow 里有 salience>=4 的锚 → 救回 1 条，替换 cap 末尾
        let items = vec![
            scored(1, 3, &now, None),
            scored(2, 3, &now, None),
            scored(3, 3, &now, None),
            scored(4, 3, &now, None),
            scored(5, 5, &now, None), // 锚
        ];
        let out = select_context_memories(
            &items,
            &SelectOptions {
                cap: 3,
                anchor_lane: 1,
                ..Default::default()
            },
        );
        let ids: Vec<i64> = out.iter().map(|m| m.memory.id).collect();
        assert_eq!(ids, vec![1, 2, 5]); // [1,2] + 锚 5
    }

    #[test]
    fn select_respects_fts_floor() {
        let now = Local::now().to_rfc3339();
        // bm25 越小越相关；floor=-5 时 score > -5 的弱命中被丢弃
        let items = vec![
            scored(1, 3, &now, Some(-10.0)), // 强命中
            scored(2, 3, &now, Some(-1.0)),  // 弱命中 → 被 floor 丢掉
            scored(3, 3, &now, None),        // 无分豁免
        ];
        let out = select_context_memories(
            &items,
            &SelectOptions {
                cap: 10,
                fts_floor: Some(-5.0),
                ..Default::default()
            },
        );
        let ids: Vec<i64> = out.iter().map(|m| m.memory.id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    // ------------------------------------------------------------------
    // search_relevant_memories（集成，FTS5-only 路径）
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn search_relevant_focus_then_context_dedup() {
        let db = test_db();
        insert_raw(&db, "fact", "用户喜欢喝咖啡，偏好少糖。", &now_iso(), 3);
        insert_raw(&db, "fact", "咖啡豆最近涨价了。", &now_iso(), 3);
        insert_raw(&db, "fact", "股票市场今天波动很大。", &now_iso(), 3);

        let embedder = NoopEmbedder;
        let hits = search_relevant_memories(
            &db,
            &embedder,
            "咖啡",
            "上次聊股票怎么样",
            &SearchOptions::default(),
        )
        .await;
        // focus 关键词"咖啡"（2 字 → LIKE 兜底）命中两条咖啡记忆；
        // context 关键词排除 focus 词后补充股票记忆
        let contents: Vec<&str> = hits.iter().map(|m| m.memory.content.as_str()).collect();
        assert!(contents.iter().any(|c| c.contains("咖啡")));
        assert!(contents.iter().any(|c| c.contains("股票")));
        assert!(contents.len() <= 12 + 8);
    }

    #[tokio::test]
    async fn search_relevant_empty_focus_returns_empty() {
        let db = test_db();
        insert_raw(&db, "fact", "咖啡豆涨价了。", &now_iso(), 3);
        let embedder = NoopEmbedder;
        let hits =
            search_relevant_memories(&db, &embedder, "", "随便", &SearchOptions::default()).await;
        assert!(hits.is_empty());
    }

    // ------------------------------------------------------------------
    // gather_temporal_recall（对齐 gatherTemporalRecall）
    // ------------------------------------------------------------------

    #[test]
    fn temporal_recall_buckets_yesterday() {
        let db = test_db();
        let now = Local::now();
        let yesterday = (now - Duration::days(1)).to_rfc3339();
        insert_raw(
            &db,
            "focus_conclusion",
            "昨天我们聊完了咖啡采购。",
            &yesterday,
            4,
        );
        insert_raw(
            &db,
            "fact",
            "普通事实不参与时间召回（类型过滤）。",
            &yesterday,
            5,
        );

        let buckets = gather_temporal_recall(&db, "昨天的事").unwrap().unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].label, "昨天");
        assert_eq!(buckets[0].memories.len(), 1);
        assert_eq!(buckets[0].memories[0].memory.event_type, "focus_conclusion");
    }

    #[test]
    fn temporal_recall_none_without_hints() {
        let db = test_db();
        assert!(gather_temporal_recall(&db, "没有时间词").unwrap().is_none());
        assert!(gather_temporal_recall(&db, "").unwrap().is_none());
    }

    // ------------------------------------------------------------------
    // search_additional_memories（对齐 searchAdditionalMemories）
    // ------------------------------------------------------------------

    #[test]
    fn additional_memories_excludes_and_limits() {
        let db = test_db();
        insert_raw(&db, "fact", "概念A相关的记忆甲", &now_iso(), 3);
        insert_raw(&db, "fact", "概念A相关的记忆乙", &now_iso(), 3);
        insert_raw(&db, "fact", "概念B相关的记忆丙", &now_iso(), 3);

        let exclude = std::collections::HashSet::from([1i64]);
        let concepts = vec!["概念A相关".to_string(), "概念B相关".to_string()];
        let out = search_additional_memories(&db, &concepts, &exclude, 10);
        // 排除了 id=1（记忆甲），剩下的被召回
        assert!(out.iter().all(|m| m.memory.id != 1));
        assert!(!out.is_empty());
    }
}
