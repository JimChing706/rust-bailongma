//! 全工具参数 schema 校验（P2-2）。
//!
//! 在 [`crate::tools::NativeToolExecutor::execute`] 分发前统一校验，fail-closed：
//! - **未知参数一律拒绝**——防 LLM 幻觉参数被各工具内部静默忽略；
//! - **必填缺失 / 类型不符 / enum 越界一律拒绝**——替代各工具内部
//!   `unwrap_or(默认值)` 的静默兜底（例如 `get_timestamp` 的 format 传
//!   `"weird"` 原来会悄悄走 iso 分支，现在直接报错）；
//! - 数组参数校验 `items.type`（若 schema 声明）。
//!
//! 校验只依赖 schema 注册表（`crate::tools::all_tool_schemas`），不触碰
//! 文件系统、无副作用、可重复执行——与 PolicyEngine 同款纯决策约束。

use serde_json::{Map, Value};

use crate::error::{CoreError, Result};
use crate::llm::tools::ToolSchema;
use crate::tools::all_tool_schemas;

/// 按工具名查找 schema 并校验参数。
pub fn validate_args(name: &str, args: &Value) -> Result<()> {
    let schema = all_tool_schemas()
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| CoreError::Tool(format!("未知工具（无法校验）: {name}")))?;
    validate_against(&schema, args)
}

/// 对照一个 schema 校验参数对象。
pub fn validate_against(schema: &ToolSchema, args: &Value) -> Result<()> {
    // 1. 参数必须是 JSON 对象
    let obj = args.as_object().ok_or_else(|| {
        CoreError::Tool(format!(
            "{} 参数必须是 JSON 对象，实际: {}",
            schema.name,
            arg_type(args)
        ))
    })?;

    // 2. 读取 schema 声明（properties / required）
    let props: Map<String, Value> = schema
        .parameters
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required: Vec<String> = schema
        .parameters
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    // 3. 必填检查（在场且非 null）
    for r in &required {
        match obj.get(r) {
            None => {
                return Err(CoreError::Tool(format!(
                    "{} 缺少必填参数: {r}",
                    schema.name
                )));
            }
            Some(v) if v.is_null() => {
                return Err(CoreError::Tool(format!(
                    "{} 必填参数 {r} 不能为 null",
                    schema.name
                )));
            }
            Some(_) => {}
        }
    }

    // 4. 逐个在场参数：类型 + enum 校验；未声明参数一律拒绝
    for (k, v) in obj {
        let prop = props
            .get(k)
            .ok_or_else(|| CoreError::Tool(format!("{} 未知参数: {k}", schema.name)))?;
        check_value(&schema.name, k, v, prop)?;
    }
    Ok(())
}

/// 单参数校验：type / enum / 数组 items。
fn check_value(tool: &str, key: &str, v: &Value, prop: &Value) -> Result<()> {
    if let Some(t) = prop.get("type").and_then(Value::as_str) {
        let ok = match t {
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            "boolean" => v.is_boolean(),
            "array" => v.is_array(),
            // schema 自身声明了未知 type：不校验（属 schema 编写问题，不属调用方过错）
            _ => true,
        };
        if !ok {
            return Err(CoreError::Tool(format!(
                "{tool} 参数 {key} 类型应为 {t}，实际: {}",
                arg_type(v)
            )));
        }
    }

    if let Some(enum_vals) = prop.get("enum").and_then(Value::as_array) {
        if !enum_vals.contains(v) {
            let allowed = enum_vals
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(CoreError::Tool(format!(
                "{tool} 参数 {key} 值 {v} 不在允许集合 {{ {allowed} }} 内"
            )));
        }
    }

    if let (Some(items), Some(arr)) = (
        prop.get("items")
            .and_then(|i| i.get("type"))
            .and_then(Value::as_str),
        v.as_array(),
    ) {
        for (i, item) in arr.iter().enumerate() {
            let ok = match items {
                "string" => item.is_string(),
                "integer" => item.is_i64() || item.is_u64(),
                "number" => item.is_number(),
                "boolean" => item.is_boolean(),
                _ => true,
            };
            if !ok {
                return Err(CoreError::Tool(format!(
                    "{tool} 参数 {key}[{i}] 类型应为 {items}，实际: {}",
                    arg_type(item)
                )));
            }
        }
    }
    Ok(())
}

