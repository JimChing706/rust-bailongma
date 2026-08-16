//! M3 里程碑验证：启动 API 服务器（HTTP / SSE / WS）。
//!
//! 用法：
//! ```text
//! cargo run -p bailongma-app --bin serve
//! BAILONGMA_API_TOKEN=secret cargo run -p bailongma-app --bin serve   # 启用 token 校验
//! cargo run -p bailongma-app --bin serve -- --port 3801               # 自定义端口（侧跑验证）
//! ```
//!
//! 验证（另开终端）：
//! ```text
//! curl http://127.0.0.1:3721/status
//! curl -N http://127.0.0.1:3721/events
//! curl -X POST http://127.0.0.1:3721/message -H "Content-Type: application/json" -d '{"content":"你好"}'
//! ```
//!
//! 第 2 轮审计验证：`--port` 支持独立端口侧跑，避免与运行中的桌面实例（3721）
//! 冲突，用于 API 层实测（token 强制校验 / 限流 429）。

use bailongma_app::api_host;
use bailongma_core::error::{CoreError, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let mut port: Option<u16> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--port" {
            let value = args
                .next()
                .ok_or_else(|| CoreError::Api("--port 需要端口号参数".into()))?;
            port = Some(
                value
                    .trim()
                    .parse::<u16>()
                    .map_err(|e| CoreError::Api(format!("--port 参数非法: {value} ({e})")))?,
            );
        }
    }
    match port {
        Some(p) => api_host::run_api_server_on(p).await,
        None => api_host::run_api_server().await,
    }
}
