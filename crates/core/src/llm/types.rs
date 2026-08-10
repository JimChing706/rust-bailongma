//! LLM 数据模型 —— 消息 / 工具调用 / usage / 流式事件 / 请求参数。
//!
//! 对齐 Node 版 `src/llm.js` / openai SDK 的对象形状，serde 序列化为
//! OpenAI 兼容 `/chat/completions` 请求体。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 消息角色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

impl std::fmt::Display for ChatRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatRole::System => write!(f, "system"),
            ChatRole::User => write!(f, "user"),
            ChatRole::Assistant => write!(f, "assistant"),
            ChatRole::Tool => write!(f, "tool"),
        }
    }
}

/// 对话消息（serde 展平，仅序列化存在的字段 —— 对齐 OpenAI SDK 行为）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    #[serde(rename = "role")]
    pub role: ChatRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// assistant 消息的工具调用（OpenAI 格式）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallPayload>>,
    /// tool 消息对应的工具调用 id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// DeepSeek 思考内容（回放时保留，Node 版会原样写回）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl ChatMessage {
    pub fn new(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(ChatRole::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(ChatRole::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(ChatRole::Assistant, content)
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
        }
    }
}

/// assistant 消息内的工具调用（OpenAI 协议格式）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallPayload {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub function: ToolFunctionPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunctionPayload {
    pub name: String,
    pub arguments: String,
}

/// 流式解析出的工具调用（增量拼装后的完整形态）
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    /// 解析参数 JSON；非法时回退空对象（对齐 Node `try { JSON.parse } catch { {} }`）
    pub fn parse_args(&self) -> Value {
        serde_json::from_str(&self.arguments).unwrap_or_else(|_| Value::Object(Default::default()))
    }
}

/// 用量统计（stream_options.include_usage 开启时末帧携带）
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub total_tokens: u32,
    pub prompt_cache_hit_tokens: u32,
    pub prompt_cache_miss_tokens: u32,
}

/// 流式事件（对齐 Node onStream 回调的 {event, mode, text, name} 形状）
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// 思考流 / 文本流开始
    Start { mode: StreamMode },
    /// 文本增量
    Chunk { text: String },
    /// 流结束（思考流或文本流）
    End,
    /// 工具调用名第一次完整出现（UI 停止思考动画的信号）
    ToolPreparing { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    Think,
    Text,
}

/// 单次流式调用的结果（对齐 Node streamOnce 的返回值）
#[derive(Debug, Clone, Default)]
pub struct StreamOnceResult {
    pub content: String,
    pub reasoning_content: String,
    pub tool_calls: Vec<ToolCall>,
    pub aborted: bool,
    pub usage: Usage,
}

/// OpenAI 兼容 chat completions 请求体
#[derive(Debug, Clone, Serialize, Default)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Value>,
}

// ─────────────────────────────────────────────────────────────
// <think> 标签流式解析（对齐 Node emitTextChunk 前的 think 状态机）
// ─────────────────────────────────────────────────────────────

/// `<think>` 标签流式状态机：
/// 输入逐段文本，输出"该段应推送为思考流 / 文本流 / 两者边界"。
/// 注意 Node 实现只在 `!thinkDone` 时处理标签；`reasoning_content` 字段
/// （DeepSeek reasoner）不经过这里，由 caller 单独分流。
#[derive(Debug, Default)]
pub struct ThinkStreamState {
    pub in_think: bool,
    pub think_done: bool,
    /// 已缓存但尚未标记为 think 的文本（跨 chunk 拼 <think> 时用）
    pending: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ThinkEvent {
    /// 输出为思考流文本
    Think(String),
    /// 输出为正文文本流
    Text(String),
    /// 思考流结束（</think> 出现），该事件本身不含文本
    EndThink,
}

impl ThinkStreamState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 推送一段原始文本，产出分段事件（对齐 Node 264-291 行逻辑）
    pub fn push(&mut self, raw: &str) -> Vec<ThinkEvent> {
        let mut events = Vec::new();
        self.pending.push_str(raw);
        loop {
            if self.think_done {
                if !self.pending.is_empty() {
                    events.push(ThinkEvent::Text(std::mem::take(&mut self.pending)));
                }
                return events;
            }
            if !self.in_think {
                if let Some(pos) = self.pending.find("<think>") {
                    let before = self.pending[..pos].to_string();
                    self.pending = self.pending[pos + "<think>".len()..].to_string();
                    self.in_think = true;
                    if !before.is_empty() {
                        events.push(ThinkEvent::Text(before));
                    }
                    continue;
                }
                // 还没出现完整 <think>：仅保留"恰好是其前缀"的尾部（跨 chunk 拼标签），
                // 其余直接作为正文输出
                let keep = longest_think_prefix_len(&self.pending);
                let emit_len = self.pending.len() - keep;
                let emit = self.pending[..emit_len].to_string();
                self.pending = self.pending[emit_len..].to_string();
                if !emit.is_empty() {
                    events.push(ThinkEvent::Text(emit));
                }
                return events;
            }
            // in_think：查找 </think>
            if let Some(pos) = self.pending.find("</think>") {
                let before = self.pending[..pos].to_string();
                self.pending = self.pending[pos + "</think>".len()..]
                    .trim_start()
                    .to_string();
                self.in_think = false;
                self.think_done = true;
                if !before.is_empty() {
                    events.push(ThinkEvent::Think(before));
                }
                events.push(ThinkEvent::EndThink);
                continue;
            }
            if !self.pending.is_empty() {
                events.push(ThinkEvent::Think(std::mem::take(&mut self.pending)));
            }
            return events;
        }
    }
}

