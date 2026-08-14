//! 计划对齐补充工具：`collect_agents`（known_agents 表）、`remind`（reminders 表）
//! 与 `set_reminder`（创建提醒，收尾 A 级「生产无提醒写入方」缺口）。
//!
//! 背景：bailongma-multiagent-enhancement 计划的 9 工具清单为
//! get_timestamp / read_file / write_file / list_dir / exec_command /
//! search_memory / send_message / collect_agents / remind；R2 首版以
//! make_dir / delete_file 补足了文件类，本模块补齐其余两个（执行端读
//! `known_agents` / `reminders` 表，同步、可测）；审计 A 级遗留再补
//! `set_reminder` 打通提醒创建路径（LLM → reminders 表 pending → wakeup 到点投递）。
//!
//! 接线门与 search_memory / send_message 一致：无 Db 注入时返回明确错误，
//! 由 [`super::NativeToolExecutor::is_ready`] 决定是否暴露给 LLM。

use serde_json::{json, Value};

use super::NativeToolExecutor;
use crate::error::{CoreError, Result};

/// collect_agents 默认返回条数
const DEFAULT_AGENT_LIMIT: u32 = 20;
/// remind 默认返回条数
const DEFAULT_REMIND_LIMIT: u32 = 10;

/// collect_agents：列出已知本地 AI Agent（读 known_agents 表）。
///
/// - `include_unavailable=true` 时含不可用 Agent（available=0），默认只列可用；
/// - `limit` 截断返回条数（默认 20，最大 100）；
/// - 不触发本地探测（探测是启动期 `agents::collect_agents` 的职责，工具只读台账）。
pub fn collect_agents_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "collect_agents 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let include_unavailable = args
        .get("include_unavailable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_AGENT_LIMIT as u64)
        .min(100) as usize;

    let all = if include_unavailable {
        crate::db::repositories::agents::get_all_agents(db)
    } else {
        crate::db::repositories::agents::get_available_agents(db)
    }
    .map_err(|e| CoreError::Tool(format!("读取 Agent 列表失败: {e}")))?;

    let items: Vec<Value> = all
        .into_iter()
        .take(limit)
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "description": a.description,
                "available": a.available,
                "invoke_type": a.invoke_type,
                "version": a.version,
            })
        })
        .collect();

    Ok(json!({
        "ok": true,
        "count": items.len(),
        "agents": items,
    }))
}

/// remind：查询到期待触发提醒（读 reminders 表，`status='pending' AND due_at <= now`）。
///
/// - `action` 当前仅支持 `list`（默认）：列出到期提醒（不消费，不标记 fired）；
/// - `now` 可显式传入 ISO 时间用于测试/回放，缺省取本地当前时间；
/// - `limit` 截断返回条数（默认 10，最大 50）。
pub fn remind_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "remind 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("list");
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_REMIND_LIMIT as u64)
        .min(50) as usize;
    let now = args
        .get("now")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| chrono::Local::now().to_rfc3339());

    match action {
        "list" | "due" => {
            let due = crate::db::repositories::reminders::due_reminders(db, &now)
                .map_err(|e| CoreError::Tool(format!("读取提醒失败: {e}")))?;
            let items: Vec<Value> = due
                .into_iter()
                .take(limit)
                .map(|r| {
                    json!({
                        "id": r.id,
                        "due_at": r.due_at,
                        "task": r.task,
                        "user_id": r.user_id,
                    })
                })
                .collect();
            Ok(json!({
                "ok": true,
                "action": action,
                "count": items.len(),
                "reminders": items,
            }))
        }
        other => Err(CoreError::Tool(format!("remind 未知 action: {other}"))),
    }
}

/// 默认用户 ID（单用户部署，与现有 remind 测试数据一致；工具层不向 LLM 开放 user_id）。
const DEFAULT_USER_ID: &str = "ID:000001";
/// task 长度上限（防 LLM 幻觉内容撑爆提醒表 / 触发消息）
const MAX_TASK_LEN: usize = 500;

