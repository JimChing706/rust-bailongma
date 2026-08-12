//! matter 账本工具：`matter_create`（创建事项）与 `matter_query`（查询事项/幽灵信号）。
//!
//! 背景：PHILOSOPHY_MULTI_AGENT_MATTER.md 七命题已在 M5 通过 [`crate::matter`] 业务层
//! 全部落地（验收强制 / 意图原句 / 可加性声明 / 三主体分离 / 证据 / 五决策点 / 四态
//! 死亡），但工具循环未暴露任何 matter 能力——账本建成但 agent 够不着。本模块补齐
//! 活性缺口：
//! - `matter_create`：创建事项，走 `matter::create` 全部规则（无验收标准拒绝、验证者
//!   不得是执行者、父事项必须存在、子事项必须声明可加性），违反即报错。
//! - `matter_query`：只读查询。`action` 支持 `active`（进行中）/ `ghosts`（幽灵候选
//!   信号）/ `by_id`（详情）/ `events`（事件流）/ `children`（子事项）。
//!
//! 状态流转（start / evidence / verify / cancel / shelve）不暴露：五决策点默认全归
//! 人类（delegation_map 全 false），agent 只有在显式委托后才能推进事项——见
//! [`crate::matter::decision_allowed`]。幽灵候选只出信号不自动终止（人类决策）。

use serde_json::{json, Value};

use super::NativeToolExecutor;
use crate::error::{CoreError, Result};

/// matter_query 支持的 action 集合（schema enum 与分派共用）。
const MATTER_QUERY_ACTIONS: &[&str] = &["active", "ghosts", "by_id", "events", "children"];

/// 幽灵默认阈值：30 天无进展（SQLite datetime 修饰符，与 repo 时间格式一致）。
const GHOST_STALE_SQL: &str = "-30 day";

/// 取可选字符串参数（空串视为未传）。
fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// matter_create：创建事项（写）。返回 `{ id, status: "open" }`。
pub fn matter_create_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "matter_create 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let id = crate::matter::create(
        db,
        &s("title"),
        &s("expectation"),
        &s("current_state"),
        &s("gap_desc"),
        &s("acceptance_criteria"),
        &s("creator_id"),
        opt_str(args, "executor_id"),
        opt_str(args, "verifier_id"),
        args.get("parent_id").and_then(Value::as_i64),
        &s("intent_original"),
        &s("additivity_decl"),
    )
    .map_err(|e| CoreError::Tool(format!("创建事项失败: {e}")))?;
    Ok(json!({ "id": id, "status": "open" }))
}

