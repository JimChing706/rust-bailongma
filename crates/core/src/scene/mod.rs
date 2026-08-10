//! Scene 场景系统：Agent 驱动 UI 的唯一真相源（传输无关）。
//!
//! 对齐 Node 版 `src/scene/*`：
//! - `store`：SceneStore 状态机（surfaces + rev + 幂等 set / snapshot / manifest）
//! - 传输层在 `api::scene`（/scene WebSocket 协议），变更经 broadcast 转成
//!   `scene.patch` / 全量 `scene` 消息下发。

pub mod store;

pub use store::{SceneOp, SceneStore, Surface};