/// set_reminder：创建一条到期提醒（写 reminders 表，status='pending'）。
///
/// - `due_at`：ISO 8601 到期时间（必填，任意时区；存储前归一为 UTC RFC3339，
///   与 D1 修复后的比较语义一致——字典序即真实时间序）；
/// - `task`：提醒内容（必填，非空，≤500 字符）；
/// - 写入后到点由 wakeup 轮触发投递（system_message），可用 `remind` 查询。
///
/// 低风险落库（Medium / MemoryWrite / Schedule），与 send_message 同档不要求人工审批；
/// 无 Db 注入时返回明确错误（由 [`super::NativeToolExecutor::is_ready`] 决定是否暴露）。
pub fn set_reminder_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "set_reminder 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let raw_due = args
        .get("due_at")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Tool("set_reminder 缺 due_at".into()))?;
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Tool("set_reminder 缺 task".into()))?;
    let task = task.trim();
    if task.is_empty() {
        return Err(CoreError::Tool("set_reminder task 不能为空".into()));
    }
    if task.chars().count() > MAX_TASK_LEN {
        return Err(CoreError::Tool(format!(
            "set_reminder task 超长（{} 字符，上限 {MAX_TASK_LEN}）",
            task.chars().count()
        )));
    }
    // 时间归一：任意时区 ISO → UTC RFC3339（`Z` 后缀，与库内 now_iso()/D1 比较语义一致）
    let due_utc = chrono::DateTime::parse_from_rfc3339(raw_due)
        .map_err(|e| CoreError::Tool(format!("set_reminder due_at 非法: {e}")))?
        .to_utc()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let id = crate::db::repositories::reminders::insert_reminder(
        db,
        DEFAULT_USER_ID,
        &due_utc,
        task,
        "set_reminder",
    )
    .map_err(|e| CoreError::Tool(format!("写入提醒失败: {e}")))?;
    Ok(json!({
        "ok": true,
        "id": id,
        "due_at": due_utc,
        "task": task,
        "status": "pending",
    }))
}

