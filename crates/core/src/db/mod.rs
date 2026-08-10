//! 数据层：SQLite 连接管理 / schema 迁移 / 仓库层（M1）。
//!
//! 与 Node 版 `src/db/*` 一一对应：
//! - `connection` → `src/db/connection.js`（WAL + 打开时迁移）
//! - `schema`     → `src/db/schema.js`（幂等迁移）
//! - `models`     → 行 ↔ serde 结构映射（JS 版是裸 row 对象）
//! - `repositories` → `src/db/repositories/*` 与 `src/db.js` 中的仓库函数

pub mod connection;
pub mod models;
pub mod repositories;
pub mod schema;

pub use connection::{open_database, Db};
pub use models::{Conversation, Memory, Thread};
pub use repositories::brain_ui_events;
pub use repositories::conversations;
pub use repositories::memories;
pub use repositories::threads;
pub use repositories::ui_signals;
