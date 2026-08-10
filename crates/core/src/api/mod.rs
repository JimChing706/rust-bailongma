//! API 层：HTTP / SSE / WebSocket 服务（对齐 Node 版 `src/api*.js`）。
//!
//! - [`events`]   事件总线：SSE 广播 + 粘性事件 + brain-ui 观测历史状态机
//! - [`routes`]   路由处理器：/message、/events/history、/status
//! - [`security`] WebSocket 授权与地址/来源校验
//!
//! HTTP 服务装配（axum Router / SSE 流 / WS upgrade）在 `bailongma-server`
//! crate 的 `server.rs`。

pub mod events;
pub mod routes;
pub mod scene;
pub mod security;
pub mod server;
pub mod static_assets;