/// matter_query：只读查询事项账本。
pub fn matter_query_impl(ex: &NativeToolExecutor, args: &Value) -> Result<Value> {
    let Some(db) = &ex.db else {
        return Err(CoreError::Tool(
            "matter_query 未接线（未注入 Db，当前轮不可用）".into(),
        ));
    };
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("active");
    let id = args.get("id").and_then(Value::as_i64);
    match action {
        "active" => {
            let rows = crate::matter::list_active(db)
                .map_err(|e| CoreError::Tool(format!("读取进行中事项失败: {e}")))?;
            Ok(json!({
                "action": "active",
                "count": rows.len(),
                "matters": rows.into_iter().map(matter_to_json).collect::<Vec<_>>(),
            }))
        }
        "ghosts" => {
            let stale = opt_str(args, "stale_before")
                .map(|s| s.to_string())
                .unwrap_or(default_stale(db)?);
            let ghosts = crate::matter::detect_ghosts(db, &stale)
                .map_err(|e| CoreError::Tool(format!("幽灵检测失败: {e}")))?;
            Ok(json!({
                "action": "ghosts",
                "stale_before": stale,
                "count": ghosts.len(),
                "ghosts": ghosts
                    .into_iter()
                    .map(|g| json!({
                        "id": g.id,
                        "title": g.title,
                        "updated_at": g.updated_at,
                        "reason": g.reason,
                    }))
                    .collect::<Vec<_>>(),
            }))
        }
        "by_id" => {
            let Some(mid) = id else {
                return Err(CoreError::Tool(
                    "matter_query action=by_id 需要 id 参数".into(),
                ));
            };
            match crate::db::repositories::matters::get(db, mid)
                .map_err(|e| CoreError::Tool(format!("读取事项失败: {e}")))?
            {
                Some(row) => Ok(json!({ "action": "by_id", "matter": matter_to_json(row) })),
                None => Ok(json!({ "action": "by_id", "matter": null, "not_found": true })),
            }
        }
        "events" => {
            let Some(mid) = id else {
                return Err(CoreError::Tool(
                    "matter_query action=events 需要 id 参数".into(),
                ));
            };
            let evs = crate::db::repositories::matters::list_events(db, mid)
                .map_err(|e| CoreError::Tool(format!("读取事件流失败: {e}")))?;
            Ok(json!({
                "action": "events",
                "matter_id": mid,
                "count": evs.len(),
                "events": evs
                    .into_iter()
                    .map(|e| json!({
                        "id": e.id,
                        "event_type": e.event_type,
                        "from_status": e.from_status,
                        "to_status": e.to_status,
                        "reason": e.reason,
                        "actor": e.actor,
                        "created_at": e.created_at,
                    }))
                    .collect::<Vec<_>>(),
            }))
        }
        "children" => {
            let Some(mid) = id else {
                return Err(CoreError::Tool(
                    "matter_query action=children 需要 id 参数".into(),
                ));
            };
            let rows = crate::db::repositories::matters::list_children(db, mid)
                .map_err(|e| CoreError::Tool(format!("读取子事项失败: {e}")))?;
            Ok(json!({
                "action": "children",
                "parent_id": mid,
                "count": rows.len(),
                "matters": rows.into_iter().map(matter_to_json).collect::<Vec<_>>(),
            }))
        }
        other => Err(CoreError::Tool(format!(
            "matter_query 未知 action: {other}（支持: {}）",
            MATTER_QUERY_ACTIONS.join(" | ")
        ))),
    }
}

/// 幽灵默认阈值：SQLite 计算 now - 30 day（复用 repo 的 datetime 时间格式，零新依赖）。
fn default_stale(db: &crate::db::Db) -> Result<String> {
    let sql = format!("SELECT datetime('now', '{GHOST_STALE_SQL}')");
    db.conn()
        .query_row(&sql, [], |r| r.get(0))
        .map_err(|e| CoreError::Tool(format!("计算幽灵阈值失败: {e}")))
}

/// MatterRow → JSON 摘要（给决策所需最小集，不吐 signals 全文等冗余）。
fn matter_to_json(m: crate::db::repositories::matters::MatterRow) -> Value {
    json!({
        "id": m.id,
        "title": m.title,
        "status": m.status,
        "creator_id": m.creator_id,
        "executor_id": m.executor_id,
        "verifier_id": m.verifier_id,
        "parent_id": m.parent_id,
        "acceptance_criteria": m.acceptance_criteria,
        "intent_original": m.intent_original,
        "additivity_decl": m.additivity_decl,
        "evidence": m.evidence,
        "death_reason": m.death_reason,
        "self_verified": m.self_verified,
        "created_at": m.created_at,
        "updated_at": m.updated_at,
    })
}