/// 返回 `pending` 末尾"恰好是 `<think>` 前缀"的最大字节数（0..7）。
/// 只检查落在字符边界上的切分点，避免切坏多字节字符。
fn longest_think_prefix_len(pending: &str) -> usize {
    const TAG: &str = "<think>";
    let n = pending.len();
    for l in (1..TAG.len()).rev() {
        if n < l {
            continue;
        }
        let start = n - l;
        if !pending.is_char_boundary(start) {
            continue;
        }
        if pending[start..] == TAG[..l] {
            return l;
        }
    }
    0
}

// ─────────────────────────────────────────────────────────────
// XML 格式工具调用解析（MiniMax 备用格式，对齐 parseXmlToolCalls）
// ─────────────────────────────────────────────────────────────

/// 从文本内容解析 `<invoke name="..."><parameter name="...">...</parameter></invoke>` 工具调用
pub fn parse_xml_tool_calls(content: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<invoke") {
        let after_open = &rest[start..];
        // 提取 name 属性
        let name_attr = after_open.find("name=").map(|p| &after_open[p + 5..]);
        let Some(name_attr) = name_attr else { break };
        let name_end = name_attr
            .find('>')
            .or_else(|| name_attr.find('/'))
            .unwrap_or(0);
        if name_end == 0 {
            break;
        }
        let name = name_attr[..name_end]
            .trim_matches(|c| c == '"' || c == '\'' || c == ' ')
            .to_string();
        // 找闭合 </invoke>
        let Some(close) = after_open.find("</invoke>") else {
            break;
        };
        let body = &after_open[..close];
        let mut xml_args = serde_json::Map::new();
        let mut body_rest = body;
        while let Some(ps) = body_rest.find("<parameter") {
            let after_p = &body_rest[ps..];
            let Some(pi) = after_p.find("name=") else {
                break;
            };
            let pname = &after_p[pi + 5..];
            let pname_end = pname.find('>').unwrap_or(0);
            if pname_end == 0 {
                break;
            }
            let pn = pname[..pname_end]
                .trim_matches(|c| c == '"' || c == '\'' || c == ' ')
                .to_string();
            let Some(pclose) = after_p.find("</parameter>") else {
                break;
            };
            // 值起始位置统一按相对 after_p 的偏移计算（pname_end 相对 pname）
            let value_start = pi + 5 + pname_end + 1;
            let pvalue = &after_p[value_start..pclose];
            xml_args.insert(pn, Value::String(pvalue.trim().to_string()));
            body_rest = &after_p[pclose + "</parameter>".len()..];
        }
        calls.push(ToolCall {
            id: format!("xml_{}", calls.len()),
            name,
            arguments: Value::Object(xml_args).to_string(),
        });
        rest = &after_open[close + "</invoke>".len()..];
    }
    calls
}

// ─────────────────────────────────────────────────────────────
// 工具指纹（对齐 buildToolFingerprint 的 stableStringify）
// ─────────────────────────────────────────────────────────────

