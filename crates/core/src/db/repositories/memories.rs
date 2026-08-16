//! memories 仓库：插入 / 读取 / 软隐藏 / FTS5 搜索 / 日期窗口 / 向量召回（对齐 `src/db.js`）。
//!
//! 可见性约定：所有读路径默认 `visibility = 1`（软隐藏过滤），
//! 但 mem_id 主键去重 / 整合器需要看到隐藏行（与 Node 版 `VISIBLE_CLAUSE` 一致）。

use rusqlite::{params, OptionalExtension};

use crate::db::models::{now_iso, Memory, NewMemory};
use crate::db::Db;
use crate::error::Result;

/// 插入结果（对齐 Node insertMemory 返回 `{ id, updated }`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertMemoryOutcome {
    /// 行 id（upsert 命中时为已有行 id）
    pub id: i64,
    /// true 表示同 mem_id 已存在、走了更新分支
    pub updated: bool,
}

/// 插入一条记忆（对齐 `insertMemory`）。
/// mem_id 去重：同 mem_id 已存在时直接更新 content/detail/title/entities/tags/links/timestamp，
/// 不新增行（对齐 Node db.js:442-461）。`detail` 缺省时与 `content` 相同；`timestamp` 缺省取当前 UTC ISO。
///
/// 审计 D4 修复：check-then-act（SELECT mem_id → UPDATE/INSERT）整体包进事务。
/// 此前 SELECT 与写入是两次独立 `conn()`（中间释放 Mutex），并发下两线程可同时
/// 通过 SELECT（都不见已存在行）→ 产生重复/悬空记忆；事务内 conn guard 全程持有，
/// `Db` 的单连接 Mutex 串行化保证检查与写入原子。
pub fn insert_memory(db: &Db, m: &NewMemory) -> Result<InsertMemoryOutcome> {
    let entities_json = serde_json::to_string(&m.entities).unwrap_or_else(|_| "[]".into());
    let concepts_json = serde_json::to_string(&m.concepts).unwrap_or_else(|_| "[]".into());
    let tags_json = serde_json::to_string(&m.tags).unwrap_or_else(|_| "[]".into());
    let links_json = serde_json::to_string(&m.links).unwrap_or_else(|_| "[]".into());

    db.transaction(|tx| {
        // mem_id 去重：已存在 → 更新（对齐 Node：只更新内容类字段，不动 event_type/salience/embedding）
        if let Some(mem_id) = m.mem_id.as_deref() {
            if !mem_id.trim().is_empty() {
                let existing: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM memories WHERE mem_id = ?1 LIMIT 1",
                        params![mem_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(id) = existing {
                    tx.execute(
                        r#"
                        UPDATE memories SET
                          content = ?1, detail = ?2, title = ?3,
                          entities = ?4, tags = ?5, links = ?6, timestamp = ?7
                        WHERE id = ?8
                        "#,
                        params![
                            m.content,
                            m.detail,
                            m.title,
                            entities_json,
                            tags_json,
                            links_json,
                            m.timestamp,
                            id
                        ],
                    )?;
                    return Ok(InsertMemoryOutcome { id, updated: true });
                }
            }
        }

        tx.execute(
            r#"
            INSERT INTO memories
              (event_type, content, detail, title, mem_id, entities, concepts, tags,
               links, salience, source_ref, timestamp, parent_id, embedding,
               embedding_dim, embedding_model)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            "#,
            params![
                m.event_type,
                m.content,
                m.detail,
                m.title,
                m.mem_id,
                entities_json,
                concepts_json,
                tags_json,
                links_json,
                m.salience,
                m.source_ref,
                m.timestamp,
                m.parent_id,
                m.embedding,
                m.embedding_dim,
                m.embedding_model,
            ],
        )?;
        Ok(InsertMemoryOutcome {
            id: tx.last_insert_rowid(),
            updated: false,
        })
    })
}

/// 便捷版插入：只需 event_type/content/timestamp（其余默认）。返回新行 id。
pub fn insert_simple(db: &Db, event_type: &str, content: &str) -> Result<i64> {
    Ok(insert_memory(
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
            salience: 3,
            source_ref: None,
            timestamp: now_iso(),
            parent_id: None,
            embedding: None,
            embedding_dim: None,
            embedding_model: None,
        },
    )?
    .id)
}

