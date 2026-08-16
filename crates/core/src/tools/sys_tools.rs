//! 系统性工具（Node 版 capabilities 对齐第一批）：
//!
//! - 记忆类：`upsert_memory`（批量写入/更新，mem_id 去重）、`probe_memory`（诊断探测）、
//!   `recall_memory`（深度回忆）、`merge_memories`（合并软隐藏）、`downgrade_memory`（salience 降级）、
//!   `skip_recognition` / `skip_consolidation`（识别/合并跳过信号）
//! - 系统类：`set_agent_name` / `set_location`（config 表）、`set_tick_interval`（节奏，注入回调）、
//!   `find_tool`（工具目录检索）、`complete_startup_self_check`（启动自检收尾）
//! - 任务类：`set_task` / `complete_task` / `update_task_step`（config 表持久化，重启恢复）
//! - 进程类：`exec_quick_command`（复用 exec_command 快速档）、`list_processes` / `kill_process`
//!
//! 接线门与 `search_memory` 一致：无 Db 注入时返回明确错误，由
//! [`super::NativeToolExecutor::is_ready`] 决定是否暴露给 LLM。

use serde_json::{json, Value};

use super::NativeToolExecutor;
use crate::db::models::NewMemory;
use crate::error::{CoreError, Result};

/// 记忆类型允许集（对齐 Node memory schema enum）
const MEMORY_TYPES: &[&str] = &["fact", "person", "object", "knowledge", "article"];
/// content 最大长度（对齐 Node：≤200 中文字符）
const MAX_MEMORY_CONTENT_CHARS: usize = 200;
/// 批量写入最大条数
const MAX_UPSERT_BATCH: usize = 20;

// ─────────────────────────────────────────────────────────────
// 记忆类
// ─────────────────────────────────────────────────────────────

/// upsert_memory：批量插入/更新记忆节点（mem_id 去重：已存在=更新，缺失=插入）。
///
/// - `memories`: 数组，每项含 mem_id(必填)、type、title、content、detail、entities、
///   tags、parent_mem_id、salience(1-5)；
/// - 对齐 Node `upsertMemory` 的 PATCH 语义：同 mem_id 更新内容类字段，省略字段保留原值；
/// - 返回每项的 `{ mem_id, id, updated, status }`。
pub fn upsert_memory_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "upsert_memory 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let memories = args
        .get("memories")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Tool("upsert_memory 缺 memories 数组".into()))?;
    if memories.is_empty() {
        return Err(CoreError::Tool("upsert_memory memories 不能为空".into()));
    }
    if memories.len() > MAX_UPSERT_BATCH {
        return Err(CoreError::Tool(format!(
            "upsert_memory 批量超限（{} 条，上限 {MAX_UPSERT_BATCH}）",
            memories.len()
        )));
    }

    let mut results = Vec::with_capacity(memories.len());
    for item in memories {
        let obj = item
            .as_object()
            .ok_or_else(|| CoreError::Tool("upsert_memory 每条必须是对象".into()))?;
        let mem_id = obj
            .get("mem_id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| CoreError::Tool("upsert_memory 每条必须提供 mem_id".into()))?;
        let mem_type = obj.get("type").and_then(Value::as_str).unwrap_or("fact");
        if !MEMORY_TYPES.contains(&mem_type) {
            return Err(CoreError::Tool(format!(
                "upsert_memory type 非法: {mem_type}（允许 {}）",
                MEMORY_TYPES.join("/")
            )));
        }
        let content = obj
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            return Err(CoreError::Tool(format!(
                "upsert_memory [{mem_id}] content 不能为空"
            )));
        }
        if content.chars().count() > MAX_MEMORY_CONTENT_CHARS {
            return Err(CoreError::Tool(format!(
                "upsert_memory [{mem_id}] content 超长（{} 字符，上限 {MAX_MEMORY_CONTENT_CHARS}）",
                content.chars().count()
            )));
        }
        let salience = obj
            .get("salience")
            .and_then(Value::as_i64)
            .unwrap_or(3)
            .clamp(1, 5);
        let detail = obj.get("detail").and_then(Value::as_str).unwrap_or(content);
        let title = obj.get("title").and_then(Value::as_str).unwrap_or("");
        let entities: Vec<String> = obj
            .get("entities")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let tags: Vec<String> = obj
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let parent_mem_id = obj
            .get("parent_mem_id")
            .and_then(Value::as_str)
            .map(String::from);
        let parent_id = match parent_mem_id {
            Some(pid) if !pid.trim().is_empty() => {
                crate::db::repositories::memories::get_by_mem_id(db, &pid)?.map(|m| m.id)
            }
            _ => None,
        };

        let outcome = crate::db::repositories::memories::insert_memory(
            db,
            &NewMemory {
                event_type: mem_type.into(),
                content: content.to_string(),
                detail: detail.to_string(),
                title: title.to_string(),
                mem_id: Some(mem_id.to_string()),
                entities,
                concepts: Vec::new(),
                tags,
                links: Vec::new(),
                salience,
                source_ref: None,
                timestamp: crate::db::models::now_iso(),
                parent_id,
                embedding: None,
                embedding_dim: None,
                embedding_model: None,
            },
        )
        .map_err(|e| CoreError::Tool(format!("写入记忆失败: {e}")))?;

        results.push(json!({
            "mem_id": mem_id,
            "id": outcome.id,
            "updated": outcome.updated,
            "status": "ok",
        }));
    }
    Ok(json!({ "ok": true, "count": results.len(), "results": results }))
}

