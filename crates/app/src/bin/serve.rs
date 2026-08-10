//! M3 里程碑验证：启动 API 服务器（HTTP / SSE / WS）。
//!
//! 用法：
//! ```text
//! cargo run -p bailongma-app --bin serve
//! BAILONGMA_API_TOKEN=secret cargo run -p bailongma-app --bin serve   # 启用 token 校验
//! ```
//!
//! 验证（另开终端）：
//! ```text
//! curl http://127.0.0.1:3721/status
//! curl -N http://127.0.0.1:3721/events
//! curl -X POST http://127.0.0.1:3721/message -H "Content-Type: application/json" -d '{"content":"你好"}'
//! ```
//!
//! 意识循环（pushMessage）尚未迁移：入站消息当前仅打日志并返回占位
//! conversation_id=0，SSE 事件照常广播 —— 供前端联调与协议验证。

use bailongma_app::api_host;
use bailongma_core::error::Result;

#[tokio::main]
async fn main() -> Result<()> {
    api_host::run_api_server().await
}