/// 按 id 读取（不区分可见性——用于整合器/管理端）。
pub fn get_by_id(db: &Db, id: i64) -> Result<Option<Memory>> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM memories WHERE id = ?1")?;
    let mut rows = stmt.query_map(params![id], Memory::from_row)?;
    Ok(rows.next().transpose()?)
}

/// 按语义 mem_id 读取单条（含隐藏行，对齐 `getMemoryByMemId`）。
pub fn get_by_mem_id(db: &Db, mem_id: &str) -> Result<Option<Memory>> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM memories WHERE mem_id = ?1 LIMIT 1")?;
    let mut rows = stmt.query_map(params![mem_id], Memory::from_row)?;
    Ok(rows.next().transpose()?)
}

/// 按 mem_id 物理删除（对齐 `deleteMemoryByMemId`）。返回是否删到行。
pub fn delete_by_mem_id(db: &Db, mem_id: &str) -> Result<bool> {
    if mem_id.trim().is_empty() {
        return Err(crate::error::CoreError::InvalidInput(
            "deleteMemoryByMemId 需要 mem_id".into(),
        ));
    }
    let conn = db.conn();
    let n = conn.execute("DELETE FROM memories WHERE mem_id = ?1", params![mem_id])?;
    Ok(n > 0)
}

/// 软隐藏记忆（对齐 `hideMemoryByMemId`）：visibility=0 + hidden_at + 可选 merged_into。
/// 数据完整保留，可凭 mem_id 反向复活。
pub fn hide_by_mem_id(
    db: &Db,
    mem_id: &str,
    merged_into: Option<&str>,
    hidden_at: Option<&str>,
) -> Result<bool> {
    if mem_id.trim().is_empty() {
        return Err(crate::error::CoreError::InvalidInput(
            "hideMemoryByMemId 需要 mem_id".into(),
        ));
    }
    let fallback_ts = now_iso();
    let ts = hidden_at.unwrap_or(&fallback_ts);
    let conn = db.conn();
    let n = conn.execute(
        "UPDATE memories SET visibility = 0, hidden_at = ?1, merged_into = ?2 WHERE mem_id = ?3",
        params![ts, merged_into, mem_id],
    )?;
    Ok(n > 0)
}

/// 更新 embedding（对齐 `updateMemoryEmbedding`）：维度从 BLOB 字节数 / 4 反推。
pub fn update_embedding(
    db: &Db,
    mem_id: &str,
    embedding: Option<&[u8]>,
    model: Option<&str>,
) -> Result<()> {
    let dim = embedding
        .filter(|b| !b.is_empty())
        .map(|b| (b.len() / 4) as i64);
    let model_tag = match (embedding, model) {
        (Some(b), Some(m)) if !b.is_empty() && !m.is_empty() => Some(m.to_string()),
        _ => None,
    };
    let conn = db.conn();
    conn.execute(
        "UPDATE memories SET embedding = ?1, embedding_dim = ?2, embedding_model = ?3 WHERE mem_id = ?4",
        params![embedding, dim, model_tag, mem_id],
    )?;
    Ok(())
}