/// probe_memory：诊断探测——"如果现在问 X，记忆层会返回什么？"
/// 无副作用，不写 recall 方向（与 recall_memory 区分）。
pub fn probe_memory_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "probe_memory 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if query.is_empty() {
        return Err(CoreError::Tool("probe_memory 缺 query".into()));
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(8)
        .min(20) as u32;
    let hits = crate::db::repositories::memories::search_scored(db, &query, limit)
        .map_err(|e| CoreError::Tool(format!("记忆探测失败: {e}")))?;
    let items: Vec<Value> = hits
        .into_iter()
        .map(|s| {
            json!({
                "id": s.memory.id,
                "mem_id": s.memory.mem_id,
                "event_type": s.memory.event_type,
                "content": s.memory.content,
                "salience": s.memory.salience,
                "score": s.fts_score.or(s.vec_score.map(|v| v as f64)),
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "query": query,
        "count": items.len(),
        "matches": items,
        "hint": if items.is_empty() { "召回为空——考虑 upsert_memory 主动写入而非仅靠注入" } else { "" },
    }))
}

/// recall_memory：深度回忆。返回相关记忆 + 记录 recall 方向（影响下一轮注入）。
pub fn recall_memory_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "recall_memory 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if query.is_empty() {
        return Err(CoreError::Tool("recall_memory 缺 query".into()));
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(12)
        .min(30) as u32;
    let hits = crate::db::repositories::memories::search_scored(db, &query, limit)
        .map_err(|e| CoreError::Tool(format!("记忆回忆失败: {e}")))?;
    let items: Vec<Value> = hits
        .into_iter()
        .map(|s| {
            json!({
                "id": s.memory.id,
                "mem_id": s.memory.mem_id,
                "event_type": s.memory.event_type,
                "content": s.memory.content,
                "detail": s.memory.detail,
                "salience": s.memory.salience,
                "timestamp": s.memory.timestamp,
                "score": s.fts_score.or(s.vec_score.map(|v| v as f64)),
            })
        })
        .collect();
    // 记录 recall 方向（对齐 Node onRecall：state.prev_recall = query）
    if let Some(cb) = &ex.on_recall {
        let _ = cb(&query);
    }
    Ok(json!({ "ok": true, "query": query, "count": items.len(), "memories": items }))
}

/// merge_memories：合并多条语义重复记忆到 keep，drops 软隐藏（visibility=0，绝不物理删除）。
pub fn merge_memories_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "merge_memories 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let keep_mem_id = args
        .get("keep_mem_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| CoreError::Tool("merge_memories 缺 keep_mem_id".into()))?;
    let drop_ids: Vec<String> = args
        .get("drop_mem_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
                .collect()
        })
        .ok_or_else(|| CoreError::Tool("merge_memories 缺 drop_mem_ids".into()))?;
    if drop_ids.is_empty() {
        return Err(CoreError::Tool(
            "merge_memories drop_mem_ids 不能为空".into(),
        ));
    }
    let merged_content = args
        .get("merged_content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if merged_content.is_empty() {
        return Err(CoreError::Tool("merge_memories 缺 merged_content".into()));
    }
    if merged_content.chars().count() > MAX_MEMORY_CONTENT_CHARS {
        return Err(CoreError::Tool(format!(
            "merge_memories merged_content 超长（{} 字符，上限 {MAX_MEMORY_CONTENT_CHARS}）",
            merged_content.chars().count()
        )));
    }

    // keep 必须存在（否则合并无锚点）
    let keep = crate::db::repositories::memories::get_by_mem_id(db, keep_mem_id)
        .map_err(|e| CoreError::Tool(format!("读取 keep 失败: {e}")))?
        .ok_or_else(|| CoreError::Tool(format!("merge_memories keep 不存在: {keep_mem_id}")))?;
    if drop_ids.contains(&keep_mem_id.to_string()) {
        return Err(CoreError::Tool(
            "merge_memories keep 不能同时在 drop 中".into(),
        ));
    }

    // 计算默认 salience = max(涉及记忆) 与 entities union
    let mut salience = keep.salience.clamp(1, 5);
    let mut entities: Vec<String> = keep.entities.clone();
    for drop_id in &drop_ids {
        let m = crate::db::repositories::memories::get_by_mem_id(db, drop_id)
            .map_err(|e| CoreError::Tool(format!("读取 drop 失败: {e}")))?;
        if let Some(d) = m {
            salience = salience.max(d.salience.clamp(1, 5));
            for e in d.entities {
                if !entities.contains(&e) {
                    entities.push(e);
                }
            }
        }
    }
    let merged_salience = args
        .get("merged_salience")
        .and_then(Value::as_i64)
        .unwrap_or(salience)
        .clamp(1, 5);

    let mut hidden: Vec<String> = Vec::new();
    for drop_id in &drop_ids {
        let ok =
            crate::db::repositories::memories::hide_by_mem_id(db, drop_id, Some(keep_mem_id), None)
                .map_err(|e| CoreError::Tool(format!("隐藏 drop 失败: {e}")))?;
        if ok {
            hidden.push(drop_id.clone());
        }
    }
    crate::db::repositories::memories::merge_update(
        db,
        keep_mem_id,
        &merged_content,
        args.get("merged_detail").and_then(Value::as_str),
        &entities,
        merged_salience,
    )
    .map_err(|e| CoreError::Tool(format!("更新 keep 失败: {e}")))?;

    Ok(json!({
        "ok": true,
        "keep_mem_id": keep_mem_id,
        "merged_salience": merged_salience,
        "hidden": hidden,
        "reason": args.get("reason").and_then(Value::as_str).unwrap_or(""),
    }))
}

