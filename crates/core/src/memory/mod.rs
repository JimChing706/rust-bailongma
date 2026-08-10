//! 记忆系统（M4，对齐 Node 版 `src/memory/`）。
//!
//! 当前已就绪（纯函数优先，DB 访问集中在 `db::repositories`）：
//! - [`keywords`]   关键词抽取（`keywords.js`）：中文 n-gram + 英文词、停用词/边界过滤
//! - [`temporal`]   相对时间词解析（`temporal-parser.js`）：今天/昨天/前天/大前天 → 本地时区日期窗口
//! - [`retrieval`]  召回管线（`injector-retrieval.js`）：消息解析、FTS5+向量召回、重要性重排、
//!   「少即是强」选择器、时间词轮廓召回、概念追加召回
//! - [`threads`]    线程/焦点（`threads.js` + `focus.js`）：承诺跟踪、温度分级、消息归属判定、
//!   线程合并/冷驱逐、焦点栈迁移
//! - [`injector`]   注入编排（`injector.js` + `injector-format.js` 摘要部分）：上下文限制计算、
//!   RECALL/方向启发、UI 信号摘要、工具去重保序
//! - [`injector_format`] 注入渲染层（`injector-format.js`）：本地时钟、temporal-recall 块、
//!   记忆/预热/场景/面板/任务知识渲染，纯函数无 DB 依赖
//! - [`messages`]  LLM 消息组装（`runtime/messages.js`）：conversation metadata、系统信号、
//!   过期悬念、TICK 连续性检查、buildLLMMessages 全链路，纯函数
//! - [`channel`]   渠道规范化（`runtime/channel.js`）：normalizeChannel / isSystemSignalRow
//! - [`coding_discipline`] 编程纪律块（`prompt-blocks/coding-discipline.js`）：CODING_BLOCK /
//!   DIAGNOSE_BLOCK + 三信号源注入判定（消息正文 / task 文本 / 最近动作模式）
//! - [`capability_demo_intent`] 能力展示意图（`capability-demo-intent.js`）：CAPABILITY_DEMO 块 +
//!   本地渠道候选注入判定
//! - [`system_prompt`] 系统提示词构建（`prompt.js` buildSystemPrompt）：STABLE 核心 +
//!   Wave 2 按需场景段（音乐/视频/微信/飞书/焦点/沙箱/复杂任务/平台路由等）
//! - [`software_install_intent`] 安装软件意图（`software-install-intent.js`）：触发词 +
//!   动词/名词/winget 包 ID 三级判定，供能力注册表与 tool-router 共用
//! - [`capability_registry`] 能力注册表（`capabilities/capability-registry.js` v1）：
//!   weather / hotspot / worldcup / software-install 四能力的工作流块 + 触发判定 +
//!   消费端（selectActive / contextBlocks / toolsFor）
//! - [`agent_registry`] Agent 委托注册表（`agents/registry.js` prompt 块半）：
//!   AI Collaborators 块 + 一次性发现文本（纯函数；DB 半在
//!   [`crate::db::repositories::agents`]）
//!
//! 规划中（后续里程碑）：`concepts`（涌现概念，对齐 `concept-extractor.js`）、
//! `capability_registry` 的 browser/web/typhoon 能力与运行时数据预喂、
//! `agent_detector`（本地 Agent 扫描，对齐 `agents/detector.js`）。

pub mod agent_registry;
pub mod capability_demo_intent;
pub mod capability_registry;
pub mod channel;
pub mod coding_discipline;
pub mod injector;
pub mod injector_format;
pub mod keywords;
pub mod messages;
pub mod retrieval;
pub mod self_evolution;
pub mod software_install_intent;
pub mod system_prompt;
pub mod weather;
pub mod temporal;
pub mod threads;
