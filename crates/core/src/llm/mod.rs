//! LLM 调用器与工具循环（M2）。
//!
//! 对齐 Node 版 `src/llm.js` + `src/config.js` 的 provider 适配：
//! - [`providers`]：7 个内置 provider 注册表与请求参数适配决策
//! - [`types`]：消息 / 工具调用 / usage / 流式事件 / `<think>` 状态机 / XML 工具调用解析
//! - [`sse`]：手动 SSE 流解析
//! - [`caller`]：请求构造 + 单次流式调用（空闲超时 / 中止 / usage / P1-2 幂等键）
//! - [`retry`]：瞬时错误判定 + 退避重试 + MiMo 模型降级链
//! - [`metrics`]：LLM 调用指标采集（P0 观测层，M1；mpsc 后台 flusher + 幂等落库）
//! - [`replay`]：工具执行防重放（P1-2；台账读侧，响应丢失重试不重复执行）
//! - [`tools`]：工具 schema 构造器（serde ↔ JSON Schema）
//! - [`markers`]：文本协议标记剥离（[RECALL:]/[SET_TASK:]/[CLEAR_TASK]/[UPDATE_PERSONA:] 与 think 块）
//! - [`tool_loop`]：agentic 主循环（熔断 / 指纹 / 参数别名 / nudge）

pub mod caller;
pub mod markers;
pub mod metrics;
pub mod providers;
pub mod replay;
pub mod retry;
pub mod sse;
pub mod tool_loop;
pub mod tools;
pub mod types;