/// downgrade_memory：降低记忆 salience（过时/非核心但未到合并程度的记忆）。
pub fn downgrade_memory_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "downgrade_memory 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let mem_id = args
        .get("mem_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| CoreError::Tool("downgrade_memory 缺 mem_id".into()))?;
    let new_salience = args
        .get("new_salience")
        .and_then(Value::as_i64)
        .ok_or_else(|| CoreError::Tool("downgrade_memory 缺 new_salience".into()))?
        .clamp(1, 5);
    let ok = crate::db::repositories::memories::update_salience(db, mem_id, new_salience)
        .map_err(|e| CoreError::Tool(format!("降级记忆失败: {e}")))?;
    if !ok {
        return Err(CoreError::Tool(format!(
            "downgrade_memory 记忆不存在: {mem_id}"
        )));
    }
    Ok(json!({
        "ok": true,
        "mem_id": mem_id,
        "new_salience": new_salience,
        "reason": args.get("reason").and_then(Value::as_str).unwrap_or(""),
    }))
}

/// skip_recognition：识别器专用停止信号——"已审阅，无需写入"。
pub fn skip_recognition_impl(_ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    Ok(json!({
        "ok": true,
        "skipped": true,
        "reason": args.get("reason").and_then(Value::as_str).unwrap_or(""),
    }))
}

/// skip_consolidation：整合器专用停止信号——"无重复、无过时条目"。
pub fn skip_consolidation_impl(_ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    Ok(json!({
        "ok": true,
        "skipped": true,
        "reason": args.get("reason").and_then(Value::as_str).unwrap_or(""),
    }))
}

// ─────────────────────────────────────────────────────────────
// 系统类
// ─────────────────────────────────────────────────────────────

/// set_agent_name：更新 agent 显示名/自称名（config 表，对齐 Node config.agent_name）。
pub fn set_agent_name_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "set_agent_name 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(CoreError::Tool("set_agent_name 缺 name".into()));
    }
    let chars = name.chars().count();
    if chars > 32 {
        return Err(CoreError::Tool(format!(
            "set_agent_name 名称超长（{chars} 字符，上限 32）"
        )));
    }
    for c in name.chars() {
        if !(c.is_alphanumeric() || c.is_whitespace() || c == '_' || c == '-') {
            return Err(CoreError::Tool(format!(
                "set_agent_name 名称含非法字符: {c}"
            )));
        }
    }
    crate::db::repositories::config::set_config(db, "agent_name", &name)
        .map_err(|e| CoreError::Tool(format!("写入配置失败: {e}")))?;
    Ok(json!({ "ok": true, "agent_name": name }))
}

/// set_location：记录用户当前城市/地区（config 表，供天气等功能使用）。
pub fn set_location_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "set_location 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let city = args
        .get("city")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if city.is_empty() {
        return Err(CoreError::Tool("set_location 缺 city".into()));
    }
    if city.chars().count() > 64 {
        return Err(CoreError::Tool(
            "set_location city 超长（上限 64 字符）".into(),
        ));
    }
    crate::db::repositories::config::set_config(db, "location_city", &city)
        .map_err(|e| CoreError::Tool(format!("写入配置失败: {e}")))?;
    Ok(json!({ "ok": true, "city": city }))
}

/// set_tick_interval：L2 调节自身思维节奏（对齐 Node ticker.js）。
/// 需要注入 tick 回调（`ex.set_tick_interval`），未接线时返回明确错误。
pub fn set_tick_interval_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(cb) = &ex.set_tick_interval else {
        return Err(CoreError::Tool(
            "set_tick_interval 未接线（当前运行时未提供节奏调整回调）".into(),
        ));
    };
    let seconds = args
        .get("seconds")
        .and_then(Value::as_u64)
        .ok_or_else(|| CoreError::Tool("set_tick_interval 缺 seconds".into()))?
        .clamp(0, 36_000);
    let ttl = args
        .get("ttl")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 100);
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let detail = cb(seconds, ttl, &reason)?;
    Ok(json!({ "ok": true, "seconds": seconds, "ttl": ttl, "detail": detail }))
}