/// 最近 N 条可见记忆（时间倒序）。
pub fn recent(db: &Db, limit: u32) -> Result<Vec<Memory>> {
    let conn = db.conn();
    let mut stmt =
        conn.prepare("SELECT * FROM memories WHERE visibility = 1 ORDER BY id DESC LIMIT ?")?;
    let rows = stmt.query_map(params![limit], Memory::from_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 与某实体相关的记忆（entities 数组包含该实体，按 salience 降序再按时间降序；
/// 对齐 getMemoriesByEntity 的 entities 匹配分支）。
pub fn get_by_entity(db: &Db, entity: &str, limit: u32) -> Result<Vec<Memory>> {
    let pattern = format!("%{}%", entity.trim());
    let conn = db.conn();
    let mut stmt = conn.prepare(
        r#"
        SELECT * FROM memories
        WHERE entities LIKE ?1
          AND visibility = 1
        ORDER BY COALESCE(salience, 3) DESC, timestamp DESC
        LIMIT ?2
        "#,
    )?;
    let rows = stmt.query_map(params![pattern, limit], Memory::from_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 搜索记忆（对齐 `searchMemories`）：
/// - 查询 < 3 字符：直接 LIKE 兜底（trigram 无法匹配短查询）
/// - ≥3 字符：FTS5 MATCH + bm25 相关度排序；命中 0 或语法错误时降级 LIKE
pub fn search(db: &Db, keyword: &str, limit: u32) -> Result<Vec<Memory>> {
    let kw = keyword.trim().to_string();
    let fallback = |db: &Db, kw: &str, limit: u32| -> Result<Vec<Memory>> {
        let like = format!("%{kw}%");
        let conn = db.conn();
        let mut stmt = conn.prepare(
            r#"
            SELECT * FROM memories
            WHERE (
              title LIKE ?1 OR mem_id LIKE ?1 OR content LIKE ?1 OR detail LIKE ?1
              OR entities LIKE ?1 OR concepts LIKE ?1 OR tags LIKE ?1
            )
            AND visibility = 1
            ORDER BY COALESCE(salience, 3) DESC, timestamp DESC
            LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![like, limit], Memory::from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    };

    if kw.chars().count() < 3 {
        return fallback(db, &kw, limit);
    }

    // FTS5 命中（bm25 相关度 + 时间倒序）
    let fts_hits = conn_query_fts(db, &kw, limit);
    match fts_hits {
        Ok(hits) if !hits.is_empty() => Ok(hits),
        _ => fallback(db, &kw, limit), // 0 命中 / 语法错误 → LIKE
    }
}

fn conn_query_fts(db: &Db, kw: &str, limit: u32) -> Result<Vec<Memory>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        r#"
        SELECT m.*, bm25(memories_fts) AS _ftsScore FROM memories m
        JOIN memories_fts ON memories_fts.rowid = m.id
        WHERE memories_fts MATCH ?1 AND m.visibility = 1
        ORDER BY bm25(memories_fts), m.timestamp DESC
        LIMIT ?2
        "#,
    )?;
    let rows = stmt.query_map(params![kw, limit], Memory::from_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 一条带相关度得分的记忆（对齐 Node 检索层 `row._ftsScore / _vecScore` 附加字段）。
/// `embedding` 已由 `search_by_embedding` 剥离，不随得分结构传出（对齐 Node drop 行为）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredMemory {
    pub memory: Memory,
    /// FTS5 bm25 得分（FTS 命中才有；LIKE 兜底为 None）
    pub fts_score: Option<f64>,
    /// 余弦相似度（向量召回才有）
    pub vec_score: Option<f32>,
}

/// 带得分的单关键词搜索（对齐 `searchMemoriesByKeywords` 内部逐关键词的 `searchMemories`）：
/// <3 字符走 LIKE（score=None），≥3 字符先 FTS（bm25）后 LIKE 兜底。
pub fn search_scored(db: &Db, keyword: &str, limit: u32) -> Result<Vec<ScoredMemory>> {
    let kw = keyword.trim().to_string();
    if kw.chars().count() < 3 {
        return fallback_scored(db, &kw, limit);
    }
    match conn_query_fts_scored(db, &kw, limit) {
        Ok(hits) if !hits.is_empty() => Ok(hits),
        _ => fallback_scored(db, &kw, limit), // 0 命中 / 语法错误 → LIKE
    }
}

fn conn_query_fts_scored(db: &Db, kw: &str, limit: u32) -> Result<Vec<ScoredMemory>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        r#"
        SELECT m.*, bm25(memories_fts) AS _ftsScore FROM memories m
        JOIN memories_fts ON memories_fts.rowid = m.id
        WHERE memories_fts MATCH ?1 AND m.visibility = 1
        ORDER BY bm25(memories_fts), m.timestamp DESC
        LIMIT ?2
        "#,
    )?;
    let rows = stmt.query_map(params![kw, limit], |row| {
        let score = row.get::<_, f64>("_ftsScore").ok();
        Ok(ScoredMemory {
            memory: Memory::from_row(row)?,
            fts_score: score,
            vec_score: None,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn fallback_scored(db: &Db, kw: &str, limit: u32) -> Result<Vec<ScoredMemory>> {
    let like = format!("%{kw}%");
    let conn = db.conn();
    let mut stmt = conn.prepare(
        r#"
        SELECT * FROM memories
        WHERE (
          title LIKE ?1 OR mem_id LIKE ?1 OR content LIKE ?1 OR detail LIKE ?1
          OR entities LIKE ?1 OR concepts LIKE ?1 OR tags LIKE ?1
        )
        AND visibility = 1
        ORDER BY COALESCE(salience, 3) DESC, timestamp DESC
        LIMIT ?2
        "#,
    )?;
    let rows = stmt.query_map(params![like, limit], |row| {
        Ok(ScoredMemory {
            memory: Memory::from_row(row)?,
            fts_score: None,
            vec_score: None,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 日期窗口排序方式（对齐 `getMemoriesByDateRange` 的 orderBy 白名单）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateRangeOrder {
    /// 默认：先重要性（salience desc），再时间早晚（timestamp asc）
    SalienceDescTimestampAsc,
    /// 纯时间倒序（对齐 `getMemoriesByTimeRange`）
    TimestampDesc,
}

impl DateRangeOrder {
    fn as_sql(self) -> &'static str {
        match self {
            DateRangeOrder::SalienceDescTimestampAsc => "COALESCE(salience, 3) DESC, timestamp ASC",
            DateRangeOrder::TimestampDesc => "timestamp DESC",
        }
    }
}

/// 日期窗口查询参数（对齐 `getMemoriesByDateRange` 的 options 解构，limit 默认 8）。
#[derive(Debug, Clone)]
pub struct DateRangeQuery<'a> {
    pub types: Option<&'a [&'a str]>,
    pub min_salience: Option<i64>,
    pub limit: u32,
    pub order_by: Option<DateRangeOrder>,
}

impl Default for DateRangeQuery<'_> {
    fn default() -> Self {
        DateRangeQuery {
            types: None,
            min_salience: None,
            limit: 8,
            order_by: None,
        }
    }
}

/// 按日期窗口拉记忆（对齐 `getMemoriesByDateRange`）：
/// - 半开区间 [from, to)，避免跨日边界双重计入
/// - from/to 为本地带偏移 ISO；用 `strftime('%s', ...)` 转 unixepoch 比较，
///   避开字符串字典序在 '+08:00' / 'Z' 上的踩坑
/// - 默认按 salience desc, timestamp asc：先重要、再时间早晚
pub fn get_by_date_range(
    db: &Db,
    from: &str,
    to: &str,
    query: &DateRangeQuery,
) -> Result<Vec<Memory>> {
    let conn = db.conn();
    // L17（波 1）：timestamp 经迁移归一为 UTC Z，from/to 归一为 Z 后直接字符串比较（走索引）
    let from_z = crate::db::models::normalize_to_utc_z(from).unwrap_or_else(|| from.to_string());
    let to_z = crate::db::models::normalize_to_utc_z(to).unwrap_or_else(|| to.to_string());
    let mut conditions = vec![
        "timestamp >= ?1".to_string(),
        "timestamp <  ?2".to_string(),
        format!("{}", Memory::visible_clause()),
    ];
    let mut params: Vec<rusqlite::types::Value> = vec![from_z.into(), to_z.into()];
    if let Some(types) = query.types {
        if !types.is_empty() {
            let ph: Vec<String> = types
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 3))
                .collect();
            conditions.push(format!("event_type IN ({})", ph.join(",")));
            params.extend(types.iter().map(|t| t.to_string().into()));
        }
    }
    let next_idx = 3 + query.types.map_or(0, |t| t.len());
    if let Some(min_salience) = query.min_salience {
        conditions.push(format!("COALESCE(salience, 3) >= ?{next_idx}"));
        params.push(min_salience.into());
    }
    let limit_idx = next_idx + usize::from(query.min_salience.is_some());
    params.push(query.limit.into()); // LIMIT ?{limit_idx}
    let order_by = query
        .order_by
        .unwrap_or(DateRangeOrder::SalienceDescTimestampAsc);
    let sql = format!(
        "SELECT * FROM memories WHERE {} ORDER BY {} LIMIT ?{limit_idx}",
        conditions.join(" AND "),
        order_by.as_sql(),
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(Memory::from_row(row)?);
    }
    Ok(out)
}

/// 向量召回上限：候选行超过该数量时静默跳过向量路径（对齐 `VEC_FULL_SCAN_LIMIT`）。
pub const VEC_FULL_SCAN_LIMIT: u32 = 5000;

/// 按 embedding 向量召回（对齐 `searchByEmbedding`）：
/// - 只比与 query 同维度（或历史 NULL 维度）的可见行；超上限直接返回空
/// - 余弦相似度在 Rust 侧手算（f32），维度不符的行兜底剔除
/// - 返回按相似度降序，且剥离 embedding BLOB（对齐 Node drop 行为）
pub fn search_by_embedding(db: &Db, query: &[f32], limit: u32) -> Result<Vec<ScoredMemory>> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let dim = query.len();
    let conn = db.conn();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL AND visibility = 1 \
         AND (embedding_dim = ?1 OR embedding_dim IS NULL)",
        params![dim as i64],
        |r| r.get(0),
    )?;
    if count > VEC_FULL_SCAN_LIMIT as i64 {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT * FROM memories WHERE embedding IS NOT NULL AND visibility = 1 \
         AND (embedding_dim = ?1 OR embedding_dim IS NULL)",
    )?;
    let rows = stmt.query_map(params![dim as i64], Memory::from_row)?;
    let mut scored: Vec<ScoredMemory> = Vec::new();
    for r in rows {
        let mut memory = r?;
        let Some(blob) = memory.embedding.take() else {
            continue;
        };
        if blob.len() % 4 != 0 {
            continue;
        }
        let vec: Vec<f32> = blob
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        if vec.len() != dim {
            continue; // 历史 NULL 维度行的兜底剔除（对齐 cosine 守卫）
        }
        let score = cosine_f32(query, &vec);
        scored.push(ScoredMemory {
            memory,
            fts_score: None,
            vec_score: Some(score),
        });
    }
    scored.sort_by(|a, b| {
        b.vec_score
            .partial_cmp(&a.vec_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit as usize);
    Ok(scored)
}

/// 更新记忆 salience（对齐 `downgrade_memory` / recognizer 更新路径）。
/// 返回是否命中行。范围校验（1..=5）由工具层负责。
pub fn update_salience(db: &Db, mem_id: &str, new_salience: i64) -> Result<bool> {
    if mem_id.trim().is_empty() {
        return Err(crate::error::CoreError::InvalidInput(
            "update_salience 需要 mem_id".into(),
        ));
    }
    let conn = db.conn();
    let n = conn.execute(
        "UPDATE memories SET salience = ?1 WHERE mem_id = ?2",
        params![new_salience, mem_id],
    )?;
    Ok(n > 0)
}

/// 合并后更新 keep 记忆（对齐 `merge_memories` 语义）：
/// content/detail/title/salience 取合并值；entities 做 union 合并。
/// 返回是否命中行。
pub fn merge_update(
    db: &Db,
    keep_mem_id: &str,
    merged_content: &str,
    merged_detail: Option<&str>,
    merged_entities: &[String],
    merged_salience: i64,
) -> Result<bool> {
    if keep_mem_id.trim().is_empty() {
        return Err(crate::error::CoreError::InvalidInput(
            "merge_update 需要 keep_mem_id".into(),
        ));
    }
    let detail = merged_detail
        .map(|s| s.to_string())
        .unwrap_or_else(|| merged_content.to_string());
    let entities_json = serde_json::to_string(merged_entities).unwrap_or_else(|_| "[]".into());
    let conn = db.conn();
    let n = conn.execute(
        r#"
        UPDATE memories SET
          content = ?1, detail = ?2, entities = ?3, salience = ?4, timestamp = ?5
        WHERE mem_id = ?6
        "#,
        params![
            merged_content,
            detail,
            entities_json,
            merged_salience,
            now_iso(),
            keep_mem_id
        ],
    )?;
    Ok(n > 0)
}

/// 余弦相似度（f32，NaN/Inf 归 0）。
fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom <= f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        open_database(path).unwrap()
    }

    #[test]
    fn insert_and_read_roundtrip() {
        let db = test_db();
        let id = insert_simple(&db, "fact", "用户喜欢喝咖啡，偏好少糖。").unwrap();
        let got = get_by_id(&db, id).unwrap().unwrap();
        assert_eq!(got.event_type, "fact");
        assert_eq!(got.content, "用户喜欢喝咖啡，偏好少糖。");
        assert_eq!(got.salience, 3); // 默认
        assert!(got.visibility); // 默认可见
        assert!(got.created_at.starts_with("2026") || !got.created_at.is_empty());
    }

    #[test]
    fn insert_memory_upserts_on_same_mem_id() {
        let db = test_db();
        let mk = |content: &str| NewMemory {
            event_type: "fact".into(),
            content: content.into(),
            detail: content.into(),
            title: String::new(),
            mem_id: Some("mem_upsert_1".into()),
            entities: vec!["测试".into()],
            concepts: Vec::new(),
            tags: Vec::new(),
            links: Vec::new(),
            salience: 3,
            source_ref: None,
            timestamp: "2026-08-09T00:00:00Z".into(),
            parent_id: None,
            embedding: None,
            embedding_dim: None,
            embedding_model: None,
        };

        // 首次插入 → 新行
        let first = insert_memory(&db, &mk("第一版内容")).unwrap();
        assert!(!first.updated);
        // 同 mem_id 再插 → 更新原行，不新增
        let second = insert_memory(&db, &mk("第二版内容")).unwrap();
        assert!(second.updated);
        assert_eq!(second.id, first.id);
        let got = get_by_mem_id(&db, "mem_upsert_1").unwrap().unwrap();
        assert_eq!(got.content, "第二版内容");
        // 行数不变
        assert_eq!(recent(&db, 100).unwrap().len(), 1);

        // 空 mem_id / 无 mem_id → 始终新增
        let no_id = insert_memory(
            &db,
            &NewMemory {
                mem_id: None,
                ..mk("x")
            },
        )
        .unwrap();
        assert!(!no_id.updated);
        let blank_id = insert_memory(
            &db,
            &NewMemory {
                mem_id: Some("   ".into()),
                ..mk("y")
            },
        )
        .unwrap();
        assert!(!blank_id.updated);
    }

    #[test]
    fn mem_id_lookup_and_delete() {
        let db = test_db();
        let mem_id = "mem_root_001";
        insert_memory(
            &db,
            &NewMemory {
                event_type: "identity".into(),
                content: "我是小白龙".into(),
                detail: "root identity".into(),
                title: "人格根记忆".into(),
                mem_id: Some(mem_id.into()),
                entities: vec!["小白龙".into()],
                concepts: Vec::new(),
                tags: vec!["身份".into()],
                links: Vec::new(),
                salience: 5,
                source_ref: Some("identity_normalizer".into()),
                timestamp: now_iso(),
                parent_id: None,
                embedding: None,
                embedding_dim: None,
                embedding_model: None,
            },
        )
        .unwrap();

        let got = get_by_mem_id(&db, mem_id).unwrap().unwrap();
        assert_eq!(got.title, "人格根记忆");
        assert_eq!(got.entities, vec!["小白龙"]);
        assert_eq!(got.tags, vec!["身份"]);

        // 软隐藏后：普通读路径消失，mem_id 直查仍在（对齐 Node 版）
        hide_by_mem_id(&db, mem_id, None, None).unwrap();
        assert!(recent(&db, 10).unwrap().is_empty());
        assert!(get_by_mem_id(&db, mem_id).unwrap().is_some());

        // 物理删除
        assert!(delete_by_mem_id(&db, mem_id).unwrap());
        assert!(get_by_mem_id(&db, mem_id).unwrap().is_none());
    }

    #[test]
    fn fts_search_chinese_and_like_fallback() {
        let db = test_db();
        insert_simple(&db, "fact", "用户喜欢喝咖啡，偏好少糖。").unwrap();
        insert_simple(&db, "fact", "股票市场今天波动很大。").unwrap();

        // ≥3 字符：FTS trigram 命中
        let hits = search(&db, "喜欢喝咖啡", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "用户喜欢喝咖啡，偏好少糖。");

        // <3 字符：LIKE 兜底命中
        let hits = search(&db, "咖啡", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hits = search(&db, "股票", 10).unwrap();
        assert_eq!(hits.len(), 1);

        // 无命中 → 空
        assert!(search(&db, "不存在的关键词测试", 10).unwrap().is_empty());
    }

    #[test]
    fn update_embedding_sets_dim() {
        let db = test_db();
        let mem_id = "mem_emb_1";
        insert_memory(
            &db,
            &NewMemory {
                event_type: "fact".into(),
                content: "x".into(),
                detail: "x".into(),
                title: String::new(),
                mem_id: Some(mem_id.into()),
                entities: Vec::new(),
                concepts: Vec::new(),
                tags: Vec::new(),
                links: Vec::new(),
                salience: 3,
                source_ref: None,
                timestamp: now_iso(),
                parent_id: None,
                embedding: None,
                embedding_dim: None,
                embedding_model: None,
            },
        )
        .unwrap();

        // 4 个 f32 = 16 字节 → dim 4
        let buf: Vec<u8> = vec![0u8; 16];
        update_embedding(&db, mem_id, Some(&buf), Some("Xenova/bge-large-zh-v1.5")).unwrap();
        let got = get_by_mem_id(&db, mem_id).unwrap().unwrap();
        assert_eq!(got.embedding_dim, Some(4));
        assert_eq!(
            got.embedding_model.as_deref(),
            Some("Xenova/bge-large-zh-v1.5")
        );
        assert_eq!(got.embedding.as_deref(), Some(&buf[..]));
    }

    #[test]
    fn search_scored_returns_bm25_or_none() {
        let db = test_db();
        insert_simple(&db, "fact", "用户喜欢喝咖啡，偏好少糖。").unwrap();
        insert_simple(&db, "fact", "股票市场今天波动很大。").unwrap();

        // ≥3 字符：FTS 命中带 bm25 得分
        let hits = search_scored(&db, "喜欢喝咖啡", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].fts_score.is_some());
        assert!(hits[0].vec_score.is_none());

        // <3 字符：LIKE 兜底，score=None
        let hits = search_scored(&db, "咖啡", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].fts_score.is_none());
    }

    #[test]
    fn date_range_half_open_window() {
        let db = test_db();
        // 用显式 timestamp 插入（本地带偏移 ISO）
        let mk = |mem_id: &str, content: &str, ts: &str, sal: i64| {
            insert_memory(
                &db,
                &NewMemory {
                    event_type: "fact".into(),
                    content: content.into(),
                    detail: content.into(),
                    title: String::new(),
                    mem_id: Some(mem_id.into()),
                    entities: Vec::new(),
                    concepts: Vec::new(),
                    tags: Vec::new(),
                    links: Vec::new(),
                    salience: sal,
                    source_ref: None,
                    // L17：对齐生产写入侧（now_iso 归一 Z），测试数据也归一
                    timestamp: crate::db::models::normalize_to_utc_z(ts)
                        .unwrap_or_else(|| ts.to_string())
                        .into(),
                    parent_id: None,
                    embedding: None,
                    embedding_dim: None,
                    embedding_model: None,
                },
            )
            .unwrap();
        };
        // 窗口 [04-12, 04-13)（本地 +08:00）
        mk("m1", "昨天的事", "2026-04-12T00:00:00+08:00", 5);
        mk("m2", "昨天稍晚的事", "2026-04-12T23:59:59+08:00", 1);
        mk("m3", "今天的事（窗口外）", "2026-04-13T00:00:00+08:00", 5);
        mk("m4", "前天的事（窗口外）", "2026-04-11T23:59:59+08:00", 5);

        let got = get_by_date_range(
            &db,
            "2026-04-12T00:00:00+08:00",
            "2026-04-13T00:00:00+08:00",
            &DateRangeQuery::default(),
        )
        .unwrap();
        assert_eq!(got.len(), 2);
        // 默认排序：salience desc, timestamp asc → m1(5) 在 m2(1) 前
        assert_eq!(got[0].mem_id.as_deref(), Some("m1"));
        assert_eq!(got[1].mem_id.as_deref(), Some("m2"));

        // minSalience 过滤
        let got = get_by_date_range(
            &db,
            "2026-04-12T00:00:00+08:00",
            "2026-04-13T00:00:00+08:00",
            &DateRangeQuery {
                min_salience: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].mem_id.as_deref(), Some("m1"));

        // types 过滤
        let got = get_by_date_range(
            &db,
            "2026-04-12T00:00:00+08:00",
            "2026-04-13T00:00:00+08:00",
            &DateRangeQuery {
                types: Some(&["opinion"][..]),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(got.is_empty());

        // limit
        let got = get_by_date_range(
            &db,
            "2026-04-12T00:00:00+08:00",
            "2026-04-13T00:00:00+08:00",
            &DateRangeQuery {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn date_range_ignores_utc_vs_local_lexical_trick() {
        let db = test_db();
        // timestamp 用 Z（UTC）后缀写入；from/to 用 +08:00 —— strftime 转 unixepoch 后相等
        insert_memory(
            &db,
            &NewMemory {
                event_type: "fact".into(),
                content: "边界行".into(),
                detail: "边界行".into(),
                title: String::new(),
                mem_id: Some("edge".into()),
                entities: Vec::new(),
                concepts: Vec::new(),
                tags: Vec::new(),
                links: Vec::new(),
                salience: 3,
                source_ref: None,
                timestamp: "2026-04-12T16:00:00Z".into(), // = 04-13 00:00 +08:00
                parent_id: None,
                embedding: None,
                embedding_dim: None,
                embedding_model: None,
            },
        )
        .unwrap();
        let got = get_by_date_range(
            &db,
            "2026-04-13T00:00:00+08:00",
            "2026-04-14T00:00:00+08:00",
            &DateRangeQuery::default(),
        )
        .unwrap();
        assert_eq!(got.len(), 1); // 04-13 00:00+08 == 04-12 16:00Z → 落在 [04-13, 04-14) 窗口内
                                  // 且不在 [04-12, 04-13) 窗口内
        let got = get_by_date_range(
            &db,
            "2026-04-12T00:00:00+08:00",
            "2026-04-13T00:00:00+08:00",
            &DateRangeQuery::default(),
        )
        .unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn search_by_embedding_ranks_by_cosine() {
        let db = test_db();
        let f32_bytes = |v: f32| v.to_le_bytes().to_vec();
        insert_memory(
            &db,
            &NewMemory {
                event_type: "fact".into(),
                content: "a".into(),
                detail: "a".into(),
                title: String::new(),
                mem_id: Some("v1".into()),
                entities: Vec::new(),
                concepts: Vec::new(),
                tags: Vec::new(),
                links: Vec::new(),
                salience: 3,
                source_ref: None,
                timestamp: now_iso(),
                parent_id: None,
                embedding: Some([f32_bytes(1.0), f32_bytes(0.0), f32_bytes(0.0)].concat()),
                embedding_dim: Some(3),
                embedding_model: Some("test".into()),
            },
        )
        .unwrap();
        insert_memory(
            &db,
            &NewMemory {
                event_type: "fact".into(),
                content: "b".into(),
                detail: "b".into(),
                title: String::new(),
                mem_id: Some("v2".into()),
                entities: Vec::new(),
                concepts: Vec::new(),
                tags: Vec::new(),
                links: Vec::new(),
                salience: 3,
                source_ref: None,
                timestamp: now_iso(),
                parent_id: None,
                embedding: Some([f32_bytes(1.0), f32_bytes(1.0), f32_bytes(0.0)].concat()),
                embedding_dim: Some(3),
                embedding_model: Some("test".into()),
            },
        )
        .unwrap();
        // 异维度行（4 维）→ 被剔除
        insert_memory(
            &db,
            &NewMemory {
                event_type: "fact".into(),
                content: "c".into(),
                detail: "c".into(),
                title: String::new(),
                mem_id: Some("v3".into()),
                entities: Vec::new(),
                concepts: Vec::new(),
                tags: Vec::new(),
                links: Vec::new(),
                salience: 3,
                source_ref: None,
                timestamp: now_iso(),
                parent_id: None,
                embedding: Some(
                    std::iter::repeat_with(|| f32_bytes(0.0))
                        .take(4)
                        .collect::<Vec<_>>()
                        .concat(),
                ),
                embedding_dim: Some(4),
                embedding_model: Some("test".into()),
            },
        )
        .unwrap();

        let query = vec![1.0f32, 1.0f32, 0.0f32];
        let hits = search_by_embedding(&db, &query, 10).unwrap();
        assert_eq!(hits.len(), 2); // v3 被剔除
                                   // 相似度降序：v2(1.0) > v1(0.707)
        assert_eq!(hits[0].memory.mem_id.as_deref(), Some("v2"));
        assert_eq!(hits[1].memory.mem_id.as_deref(), Some("v1"));
        // embedding 已被剥离
        assert!(hits[0].memory.embedding.is_none());
        assert!(hits[0].vec_score.unwrap() > 0.9);
    }
}