/// matter 工具 schema 注册（并入 all_tool_schemas）。
pub fn matter_tool_schemas() -> Vec<crate::llm::tools::ToolSchema> {
    use crate::llm::tools::{enum_param, integer_param, string_param, ToolSchema};
    vec![
        ToolSchema::new(
            "matter_create",
            "创建事项（多Agent 事项账本）：记录期望状态与当前状态的差距，必须带验收标准（无验收标准的只是愿望，拒绝创建）。验证者不得同时是执行者；子事项必须声明可加性还原关系（all_completed | any_completed）。",
        )
        .required("title", string_param("事项标题"))
        .required("acceptance_criteria", string_param("验收标准（达成判定依据，非空）"))
        .required("intent_original", string_param("意图原句（用户原话，锚定意图漂移度量）"))
        .param("expectation", string_param("期望状态 S"))
        .param("current_state", string_param("当前状态"))
        .param("gap_desc", string_param("差距描述"))
        .param("creator_id", string_param("发起者 ID（默认 ID:000001 用户主体）"))
        .param("executor_id", string_param("执行者 ID（可选）"))
        .param("verifier_id", string_param("验证者 ID（可选，不得等于执行者）"))
        .param("parent_id", integer_param("父事项 id（可选；子事项必须同时传 additivity_decl）"))
        .param(
            "additivity_decl",
            enum_param("子事项与母项验收判据的还原关系", &["all_completed", "any_completed"]),
        ),
        ToolSchema::new(
            "matter_query",
            "查询多Agent 事项账本（只读）：active 进行中事项 / ghosts 幽灵候选（无主+超时无进展+无验收判据，只出信号不自动终止）/ by_id 详情 / events 事件流 / children 子事项。",
        )
        .param("action", enum_param("查询类型", MATTER_QUERY_ACTIONS))
        .param("id", integer_param("事项 id（by_id / events / children 需要）"))
        .param(
            "stale_before",
            string_param("幽灵判定阈值：ISO 时间，早于该时刻无进展即 idle（不传默认 30 天前）"),
        ),
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

    fn create_ok(ex: &NativeToolExecutor) -> i64 {
        let r = ex
            .execute(
                "matter_create",
                &json!({
                    "title": "测试事项",
                    "expectation": "S",
                    "current_state": "C",
                    "gap_desc": "C != S",
                    "acceptance_criteria": "全部单测通过",
                    "intent_original": "把这事做完",
                    "executor_id": "exec-1",
                    "verifier_id": "ver-1",
                }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["status"], "open");
        v["id"].as_i64().unwrap()
    }

    #[test]
    fn matter_requires_db() {
        let dir = tempfile::tempdir().unwrap();
        let ex = NativeToolExecutor::new(dir.path().to_path_buf());
        for tool in ["matter_create", "matter_query"] {
            let args = if tool == "matter_create" {
                json!({ "title": "t", "acceptance_criteria": "c", "intent_original": "i" })
            } else {
                json!({})
            };
            let r = ex.execute(tool, &args);
            assert!(r.is_err());
            assert!(r.unwrap_err().to_string().contains("未接线"));
            assert!(!ex.is_ready(tool));
        }
    }

    #[test]
    fn matter_create_ok_and_by_id() {
        let db = test_db();
        let ex = executor(db);
        assert!(ex.is_ready("matter_create"));

        let id = create_ok(&ex);

        // by_id 回读落库内容
        let r = ex
            .execute("matter_query", &json!({ "action": "by_id", "id": id }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["matter"]["title"], "测试事项");
        assert_eq!(v["matter"]["status"], "open");
        assert_eq!(v["matter"]["executor_id"], "exec-1");
        assert_eq!(v["matter"]["verifier_id"], "ver-1");
        assert_eq!(v["matter"]["acceptance_criteria"], "全部单测通过");
        assert!(!v["matter"]["self_verified"].as_bool().unwrap());
    }

    #[test]
    fn matter_create_rejects_no_criteria() {
        let db = test_db();
        let ex = executor(db);
        let r = ex.execute(
            "matter_create",
            &json!({ "title": "愿望", "acceptance_criteria": "", "intent_original": "希望" }),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("验收标准"));
    }

    #[test]
    fn matter_create_rejects_self_verify() {
        let db = test_db();
        let ex = executor(db);
        let r = ex.execute(
            "matter_create",
            &json!({
                "title": "t",
                "acceptance_criteria": "c",
                "intent_original": "i",
                "executor_id": "same",
                "verifier_id": "same",
            }),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("验证者"));
    }

    #[test]
    fn matter_create_rejects_bad_parent() {
        let db = test_db();
        let ex = executor(db);
        let r = ex.execute(
            "matter_create",
            &json!({
                "title": "子",
                "acceptance_criteria": "c",
                "intent_original": "i",
                "parent_id": 999,
                "additivity_decl": "all_completed",
            }),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("父事项"));
    }

    #[test]
    fn matter_query_active_lists() {
        let db = test_db();
        let ex = executor(db);
        create_ok(&ex);

        let r = ex
            .execute("matter_query", &json!({ "action": "active" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["action"], "active");
        assert_eq!(v["count"], 1, "{v}");
        assert_eq!(v["matters"][0]["title"], "测试事项");
    }

    #[test]
    fn matter_query_ghosts_detects() {
        let db = test_db();
        // 业务层 create 强制验收标准，幽灵只能来自遗留/手工数据：直接插一条无主无验收的旧事项
        db.conn()
            .execute(
                "INSERT INTO matters (title, expectation, acceptance_criteria, creator_id, intent_original, status, updated_at)
                 VALUES ('遗留事项', '遗留期望', '', 'ID:000001', '遗留', 'open', '2020-01-01 00:00:00')",
                [],
            )
            .unwrap();
        let ex = executor(db);

        let r = ex
            .execute(
                "matter_query",
                &json!({ "action": "ghosts", "stale_before": "2026-01-01 00:00:00" }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["count"], 1, "{v}");
        assert_eq!(v["ghosts"][0]["title"], "遗留事项");
        // 信号已落台账（ghost_candidate）
        let n: i64 = ex
            .db
            .as_ref()
            .unwrap()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM matters WHERE signals LIKE '%ghost_candidate%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        // 默认阈值（30 天前）：首次检测落信号刷新了 updated_at（记录=有动静），重置后再验
        ex.db
            .as_ref()
            .unwrap()
            .conn()
            .execute(
                "UPDATE matters SET updated_at = '2020-01-01 00:00:00' WHERE title = '遗留事项'",
                [],
            )
            .unwrap();
        let r2 = ex.execute("matter_query", &json!({ "action": "ghosts" })).unwrap();
        let v2: Value = serde_json::from_str(&r2).unwrap();
        assert_eq!(v2["count"], 1, "{v2}");
        assert!(v2["stale_before"].as_str().unwrap().contains("20"));
    }

    #[test]
    fn matter_query_events_and_children() {
        let db = test_db();
        let ex = executor(db);
        let parent = create_ok(&ex);

        // events：新事项尚无事件，空列表即可
        let r = ex
            .execute("matter_query", &json!({ "action": "events", "id": parent }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["action"], "events");
        assert_eq!(v["count"], 0, "{v}");

        // children：挂一个子事项
        let r = ex
            .execute(
                "matter_create",
                &json!({
                    "title": "子事项",
                    "acceptance_criteria": "子验收",
                    "intent_original": "子意图",
                    "parent_id": parent,
                    "additivity_decl": "all_completed",
                }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        let child = v["id"].as_i64().unwrap();

        let r2 = ex
            .execute("matter_query", &json!({ "action": "children", "id": parent }))
            .unwrap();
        let v2: Value = serde_json::from_str(&r2).unwrap();
        assert_eq!(v2["count"], 1, "{v2}");
        assert_eq!(v2["matters"][0]["id"], child);
        assert_eq!(v2["matters"][0]["additivity_decl"], "all_completed");
    }

    #[test]
    fn matter_query_unknown_action() {
        let db = test_db();
        let ex = executor(db);
        let r = ex.execute("matter_query", &json!({ "action": "fire" }));
        assert!(r.is_err());
        // P2-2 schema enum 先行 fail-closed（fire 不在允许集合）；impl 内"未知 action"为纵深防御
        assert!(r.unwrap_err().to_string().contains("不在允许集合"));
    }

    #[test]
    fn matter_schemas_shape() {
        let schemas = matter_tool_schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"matter_create"));
        assert!(names.contains(&"matter_query"));
        for s in &schemas {
            let v = s.to_openai_value();
            assert_eq!(v["type"], "function");
            assert_eq!(v["function"]["name"], s.name);
            assert_eq!(v["function"]["parameters"]["type"], "object");
        }
    }
}