/// find_tool：按自然语言描述检索工具目录，返回命中的可用工具（含 schema 名称）。
/// 对齐 Node find_tool 的"自感知按需激活"发现半。
pub fn find_tool_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if query.is_empty() {
        return Err(CoreError::Tool(
            "find_tool 缺 query：用一句话描述你需要做什么".into(),
        ));
    }
    // 关键词 → 工具映射（中文 + 英文）
    let catalog: &[(&str, &[&str])] = &[
        ("web_search", &["搜索", "查一下", "搜", "search", "news"]),
        (
            "web_read",
            &["网页", "阅读", "读取", "url", "read", "网页内容"],
        ),
        ("fetch_url", &["下载页面", "抓取", "fetch"]),
        ("browser_open", &["浏览器", "打开网页", "browser", "浏览"]),
        (
            "read_file",
            &["读文件", "读取文件", "打开文件", "read file"],
        ),
        (
            "write_file",
            &["写文件", "写入文件", "保存文件", "write", "创建文件"],
        ),
        ("list_dir", &["列目录", "查看目录", "list", "目录"]),
        (
            "exec_command",
            &[
                "运行",
                "执行命令",
                "命令",
                "command",
                "shell",
                "终端",
                "运行命令",
            ],
        ),
        (
            "exec_quick_command",
            &["快速命令", "pwd", "whoami", "quick", "命令", "运行"],
        ),
        ("list_processes", &["进程", "后台", "process", "任务"]),
        (
            "kill_process",
            &["结束进程", "杀进程", "停止进程", "kill", "stop"],
        ),
        ("set_reminder", &["提醒", "提醒我", "reminder", "日程"]),
        (
            "search_memory",
            &["回忆", "记忆", "我记得", "memory", "recall"],
        ),
        ("upsert_memory", &["记住", "写入记忆", "save", "记得"]),
        ("send_message", &["发消息", "回复", "send", "message"]),
        ("get_timestamp", &["时间", "现在几点", "time", "timestamp"]),
        ("ui_set", &["卡片", "面板", "界面", "ui", "显示"]),
        ("speak", &["说话", "语音", "朗读", "speak", "tts"]),
        (
            "generate_image",
            &["生成图片", "画图", "图片", "image", "插画"],
        ),
        ("generate_music", &["生成音乐", "作曲", "music", "歌曲"]),
        ("generate_lyrics", &["歌词", "lyrics"]),
        ("set_task", &["任务", "计划", "目标", "task", "todo"]),
        ("review_work", &["审查", "复查", "review", "检查"]),
        ("hotspot_mode", &["热点", "热搜", "trending", "hotspot"]),
        ("worldcup_mode", &["世界杯", "worldcup", "足球"]),
        ("typhoon_mode", &["台风", "typhoon", "天气"]),
        ("weather", &["天气", "weather", "气温"]),
        ("install_software", &["安装软件", "安装", "install", "卸载"]),
        ("download_file", &["下载", "download"]),
        (
            "collect_agents",
            &["agent", "协作", "委托", "claude", "codex"],
        ),
        ("delegate_to_agent", &["委托", "交给", "delegate"]),
        ("matter_create", &["事项", "账本", "matter", "任务单"]),
        ("set_agent_name", &["改名", "名字", "称呼", "rename"]),
        (
            "set_location",
            &["位置", "城市", "地区", "location", "city"],
        ),
        (
            "complete_startup_self_check",
            &["自检", "startup check", "体检"],
        ),
    ];
    let schemas = super::all_tool_schemas();
    let mut loaded: Vec<Value> = Vec::new();
    for (name, keywords) in catalog {
        if !ex.is_ready(name) {
            continue;
        }
        if keywords.iter().any(|k| query.contains(&k.to_lowercase())) {
            if let Some(s) = schemas.iter().find(|s| s.name == *name) {
                loaded.push(json!({
                    "name": s.name,
                    "description": s.description,
                }));
            }
        }
    }
    Ok(json!({
        "ok": true,
        "tool": "find_tool",
        "query": query,
        "count": loaded.len(),
        "loaded": loaded,
        "note": "找到即在本轮可用——直接调用即可，不必再 find_tool",
    }))
}

/// complete_startup_self_check：关闭一次性 L2 启动自检（config 表记录 summary/results）。
pub fn complete_startup_self_check_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "complete_startup_self_check 未接线（未注入 Db）".into(),
        ));
    };
    let summary = args
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if summary.is_empty() {
        return Err(CoreError::Tool(
            "complete_startup_self_check 缺 summary".into(),
        ));
    }
    let results = args.get("results").cloned().unwrap_or(Value::Null);
    let record = json!({
        "summary": summary,
        "results": results,
        "completed_at": crate::db::models::now_iso(),
    });
    crate::db::repositories::config::set_config(
        db,
        "startup_self_check",
        &serde_json::to_string(&record).map_err(|e| CoreError::Tool(format!("序列化失败: {e}")))?,
    )
    .map_err(|e| CoreError::Tool(format!("写入配置失败: {e}")))?;
    Ok(json!({ "ok": true, "persisted": true }))
}

// ─────────────────────────────────────────────────────────────
// 任务类（config 表持久化，对齐 Node task-manager.js）
// ─────────────────────────────────────────────────────────────

/// 读取当前任务（config 表 current_task / current_task_steps）。
fn load_task(db: &crate::db::Db) -> Result<(Option<String>, Vec<Value>)> {
    let task = crate::db::repositories::config::get_config(db, "current_task")
        .map_err(|e| CoreError::Tool(format!("读取任务失败: {e}")))?;
    let steps_raw = crate::db::repositories::config::get_config(db, "current_task_steps")
        .map_err(|e| CoreError::Tool(format!("读取任务步骤失败: {e}")))?;
    let steps: Vec<Value> = match steps_raw {
        Some(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        None => Vec::new(),
    };
    Ok((task, steps))
}

fn save_task(db: &crate::db::Db, task: Option<&str>, steps: &[Value]) -> Result<()> {
    match task {
        Some(t) => crate::db::repositories::config::set_config(db, "current_task", t)
            .map_err(|e| CoreError::Tool(format!("保存任务失败: {e}")))?,
        None => {
            let _ = crate::db::repositories::config::set_config(db, "current_task", "");
        }
    }
    crate::db::repositories::config::set_config(
        db,
        "current_task_steps",
        &serde_json::to_string(steps).map_err(|e| CoreError::Tool(format!("序列化失败: {e}")))?,
    )
    .map_err(|e| CoreError::Tool(format!("保存任务步骤失败: {e}")))?;
    Ok(())
}

/// set_task：启动多步任务（config 表持久化，重启恢复）。
pub fn set_task_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "set_task 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if description.is_empty() {
        return Err(CoreError::Tool("set_task 缺 description".into()));
    }
    let raw_steps = args
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Tool("set_task 缺 steps 数组".into()))?;
    if raw_steps.is_empty() {
        return Err(CoreError::Tool("set_task steps 不能为空".into()));
    }
    if raw_steps.len() > 50 {
        return Err(CoreError::Tool("set_task steps 超长（上限 50 步）".into()));
    }
    let steps: Vec<Value> = raw_steps
        .iter()
        .map(|s| {
            json!({
                "text": s.as_str().unwrap_or(""),
                "status": "pending",
                "note": "",
            })
        })
        .collect();
    save_task(db, Some(&description), &steps)?;
    Ok(json!({
        "ok": true,
        "task": description,
        "steps": steps,
        "restored_on_restart": true,
    }))
}