/// 稳定序列化：对象键排序，数组递归（对齐 Node stableStringify）
pub fn stable_stringify(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(stable_stringify).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        stable_stringify(&map[*k])
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

/// 工具调用指纹：`name:stableArgs`（对齐 buildToolFingerprint）
pub fn build_tool_fingerprint(name: &str, args: &Value) -> String {
    format!("{}:{}", name, stable_stringify(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serializes_openai_shape() {
        let m = ChatMessage::system("你是一个助手");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "system");
        assert_eq!(v["content"], "你是一个助手");
        assert!(v.get("tool_calls").is_none());
    }

    #[test]
    fn tool_message_has_call_id() {
        let m = ChatMessage::tool("call_1", "ok");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["tool_call_id"], "call_1");
    }

    #[test]
    fn parse_args_fallback_to_empty_object() {
        let tc = ToolCall {
            id: "1".into(),
            name: "x".into(),
            arguments: "{bad json".into(),
        };
        assert_eq!(tc.parse_args(), Value::Object(Default::default()));
        let ok = ToolCall {
            id: "1".into(),
            name: "x".into(),
            arguments: r#"{"a":1}"#.into(),
        };
        assert_eq!(ok.parse_args()["a"], 1);
    }

    #[test]
    fn think_state_single_chunk() {
        let mut s = ThinkStreamState::new();
        let evts = s.push("答案是<think>让我想想</think>42");
        assert_eq!(evts[0], ThinkEvent::Text("答案是".into()));
        assert_eq!(evts[1], ThinkEvent::Think("让我想想".into()));
        assert_eq!(evts[2], ThinkEvent::EndThink);
        assert_eq!(evts[3], ThinkEvent::Text("42".into()));
        assert!(s.think_done);
    }

    #[test]
    fn think_state_split_across_chunks() {
        let mut s = ThinkStreamState::new();
        // 第一段："答案是<thi" —— <think> 只来了一半，应输出正文"答案是"
        let evts = s.push("答案是<thi");
        assert_eq!(evts, vec![ThinkEvent::Text("答案是".into())]);
        assert!(!s.in_think);
        // 第二段："nk>让我想" —— 补全 <think>，进入思考流
        let evts = s.push("nk>让我想");
        assert_eq!(evts, vec![ThinkEvent::Think("让我想".into())]);
        assert!(s.in_think);
        // 第三段："想</think>结束"
        let evts = s.push("想</think>结束");
        assert_eq!(evts[0], ThinkEvent::Think("想".into()));
        assert_eq!(evts[1], ThinkEvent::EndThink);
        assert_eq!(evts[2], ThinkEvent::Text("结束".into()));
    }

    #[test]
    fn think_state_no_tag_passthrough() {
        let mut s = ThinkStreamState::new();
        let evts = s.push("你好世界");
        assert_eq!(evts, vec![ThinkEvent::Text("你好世界".into())]);
        assert!(!s.think_done);
        // 无标签纯文本走完
        let evts2 = s.push("继续");
        assert_eq!(evts2, vec![ThinkEvent::Text("继续".into())]);
    }

    #[test]
    fn xml_tool_calls_parsed() {
        let content = r#"思考了一下<invoke name="get_time"><parameter name="format">iso</parameter></invoke>现在时间<invoke name="echo"><parameter name="text">hi</parameter></invoke>"#;
        let calls = parse_xml_tool_calls(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "get_time");
        assert_eq!(calls[0].parse_args()["format"], "iso");
        assert_eq!(calls[1].name, "echo");
        assert_eq!(calls[1].parse_args()["text"], "hi");
        assert!(calls[0].id.starts_with("xml_"));
    }

    #[test]
    fn xml_tool_calls_skip_when_no_invoke() {
        assert!(parse_xml_tool_calls("没有工具调用").is_empty());
    }

    #[test]
    fn stable_stringify_sorts_keys() {
        let a = serde_json::json!({"b": 1, "a": [2, 1]});
        let b = serde_json::json!({"a": [2, 1], "b": 1});
        assert_eq!(stable_stringify(&a), stable_stringify(&b));
        assert_eq!(stable_stringify(&a), r#"{"a":[2,1],"b":1}"#);
    }

    #[test]
    fn fingerprint_includes_name() {
        let args = serde_json::json!({"q": "hi"});
        assert_eq!(
            build_tool_fingerprint("web_search", &args),
            "web_search:{\"q\":\"hi\"}"
        );
        // 键顺序不影响指纹
        let args2 = serde_json::json!({"q": "hi", "n": 1});
        let args3 = serde_json::json!({"n": 1, "q": "hi"});
        assert_eq!(
            build_tool_fingerprint("web_search", &args2),
            build_tool_fingerprint("web_search", &args3)
        );
    }
}
