//! 手动 SSE（Server-Sent Events）流解析器。
//!
//! Node 版通过 openai SDK 消费流；Rust 侧直接用 reqwest 的字节流 + 本解析器，
//! 零额外依赖。处理 chunk 边界任意切分、`\r\n`/`\n` 混合、多行 `data:` 拼接、
//! 注释行、`[DONE]` 终止标记。

use serde::Deserialize;

/// 解析出的一个 SSE 事件
#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    /// `data:` 字段内容（多行已拼接）
    Data(String),
    /// `data: [DONE]`
    Done,
}

/// 增量 SSE 解析器：`push` 网络字节流，产出完整事件；跨 chunk 的残片保留在内部缓冲。
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节流，返回解析出的完整事件
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        self.drain_events()
    }

    /// 流结束时调用，把残留缓冲解析掉（正常情况下应无残留）
    pub fn finish(mut self) -> Vec<SseEvent> {
        let events = self.drain_events();
        // 尾部若还有残片（无 \n\n 结尾），按一个事件处理
        if !self.buffer.is_empty() {
            let mut events = events;
            if let Some(ev) = parse_block(&self.buffer) {
                events.push(ev);
            }
            events
        } else {
            events
        }
    }

    /// 从缓冲中切出所有完整事件块（以空行分隔）
    fn drain_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        loop {
            let block_end = find_block_end(&self.buffer);
            match block_end {
                Some(end) => {
                    let block: Vec<u8> = self.buffer.drain(..end).collect();
                    if let Some(ev) = parse_block(&block) {
                        events.push(ev);
                    }
                }
                None => break,
            }
        }
        events
    }
}

/// 找到第一个完整事件块的结束位置（含末尾空行），找不到返回 None
fn find_block_end(buf: &[u8]) -> Option<usize> {
    // 事件块以空行（\n\n 或 \r\n\r\n）分隔
    if let Some(pos) = find_subslice(buf, b"\n\n") {
        return Some(pos + 2);
    }
    if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
        return Some(pos + 4);
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 解析一个事件块（可能是空块 → None）
fn parse_block(block: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(block);
    let mut data_lines: Vec<String> = Vec::new();
    let mut is_event = false;
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        } else if line.starts_with("event:") {
            is_event = true;
        }
        // `:` 开头的注释行、`id:`/`retry:` 等字段忽略
    }
    if data_lines.is_empty() {
        return None;
    }
    let payload = data_lines.join("\n");
    if payload.trim() == "[DONE]" {
        Some(SseEvent::Done)
    } else if is_event {
        // 自定义 event 类型（如 zhipu 的 thinking 事件）——按 data 处理
        Some(SseEvent::Data(payload))
    } else {
        Some(SseEvent::Data(payload))
    }
}

// ─────────────────────────────────────────────────────────────
// OpenAI 兼容流 chunk JSON 结构（对齐 openai SDK 的 ChatCompletionChunk）
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<ChunkUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Option<ChunkDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    /// DeepSeek reasoner 思考字段（同时兼容几种命名）
    #[serde(default, alias = "reasoningContent", alias = "reasoning")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChunkToolCallDelta>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkToolCallDelta {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChunkToolFunction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkToolFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct ChunkUsage {
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u32,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_event() {
        let mut p = SseParser::new();
        let evts = p.push(b"data: {\"hello\":1}\n\n");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0], SseEvent::Data("{\"hello\":1}".into()));
    }

    #[test]
    fn parses_multiple_events_in_one_chunk() {
        let mut p = SseParser::new();
        let evts = p.push(b"data: a\n\ndata: b\n\n");
        assert_eq!(
            evts,
            vec![SseEvent::Data("a".into()), SseEvent::Data("b".into())]
        );
    }

    #[test]
    fn handles_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"a\"").is_empty());
        let evts = p.push(b":1}\n\ndata: [DONE]\n\n");
        assert_eq!(
            evts,
            vec![SseEvent::Data("{\"a\":1}".into()), SseEvent::Done]
        );
    }

    #[test]
    fn done_marker_detected() {
        let mut p = SseParser::new();
        let evts = p.push(b"data: [DONE]\n\n");
        assert_eq!(evts, vec![SseEvent::Done]);
    }

    #[test]
    fn multi_line_data_joined() {
        let mut p = SseParser::new();
        let evts = p.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(evts, vec![SseEvent::Data("line1\nline2".into())]);
    }

    #[test]
    fn crlf_handled() {
        let mut p = SseParser::new();
        let evts = p.push(b"data: x\r\ndata: y\r\n\r\n");
        assert_eq!(evts, vec![SseEvent::Data("x\ny".into())]);
    }

    #[test]
    fn comment_lines_ignored() {
        let mut p = SseParser::new();
        let evts = p.push(b": comment\ndata: real\n\n");
        assert_eq!(evts, vec![SseEvent::Data("real".into())]);
    }

    #[test]
    fn chunk_json_parses_delta_and_tool_calls() {
        let json = r#"{
          "id": "chatcmpl-1",
          "choices": [{
            "index": 0,
            "delta": {
              "tool_calls": [{
                "index": 0,
                "id": "call_1",
                "function": {"name": "get_time", "arguments": "{\"format\":"}
              }]
            }
          }],
          "usage": null
        }"#;
        let chunk: ChatChunk = serde_json::from_str(json).unwrap();
        let delta = chunk.choices[0].delta.as_ref().unwrap();
        let tc = delta.tool_calls.as_ref().unwrap()[0].clone();
        assert_eq!(tc.id.as_deref(), Some("call_1"));
        assert_eq!(
            tc.function.as_ref().unwrap().name.as_deref(),
            Some("get_time")
        );
        assert_eq!(
            tc.function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"format\":")
        );
    }

    #[test]
    fn chunk_json_parses_usage() {
        let json = r#"{"usage":{"total_tokens":123,"prompt_cache_hit_tokens":10,"prompt_cache_miss_tokens":90}}"#;
        let chunk: ChatChunk = serde_json::from_str(json).unwrap();
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.total_tokens, 123);
        assert_eq!(usage.prompt_cache_hit_tokens, 10);
        assert_eq!(usage.prompt_cache_miss_tokens, 90);
    }

    #[test]
    fn finish_flushes_trailing_buffer() {
        let p = SseParser::new();
        // 直接构造：模拟流结束但没等空行
        let mut p2 = SseParser::new();
        p2.push(b"data: tail");
        let evts = p2.finish();
        assert_eq!(evts, vec![SseEvent::Data("tail".into())]);
        let _ = p;
    }
}
