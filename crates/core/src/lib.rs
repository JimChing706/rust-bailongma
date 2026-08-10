//! Bailongma (白龙马) 核心运行时库。
//!
//! 从 Node.js 版 Bailongma v2.1.549 用 Rust 全量重写的持续运行数字意识框架。
//! 本 crate 承载全部业务逻辑（与 UI 解耦），Tauri 壳与应用层在 `bailongma-app`。
//!
//! # 模块规划（对应 RUST-ROADMAP.md）
//!
//! - [`error`]   统一错误类型
//! - [`logging`] tracing 日志基础设施
//! - [`config`]  配置加载/校验/迁移（兼容现有 config.json）
//! - [`runtime`] 意识主循环 / 消息队列 / 调度器（M2+）
//! - [`llm`]     LLM 调用器与工具循环（M2）
//! - [`memory`]  记忆系统：线程/焦点/召回/embedding（M4）
//! - [`db`]      数据层：schema/迁移/仓库（M1）
//! - [`tools`]   工具能力层（M5）
//! - [`api`]     HTTP/SSE/WS 服务（M3）
//! - [`voice`]   语音 TTS/ASR/唤醒（M6）
//! - [`social`]  社交连接器（M6）
//! - [`panels`]  数据面板（热点/台风/天气等）
//! - [`envscan`] 环境感知（系统信息/软件/Agent）
//!
//! 当前为 M0 骨架：`error` / `logging` / `config` 已就绪，其余模块占位。

//! 当前进度：M0（骨架）+ M1（数据层）+ M2（LLM 调用器与工具循环）+ M3（API 层）已就绪；
//! M4 记忆系统进行中：`memory::keywords` / `memory::temporal` / `memory::retrieval` 可用，
//! `db::repositories::memories` 含 FTS5 关键词搜索 / 日期窗口 / 向量召回；
//! `embedding` 提供 `Embedder` 抽象（当前 NoopEmbedder → FTS5-only 完整路径）。
//!
//! R2 落地：`tools::NativeToolExecutor` 真实工具层（9 个工具）已接入工具循环。

pub mod agents;
pub mod api;
pub mod config;
pub mod db;
pub mod embedding;
pub mod error;
pub mod llm;
pub mod logging;
pub mod memory;
pub mod runtime;
pub mod scene;
pub mod tools;
pub mod wakeup;

// ── M2+ 模块占位（规划中，避免编译期空目录问题） ──
pub mod prelude {
    pub use crate::error::{CoreError, Result};
    pub use crate::logging::LogLevel;
}

/// crate 版本号（与 Cargo.toml 一致）
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 协议/数据兼容常量：与 Node 版对齐
pub mod compat {
    /// 本地 API 默认端口
    pub const DEFAULT_API_PORT: u16 = 3721;
    /// canonical 用户 ID（与 db/utils.js 一致）
    pub const CANONICAL_USER_ID: &str = "ID:000001";
    /// 默认 Agent 名
    pub const DEFAULT_AGENT_NAME: &str = "小白龙";
}