/// complete_task：标记当前任务全部完成，清除任务状态。
pub fn complete_task_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "complete_task 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let (task, _steps) = load_task(db)?;
    if task.is_none() || task.as_deref().unwrap_or("").is_empty() {
        return Err(CoreError::Tool("complete_task 当前无进行中的任务".into()));
    }
    save_task(db, None, &[])?;
    Ok(json!({
        "ok": true,
        "completed_task": task.unwrap_or_default(),
        "summary": args.get("summary").and_then(Value::as_str).unwrap_or(""),
        "status": "completed",
    }))
}

/// update_task_step：更新某一步的完成状态（done/failed/skipped）。
pub fn update_task_step_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "update_task_step 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let (task, mut steps) = load_task(db)?;
    if task.is_none() || task.as_deref().unwrap_or("").is_empty() {
        return Err(CoreError::Tool(
            "update_task_step 当前无进行中的任务".into(),
        ));
    }
    let step_index = args
        .get("step_index")
        .and_then(Value::as_i64)
        .ok_or_else(|| CoreError::Tool("update_task_step 缺 step_index".into()))?;
    if step_index < 0 || step_index as usize >= steps.len() {
        return Err(CoreError::Tool(format!(
            "update_task_step 越界: {step_index}（共 {} 步）",
            steps.len()
        )));
    }
    let status = args
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Tool("update_task_step 缺 status".into()))?;
    if !["done", "failed", "skipped"].contains(&status) {
        return Err(CoreError::Tool(format!(
            "update_task_step status 非法: {status}（允许 done/failed/skipped）"
        )));
    }
    let idx = step_index as usize;
    if let Some(step) = steps[idx].as_object_mut() {
        step.insert("status".into(), Value::String(status.into()));
        if let Some(note) = args.get("note").and_then(Value::as_str) {
            step.insert("note".into(), Value::String(note.into()));
        }
    }
    save_task(db, task.as_deref(), &steps)?;
    Ok(json!({
        "ok": true,
        "step_index": step_index,
        "status": status,
        "task": task.unwrap_or_default(),
    }))
}

// ─────────────────────────────────────────────────────────────
// 进程类（Windows tasklist/taskkill，Unix ps/kill）
// ─────────────────────────────────────────────────────────────

/// exec_quick_command：快速非交互命令（短超时档）。
/// 直接复用 exec_command 的执行逻辑（快路径，默认 10s 超时）。
pub fn exec_quick_command_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let mut merged = args.clone();
    let quick_timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 30);
    let obj = merged
        .as_object_mut()
        .ok_or_else(|| CoreError::Tool("exec_quick_command 参数必须是 JSON 对象".into()))?;
    // exec_command 内部读取 timeout_ms；quick 档把 timeout(秒) 换算为 ms
    obj.insert("timeout_ms".into(), Value::Number(quick_timeout.into()));
    if let Some(t) = obj.get("timeout") {
        if !t.is_null() {
            obj.remove("timeout");
        }
    }
    ex.exec_command_inner(&merged)
}

/// list_processes：列出当前运行进程（对齐 Node 的简化语义——本实现直接列 OS 进程，
/// 不维护后台进程表；后台进程语义由 exec_command 的 background 扩展后续承载）。
pub fn list_processes_impl(_ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let tail = args
        .get("tail")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 200) as usize;
    let processes = if cfg!(windows) {
        list_windows_processes()
    } else {
        list_unix_processes()
    }?;
    let limited: Vec<Value> = processes.into_iter().take(tail).collect();
    Ok(json!({
        "ok": true,
        "count": limited.len(),
        "processes": limited,
        "software_install_jobs": [],
        "note": "后台软件安装作业语义将在 install_software 接入后补充",
    }))
}

/// kill_process：按 PID 终止进程。
pub fn kill_process_impl(_ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let pid = args
        .get("pid")
        .and_then(Value::as_u64)
        .ok_or_else(|| CoreError::Tool("kill_process 缺 pid".into()))?;
    if pid == 0 {
        return Err(CoreError::Tool("kill_process pid 非法".into()));
    }
    let killed = if cfg!(windows) {
        use std::process::Command;
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| CoreError::Tool(format!("taskkill 启动失败: {e}")))?;
        status.success()
    } else {
        use std::process::Command;
        let status = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| CoreError::Tool(format!("kill 启动失败: {e}")))?;
        status.success()
    };
    if !killed {
        return Err(CoreError::Tool(format!(
            "kill_process 进程不存在或已退出: {pid}"
        )));
    }
    Ok(json!({
        "ok": true,
        "pid": pid,
        "stopped": true,
        "command": "",
    }))
}