/// JSON 值的人类可读类型名（错误信息用）。
fn arg_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_call_passes() {
        // 全部 11 个工具各来一发合法调用：schema 注册表自洽，合法参数必须通过
        let cases: &[(&str, Value)] = &[
            ("get_timestamp", json!({ "format": "unix" })),
            ("read_file", json!({ "path": "a.txt", "max_bytes": 1024 })),
            ("write_file", json!({ "path": "a.txt", "content": "hi" })),
            ("list_dir", json!({ "path": "." })),
            ("make_dir", json!({ "path": "sub" })),
            ("delete_file", json!({ "path": "a.txt" })),
            (
                "exec_command",
                json!({ "command": "echo hi", "timeout_ms": 5000, "cwd": "." }),
            ),
            ("search_memory", json!({ "keyword": "rust", "limit": 5 })),
            (
                "send_message",
                json!({ "target_id": "ID:000001", "content": "hello" }),
            ),
            (
                "collect_agents",
                json!({ "include_unavailable": true, "limit": 5 }),
            ),
            ("remind", json!({ "action": "list", "limit": 3 })),
        ];
        for (name, args) in cases {
            validate_args(name, args).unwrap_or_else(|e| panic!("合法调用被拒 {name}: {e}"));
        }
    }

    #[test]
    fn missing_required_rejected() {
        let e = validate_args("read_file", &json!({})).unwrap_err();
        assert!(e.to_string().contains("缺少必填参数: path"), "{e}");
    }

    #[test]
    fn null_required_rejected() {
        let e = validate_args("write_file", &json!({ "path": null, "content": "x" })).unwrap_err();
        assert!(e.to_string().contains("不能为 null"), "{e}");
    }

    #[test]
    fn type_mismatch_rejected() {
        let e = validate_args("exec_command", &json!({ "command": 42 })).unwrap_err();
        assert!(e.to_string().contains("类型应为 string"), "{e}");
        // integer 参数传浮点
        let e = validate_args("read_file", &json!({ "path": "a", "max_bytes": 1.5 })).unwrap_err();
        assert!(e.to_string().contains("类型应为 integer"), "{e}");
        // boolean 参数传字符串
        let e =
            validate_args("collect_agents", &json!({ "include_unavailable": "yes" })).unwrap_err();
        assert!(e.to_string().contains("类型应为 boolean"), "{e}");
    }

    #[test]
    fn enum_out_of_range_rejected() {
        // 旧行为：format="weird" 静默走 iso 分支；新行为：拒绝
        let e = validate_args("get_timestamp", &json!({ "format": "weird" })).unwrap_err();
        assert!(e.to_string().contains("不在允许集合"), "{e}");
        let e = validate_args("remind", &json!({ "action": "delete_all" })).unwrap_err();
        assert!(e.to_string().contains("不在允许集合"), "{e}");
    }

    #[test]
    fn unknown_param_rejected() {
        let e =
            validate_args("read_file", &json!({ "path": "a.txt", "pathx": "b.txt" })).unwrap_err();
        assert!(e.to_string().contains("未知参数: pathx"), "{e}");
        let e = validate_args(
            "send_message",
            &json!({ "target_id": "ID:1", "content": "hi", "channel": "TUI" }),
        )
        .unwrap_err();
        assert!(e.to_string().contains("未知参数: channel"), "{e}");
    }

    #[test]
    fn unknown_tool_rejected() {
        let e = validate_args("format_c:", &json!({})).unwrap_err();
        assert!(e.to_string().contains("未知工具"), "{e}");
    }

    #[test]
    fn non_object_args_rejected() {
        let e = validate_args("get_timestamp", &json!(["iso"])).unwrap_err();
        assert!(e.to_string().contains("必须是 JSON 对象"), "{e}");
    }

    #[test]
    fn array_items_type_checked() {
        // string_array_param 的 items.type=string：数字元素被拒
        let schema = ToolSchema::new("t", "demo")
            .param("tags", crate::llm::tools::string_array_param("标签"));
        let ok = json!({ "tags": ["a", "b"] });
        assert!(validate_against(&schema, &ok).is_ok());
        let bad = json!({ "tags": ["a", 3] });
        let e = validate_against(&schema, &bad).unwrap_err();
        assert!(e.to_string().contains("类型应为 string"), "{e}");
    }
}
