//! 工具 schema 定义 —— serde 结构 ↔ OpenAI tools 数组元素。
//!
//! 对齐 Node 版 `src/capabilities/schemas.js` 的输出格式：
//! ```json
//! { "type": "function", "function": { "name", "description", "parameters": <JSON Schema> } }
//! ```
//! 具体工具的完整 schema 在 M5（工具能力层）补齐；本模块提供构造器与常用
//! JSON Schema 片段 helper。

use serde_json::{json, Value};

/// 一个工具的 OpenAI schema 描述
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema 对象（type/properties/required…）
    pub parameters: Value,
}

impl ToolSchema {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
        }
    }

    /// 追加一个参数（JSON Schema 片段）
    pub fn param(mut self, name: &str, schema: Value) -> Self {
        let props = self
            .parameters
            .get_mut("properties")
            .and_then(|p| p.as_object_mut())
            .expect("parameters.properties 必须是对象");
        props.insert(name.to_string(), schema);
        self
    }

    /// 追加一个必填参数
    pub fn required(mut self, name: &str, schema: Value) -> Self {
        self = self.param(name, schema);
        let reqs = self
            .parameters
            .get_mut("required")
            .and_then(|r| r.as_array_mut())
            .expect("parameters.required 必须是数组");
        reqs.push(Value::String(name.to_string()));
        self
    }

    /// 转 OpenAI tools 数组元素
    pub fn to_openai_value(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }

    pub fn to_value(&self) -> Value {
        self.to_openai_value()
    }
}

/// 字符串参数 schema
pub fn string_param(description: impl Into<String>) -> Value {
    json!({ "type": "string", "description": description.into() })
}

/// 带枚举的字符串参数 schema
pub fn enum_param(description: impl Into<String>, options: &[&str]) -> Value {
    json!({
        "type": "string",
        "description": description.into(),
        "enum": options,
    })
}

/// 数值参数 schema
pub fn number_param(description: impl Into<String>) -> Value {
    json!({ "type": "number", "description": description.into() })
}

/// 整数参数 schema
pub fn integer_param(description: impl Into<String>) -> Value {
    json!({ "type": "integer", "description": description.into() })
}

/// 布尔参数 schema
pub fn boolean_param(description: impl Into<String>) -> Value {
    json!({ "type": "boolean", "description": description.into() })
}

/// 字符串数组参数 schema
pub fn string_array_param(description: impl Into<String>) -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": description.into(),
    })
}

/// 把 ToolSchema 列表转成 OpenAI tools 数组
pub fn to_openai_tools(tools: &[ToolSchema]) -> Vec<Value> {
    tools.iter().map(ToolSchema::to_openai_value).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_tool() -> ToolSchema {
        ToolSchema::new("get_time", "获取当前时间")
            .required("format", enum_param("格式", &["iso", "unix", "human"]))
            .param("timezone", string_param("时区，默认本地"))
    }

    #[test]
    fn builds_openai_shape() {
        let t = demo_tool();
        let v = t.to_openai_value();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "get_time");
        assert_eq!(v["function"]["parameters"]["type"], "object");
        assert_eq!(v["function"]["parameters"]["required"], json!(["format"]));
        assert_eq!(
            v["function"]["parameters"]["properties"]["format"]["enum"],
            json!(["iso", "unix", "human"])
        );
    }

    #[test]
    fn optional_param_not_required() {
        let t = demo_tool();
        let v = t.to_openai_value();
        let reqs = v["function"]["parameters"]["required"].as_array().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0], "format");
        assert!(v["function"]["parameters"]["properties"]
            .get("timezone")
            .is_some());
    }

    #[test]
    fn array_conversion() {
        let tools = vec![demo_tool()];
        let arr = to_openai_tools(&tools);
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["function"]["name"], "get_time");
    }
}