#[cfg(windows)]
fn list_windows_processes() -> Result<Vec<Value>> {
    use std::process::Command;
    let out = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()
        .map_err(|e| CoreError::Tool(format!("tasklist 启动失败: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut processes = Vec::new();
    for line in text.lines().take(200) {
        let mut parts = line.split('"');
        let name = parts.nth(1).unwrap_or("").to_string();
        let pid = parts.nth(1).unwrap_or("").to_string();
        if name.is_empty() || pid.is_empty() {
            continue;
        }
        processes.push(json!({
            "pid": pid.parse::<u64>().unwrap_or(0),
            "command": name,
            "status": "running",
            "recent_output": "",
        }));
    }
    Ok(processes)
}

#[cfg(windows)]
fn list_unix_processes() -> Result<Vec<Value>> {
    Ok(Vec::new())
}

#[cfg(not(windows))]
fn list_unix_processes() -> Result<Vec<Value>> {
    use std::process::Command;
    let out = Command::new("ps")
        .args(["-eo", "pid,comm"])
        .output()
        .map_err(|e| CoreError::Tool(format!("ps 启动失败: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut processes = Vec::new();
    for line in text.lines().skip(1).take(200) {
        let mut parts = line.split_whitespace();
        let pid = parts.next().unwrap_or("").parse::<u64>().unwrap_or(0);
        let command = parts.next().unwrap_or("").to_string();
        if pid == 0 {
            continue;
        }
        processes.push(json!({
            "pid": pid,
            "command": command,
            "status": "running",
            "recent_output": "",
        }));
    }
    Ok(processes)
}

#[cfg(not(windows))]
fn list_windows_processes() -> Result<Vec<Value>> {
    Ok(Vec::new())
}

// ─────────────────────────────────────────────────────────────
// schemas
// ─────────────────────────────────────────────────────────────

/// 本批工具的 OpenAI schema（由 [`super::all_tool_schemas`] 追加）。
pub fn sys_tool_schemas() -> Vec<crate::llm::tools::ToolSchema> {
    use crate::llm::tools::{
        enum_param, integer_param, string_array_param, string_param, ToolSchema,
    };
    vec![
        ToolSchema::new(
            "upsert_memory",
            "批量插入或更新记忆节点（mem_id 去重：已存在=更新省略字段保留，新=插入）。识别器写入记忆前的必经工具。",
        )
        .required(
            "memories",
            json!({ "type": "array", "description": "记忆数组（每项含 mem_id/type/content/detail/title/entities/tags/salience/parent_mem_id）" }),
        ),
        ToolSchema::new(
            "probe_memory",
            "诊断探测：查看'如果现在检索 X，记忆层会返回什么'。无副作用。",
        )
        .required("query", string_param("自然语言探测查询")),
        ToolSchema::new(
            "recall_memory",
            "深度回忆相关记忆，返回结果并聚焦此主题影响下一轮注入。",
        )
        .required("query", string_param("要回忆的内容或主题"))
        .param("limit", integer_param("返回条数，默认 12，最大 30")),
        ToolSchema::new(
            "merge_memories",
            "合并多条语义重复记忆到 keep，drops 软隐藏（visibility=0，绝不物理删除）。",
        )
        .required("keep_mem_id", string_param("保留记忆的 mem_id"))
        .required("drop_mem_ids", string_array_param("要隐藏的记忆 mem_id 数组"))
        .required("merged_content", string_param("合并后的内容（≤200 字符）"))
        .param("merged_salience", integer_param("可选，覆盖默认 max(涉及) 的 salience")),
        ToolSchema::new(
            "downgrade_memory",
            "降低记忆 salience（过时/非核心但未到合并程度）。",
        )
        .required("mem_id", string_param("记忆 mem_id"))
        .required("new_salience", integer_param("新 salience（1-5）")),
        ToolSchema::new("skip_recognition", "识别器专用停止信号：'已审阅，无需写入'。")
            .param("reason", string_param("可选简短原因")),
        ToolSchema::new("skip_consolidation", "整合器专用停止信号：'无重复、无过时条目'。")
            .param("reason", string_param("可选简短原因")),
        ToolSchema::new("set_agent_name", "更新你的显示名/自称名（1-32 字符）。")
            .required("name", string_param("新名称，中英文/数字/空格/下划线/连字符")),
        ToolSchema::new("set_location", "记录用户当前城市/地区（供天气等位置相关功能）。")
            .required("city", string_param("城市名，如 北京 / Shanghai / London")),
        ToolSchema::new("set_tick_interval", "调整自身 TICK 节奏（seconds [0,36000]，ttl [1,100]）。")
            .required("seconds", integer_param("TICK 间隔秒数（0-36000）"))
            .param("ttl", integer_param("保持该节奏的成功自检心跳数（默认 10）"))
            .param("reason", string_param("可选简短原因")),
        ToolSchema::new(
            "find_tool",
            "按一句话描述检索工具目录，命中的工具本轮即可调用（无需再次 find_tool）。",
        )
        .required("query", string_param("一句话描述你要做的能力，中英文均可")),
        ToolSchema::new(
            "complete_startup_self_check",
            "关闭一次性 L2 启动自检，持久化诚实摘要/结果（基于真实证据，避免未来启动重复自检）。",
        )
        .required("summary", string_param("自检结果的简短可读摘要"))
        .param("results", crate::llm::tools::string_param("按能力的结果映射 JSON")),
        ToolSchema::new("set_task", "启动多步任务（持久化到配置，重启恢复；加速 TICK 节奏）。")
            .required("description", string_param("总体任务目标"))
            .required("steps", string_array_param("有序具体步骤（≤50 步）")),
        ToolSchema::new("complete_task", "标记当前任务全部完成，清除任务状态。")
            .param("summary", string_param("可选的完成摘要")),
        ToolSchema::new("update_task_step", "更新当前任务某步的完成状态（done/failed/skipped）。")
            .required("step_index", integer_param("步骤索引（从 0 开始）"))
            .required("status", enum_param("步骤状态", &["done", "failed", "skipped"]))
            .param("note", string_param("可选的步骤结果备注")),
        ToolSchema::new("exec_quick_command", "运行即时非交互命令（快速档，默认 10s 超时，最大 30s）。")
            .required("command", string_param("要执行的短命令"))
            .param("timeout", integer_param("超时秒数，默认 10，最大 30"))
            .param("cwd", string_param("沙箱内子目录（相对路径）")),
        ToolSchema::new("list_processes", "列出当前运行进程（含 PID 与命令名）。")
            .param("tail", integer_param("返回行数上限，默认 20，最大 200")),
        ToolSchema::new("kill_process", "按 PID 终止进程。")
            .required("pid", integer_param("要终止的进程 PID")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;
    use crate::db::Db;
    use crate::llm::tool_loop::ToolExecutor;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        open_database(dir.path().join("t.db")).unwrap()
    }

    fn executor(db: Db) -> NativeToolExecutor {
        NativeToolExecutor::new(std::env::temp_dir()).with_db(db)
    }

    #[test]
    fn upsert_memory_insert_then_update() {
        let db = test_db();
        let ex = executor(db);
        // 插入
        let r = ex
            .execute(
                "upsert_memory",
                &json!({ "memories": [{
                    "mem_id": "fact_rust_migration",
                    "type": "fact",
                    "title": "迁移",
                    "content": "用户正在把 Bailongma 从 Node 迁移到 Rust",
                    "salience": 4,
                    "entities": ["ID:000001"]
                }] }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["results"][0]["updated"], false, "{v}");
        // 再次写入同 mem_id → 更新
        let r2 = ex
            .execute(
                "upsert_memory",
                &json!({ "memories": [{
                    "mem_id": "fact_rust_migration",
                    "type": "fact",
                    "content": "迁移已完成核心工具层",
                    "salience": 3
                }] }),
            )
            .unwrap();
        let v2: Value = serde_json::from_str(&r2).unwrap();
        assert_eq!(v2["results"][0]["updated"], true, "{v2}");
        let got = crate::db::repositories::memories::get_by_mem_id(
            ex.db.as_ref().unwrap(),
            "fact_rust_migration",
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.content, "迁移已完成核心工具层");
    }

    #[test]
    fn upsert_memory_validates_batch() {
        let db = test_db();
        let ex = executor(db);
        // 空数组
        let r = ex.execute("upsert_memory", &json!({ "memories": [] }));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("不能为空"));
        // 缺 mem_id
        let r2 = ex.execute(
            "upsert_memory",
            &json!({ "memories": [{ "content": "x" }] }),
        );
        assert!(r2.is_err());
        assert!(r2.unwrap_err().to_string().contains("mem_id"));
        // type 非法
        let r3 = ex.execute(
            "upsert_memory",
            &json!({ "memories": [{ "mem_id": "m1", "type": "nope", "content": "x" }] }),
        );
        assert!(r3.is_err());
        // 无 db 时未接线
        let ex2 = NativeToolExecutor::new(std::env::temp_dir());
        let r4 = ex2.execute("upsert_memory", &json!({ "memories": [] }));
        assert!(r4.is_err());
        assert!(r4.unwrap_err().to_string().contains("未接线"));
    }

    #[test]
    fn probe_and_recall_work() {
        let db = test_db();
        crate::db::repositories::memories::insert_simple(&db, "fact", "用户喜欢冷萃咖啡").unwrap();
        let ex = executor(db);
        let r = ex
            .execute("probe_memory", &json!({ "query": "咖啡" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["count"], 1, "{v}");
        assert_eq!(v["matches"][0]["content"], "用户喜欢冷萃咖啡");
        // recall 同样可用
        let r2 = ex
            .execute("recall_memory", &json!({ "query": "咖啡" }))
            .unwrap();
        let v2: Value = serde_json::from_str(&r2).unwrap();
        assert_eq!(v2["count"], 1, "{v2}");
    }

    #[test]
    fn merge_memories_hides_drops() {
        let db = test_db();
        let ex = executor(db);
        ex.execute(
            "upsert_memory",
            &json!({ "memories": [
                { "mem_id": "m_keep", "type": "fact", "content": "用户喜欢咖啡", "salience": 3 },
                { "mem_id": "m_drop", "type": "fact", "content": "用户爱喝咖啡", "salience": 4 }
            ] }),
        )
        .unwrap();
        let r = ex
            .execute(
                "merge_memories",
                &json!({
                    "keep_mem_id": "m_keep",
                    "drop_mem_ids": ["m_drop"],
                    "merged_content": "用户喜欢并常喝咖啡"
                }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["merged_salience"], 4, "{v}"); // max(3,4)
        assert_eq!(v["hidden"], json!(["m_drop"]), "{v}");
        // drop 已隐藏
        let drop =
            crate::db::repositories::memories::get_by_mem_id(ex.db.as_ref().unwrap(), "m_drop")
                .unwrap()
                .unwrap();
        assert!(!drop.visibility);
        assert_eq!(drop.merged_into.as_deref(), Some("m_keep"));
        // keep 已更新
        let keep =
            crate::db::repositories::memories::get_by_mem_id(ex.db.as_ref().unwrap(), "m_keep")
                .unwrap()
                .unwrap();
        assert_eq!(keep.content, "用户喜欢并常喝咖啡");
    }

    #[test]
    fn downgrade_memory_updates_salience() {
        let db = test_db();
        let ex = executor(db);
        ex.execute(
            "upsert_memory",
            &json!({ "memories": [{ "mem_id": "m_stale", "type": "fact", "content": "旧信息", "salience": 5 }] }),
        )
        .unwrap();
        let r = ex
            .execute(
                "downgrade_memory",
                &json!({ "mem_id": "m_stale", "new_salience": 1 }),
            )
            .unwrap();
        assert!(
            serde_json::from_str::<Value>(&r).unwrap()["ok"]
                .as_bool()
                .unwrap_or(false),
            "{r}"
        );
        let got =
            crate::db::repositories::memories::get_by_mem_id(ex.db.as_ref().unwrap(), "m_stale")
                .unwrap()
                .unwrap();
        assert_eq!(got.salience, 1);
        // 不存在的 mem_id 报错
        let r2 = ex.execute(
            "downgrade_memory",
            &json!({ "mem_id": "m_ghost", "new_salience": 1 }),
        );
        assert!(r2.is_err());
    }

    #[test]
    fn set_agent_name_and_location_persist() {
        let db = test_db();
        let ex = executor(db);
        let r = ex
            .execute("set_agent_name", &json!({ "name": "小白龙" }))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&r).unwrap()["agent_name"],
            "小白龙"
        );
        let stored =
            crate::db::repositories::config::get_config(ex.db.as_ref().unwrap(), "agent_name")
                .unwrap()
                .unwrap();
        assert_eq!(stored, "小白龙");
        // 非法字符
        let r2 = ex.execute("set_agent_name", &json!({ "name": "bad!name" }));
        assert!(r2.is_err());
        // 位置
        let r3 = ex
            .execute("set_location", &json!({ "city": "北京" }))
            .unwrap();
        assert!(
            serde_json::from_str::<Value>(&r3).unwrap()["ok"]
                .as_bool()
                .unwrap_or(false),
            "{r3}"
        );
    }

    #[test]
    fn task_lifecycle_persists() {
        let db = test_db();
        let ex = executor(db);
        let r = ex
            .execute(
                "set_task",
                &json!({ "description": "迁移工具层", "steps": ["盘点", "实现", "测试"] }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["steps"].as_array().unwrap().len(), 3);
        // 更新步骤
        let r2 = ex
            .execute(
                "update_task_step",
                &json!({ "step_index": 0, "status": "done", "note": "已盘点 16 工具" }),
            )
            .unwrap();
        let v2: Value = serde_json::from_str(&r2).unwrap();
        assert_eq!(v2["ok"], true, "{v2}");
        assert_eq!(v2["step_index"], 0);
        // 越界
        let r3 = ex.execute(
            "update_task_step",
            &json!({ "step_index": 9, "status": "done" }),
        );
        assert!(r3.is_err());
        // 非法状态
        let r4 = ex.execute(
            "update_task_step",
            &json!({ "step_index": 0, "status": "maybe" }),
        );
        assert!(r4.is_err());
        // 完成
        let r5 = ex
            .execute("complete_task", &json!({ "summary": "完成" }))
            .unwrap();
        assert!(
            serde_json::from_str::<Value>(&r5).unwrap()["ok"]
                .as_bool()
                .unwrap_or(false),
            "{r5}"
        );
        // 已完成后再完成 → 报错
        let r6 = ex.execute("complete_task", &json!({}));
        assert!(r6.is_err());
    }

    #[test]
    fn skip_signals_return_ok() {
        let db = test_db();
        let ex = executor(db);
        let r = ex
            .execute("skip_recognition", &json!({ "reason": "无内容" }))
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&r).unwrap()["skipped"], true);
        let r2 = ex
            .execute("skip_consolidation", &json!({ "reason": "无重复" }))
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&r2).unwrap()["skipped"], true);
    }

    #[test]
    fn find_tool_matches_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let ex = executor(db);
        let r = ex
            .execute("find_tool", &json!({ "query": "运行命令" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        let names: Vec<&str> = v["loaded"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"exec_command"), "{names:?}");
        assert!(names.contains(&"exec_quick_command"), "{names:?}");
    }

    #[test]
    fn exec_quick_command_runs() {
        let dir = tempfile::tempdir().unwrap();
        let ex = NativeToolExecutor::new(dir.path().to_path_buf());
        let r = ex
            .execute("exec_quick_command", &json!({ "command": "echo quick-ok" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["exit_code"], 0);
    }

    #[test]
    fn kill_process_missing_pid_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ex = NativeToolExecutor::new(dir.path().to_path_buf());
        let r = ex.execute("kill_process", &json!({}));
        assert!(r.is_err());
    }

    #[test]
    fn schemas_registered() {
        let schemas = super::super::all_tool_schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        for tool in [
            "upsert_memory",
            "probe_memory",
            "recall_memory",
            "merge_memories",
            "downgrade_memory",
            "skip_recognition",
            "skip_consolidation",
            "set_agent_name",
            "set_location",
            "set_tick_interval",
            "find_tool",
            "complete_startup_self_check",
            "set_task",
            "complete_task",
            "update_task_step",
            "exec_quick_command",
            "list_processes",
            "kill_process",
        ] {
            assert!(names.contains(&tool), "缺少 schema: {tool}");
        }
    }
}
