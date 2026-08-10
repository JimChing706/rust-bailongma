//! 仓库层：数据访问函数（对齐 Node 版 `src/db.js` 与 `src/db/repositories/*`）。
//!
//! 所有函数第一个参数都是 `&Db`（连接包装），便于上层在任意线程调用。

pub mod agents;
pub mod brain_ui_events;
pub mod config;
pub mod conversations;
pub mod llm_metrics;
pub mod memories;
pub mod reminders;
pub mod threads;
pub mod turn_state;
pub mod ui_signals;