/// 补充工具的 OpenAI schema（由 [`super::all_tool_schemas`] 追加）。
pub fn extra_tool_schemas() -> Vec<crate::llm::tools::ToolSchema> {
    use crate::llm::tools::{boolean_param, enum_param, integer_param, ToolSchema};
    vec![
        ToolSchema::new(
            "collect_agents",
            "列出本机已知 AI Agent（known_agents 表；默认仅可用，不触发探测）",
        )
        .param("include_unavailable", boolean_param("是否包含不可用 Agent，默认 false"))
        .param("limit", integer_param("返回条数，默认 20，最大 100")),
        ToolSchema::new(
            "remind",
            "查询到期待触发的提醒（reminders 表 pending 且已到期；不消费）",
        )
        .param("action", enum_param("操作", &["list"]))
        .param("limit", integer_param("返回条数，默认 10，最大 50"))
        .param("now", crate::llm::tools::string_param("当前时间 ISO 注入（测试/调试用，默认取系统时间）")),
        ToolSchema::new(
            "set_reminder",
            "创建一条到期提醒（写入 reminders 表；到点后自动触发投递，可用 remind 查询）",
        )
        .required("due_at", crate::llm::tools::string_param("ISO 8601 到期时间（任意时区，存储归一为 UTC）"))
        .required("task", crate::llm::tools::string_param("提醒内容（非空，≤500 字符）")),
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

    fn insert_agent(db: &Db, id: &str, name: &str, available: i64) {
        let conn = db.conn();
        conn.execute(
            "INSERT INTO known_agents
               (id, name, description, available, version, invoke_type, invoke_cmd,
                invoke_args, notes, docs_url, docs_search_query, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                id,
                name,
                "测试描述",
                available,
                "1.0",
                "cli",
                "echo",
                "[]",
                "",
                "",
                "",
                "2026-08-10T00:00:00Z"
            ],
        )
        .unwrap();
    }

    fn insert_reminder(db: &Db, due_at: &str, task: &str, status: &str) -> i64 {
        let conn = db.conn();
        conn.execute(
            "INSERT INTO reminders (user_id, due_at, task, system_message, status, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "ID:000001",
                due_at,
                task,
                format!("sys:{task}"),
                status,
                "test"
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn collect_agents_requires_db() {
        let dir = tempfile::tempdir().unwrap();
        let ex = NativeToolExecutor::new(dir.path().to_path_buf());
        let r = ex.execute("collect_agents", &json!({}));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("未接线"));
        assert!(!ex.is_ready("collect_agents"));
    }

    #[test]
    fn collect_agents_lists_known() {
        let db = test_db();
        insert_agent(&db, "agent-1", "Codex", 1);
        insert_agent(&db, "agent-2", "Claude", 0);
        let ex = executor(db);
        assert!(ex.is_ready("collect_agents"));

        // 默认只看可用
        let r = ex.execute("collect_agents", &json!({})).unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["count"], 1, "{v}");
        assert_eq!(v["agents"][0]["id"], "agent-1");
        assert_eq!(v["agents"][0]["name"], "Codex");

        // include_unavailable=true 全量
        let r2 = ex
            .execute("collect_agents", &json!({ "include_unavailable": true }))
            .unwrap();
        let v2: Value = serde_json::from_str(&r2).unwrap();
        assert_eq!(v2["count"], 2, "{v2}");
    }

    #[test]
    fn remind_requires_db() {
        let dir = tempfile::tempdir().unwrap();
        let ex = NativeToolExecutor::new(dir.path().to_path_buf());
        let r = ex.execute("remind", &json!({}));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("未接线"));
        assert!(!ex.is_ready("remind"));
    }

    #[test]
    fn remind_lists_due_only() {
        let db = test_db();
        insert_reminder(&db, "2026-08-10T06:00:00+08:00", "早提醒", "pending");
        insert_reminder(&db, "2026-08-11T08:00:00+08:00", "未来提醒", "pending");
        insert_reminder(&db, "2026-08-09T08:00:00+08:00", "已触发", "fired");
        let ex = executor(db);
        assert!(ex.is_ready("remind"));

        let r = ex
            .execute("remind", &json!({ "now": "2026-08-10T07:00:00+08:00" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["count"], 1, "{v}");
        assert_eq!(v["reminders"][0]["task"], "早提醒");
        // 只读不消费：状态保持 pending
        let n: i64 = ex
            .db
            .as_ref()
            .unwrap()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM reminders WHERE status='pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);

        // 未知 action 报错
        let r2 = ex.execute("remind", &json!({ "action": "fire" }));
        assert!(r2.is_err());
    }

    #[test]
    fn extra_schemas_shape() {
        let schemas = extra_tool_schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"collect_agents"));
        assert!(names.contains(&"remind"));
        assert!(names.contains(&"set_reminder"));
        for s in &schemas {
            let v = s.to_openai_value();
            assert_eq!(v["type"], "function");
            assert_eq!(v["function"]["name"], s.name);
            assert_eq!(v["function"]["parameters"]["type"], "object");
        }
    }

    // ── set_reminder ──

    #[test]
    fn set_reminder_requires_db() {
        let dir = tempfile::tempdir().unwrap();
        let ex = NativeToolExecutor::new(dir.path().to_path_buf());
        let r = ex.execute("set_reminder", &json!({ "due_at": "2026-08-12T08:00:00Z", "task": "开会" }));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("未接线"));
        assert!(!ex.is_ready("set_reminder"));
    }

    #[test]
    fn set_reminder_creates_pending_then_due() {
        let db = test_db();
        let ex = executor(db);
        assert!(ex.is_ready("set_reminder"));

        // 创建未来 1 天提醒
        let r = ex
            .execute(
                "set_reminder",
                &json!({ "due_at": "2026-08-11T08:00:00+08:00", "task": "项目周报" }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["task"], "项目周报");
        assert_eq!(v["status"], "pending");
        assert_eq!(v["due_at"], "2026-08-11T00:00:00Z", "{v}"); // +08:00 → UTC 归一
        let id: i64 = v["id"].as_i64().unwrap();
        assert!(id > 0);

        // 到点前 remind 查不到
        let r2 = ex
            .execute("remind", &json!({ "now": "2026-08-10T23:00:00Z" }))
            .unwrap();
        let v2: Value = serde_json::from_str(&r2).unwrap();
        assert_eq!(v2["count"], 0, "{v2}");

        // 到点后 remind 查到且不消费（2026-08-11T09:00:00+08:00 = 01:00Z > due 00:00Z）
        let r3 = ex
            .execute("remind", &json!({ "now": "2026-08-11T09:00:00+08:00" }))
            .unwrap();
        let v3: Value = serde_json::from_str(&r3).unwrap();
        assert_eq!(v3["count"], 1, "{v3}");
        assert_eq!(v3["reminders"][0]["id"], id);
        assert_eq!(v3["reminders"][0]["user_id"], "ID:000001");
        let status: String = ex
            .db
            .as_ref()
            .unwrap()
            .conn()
            .query_row(
                "SELECT status FROM reminders WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
        // source 标记创建来源
        let source: String = ex
            .db
            .as_ref()
            .unwrap()
            .conn()
            .query_row(
                "SELECT source FROM reminders WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, "set_reminder");
    }

    #[test]
    fn set_reminder_rejects_bad_input() {
        let db = test_db();
        let ex = executor(db);
        // 非法时间
        let r = ex.execute(
            "set_reminder",
            &json!({ "due_at": "不是日期", "task": "x" }),
        );
        assert!(r.is_err(), "非法时间应拒绝: {r:?}");
        assert!(r.unwrap_err().to_string().contains("due_at 非法"));
        // 空 task
        let r2 = ex.execute(
            "set_reminder",
            &json!({ "due_at": "2026-08-12T08:00:00Z", "task": "   " }),
        );
        assert!(r2.is_err());
        assert!(r2.unwrap_err().to_string().contains("不能为空"));
        // 超长 task
        let long = "长".repeat(MAX_TASK_LEN + 1);
        let r3 = ex.execute(
            "set_reminder",
            &json!({ "due_at": "2026-08-12T08:00:00Z", "task": long }),
        );
        assert!(r3.is_err());
        assert!(r3.unwrap_err().to_string().contains("超长"));
        // schema 层：缺必填参数
        let r4 = ex.execute("set_reminder", &json!({ "task": "x" }));
        assert!(r4.is_err());
    }
}
