//! 波2a：显式服务层 `AppRuntime` —— 三入口共用同一装配。
//!
//! 统一承载「入站消息 → 落库 → turn_state 状态机 → 归属/注入（run_user_turn）
//! → LLM 工具循环 → 回复落库 + 广播」的完整意识闭环，chat / serve / desktop
//! 三入口共用 [`AppRuntime`] 装配：
//!
//! - [`AppRuntime::boot`]：一键装配（config + db + event bus + runtime state + 工具沙箱）
//! - [`AppRuntime::spawn_message_turn`]：serve / desktop 用（异步跑，不阻塞 HTTP 响应）
//! - [`AppRuntime::run_message_turn`]：chat 用（同步 await，返回可展示 [`TurnReply`]）
//! - [`AppRuntime::spawn_wakeup_loop`]：后台提醒唤醒循环（TICK 轮）
//!
//! 波2a 验收：三 bin 共用装配函数；chat 不再维护独立 executor / tools / LLM 配置。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bailongma_core::api::events::EventBus;
use bailongma_core::api::routes::InboundMessage;
use bailongma_core::compat;
use bailongma_core::config::{load_config, Config};
use bailongma_core::db::repositories::{brain_ui_events, conversations, turn_state};
use bailongma_core::db::Db;
use bailongma_core::embedding::NoopEmbedder;
use bailongma_core::error::Result;
use bailongma_core::llm::caller::{LlmConfig, StreamContext};
use bailongma_core::llm::replay::DbToolReplayGuard;
use bailongma_core::llm::tool_loop::{
    call_llm, real_stream_fn, CallLlmArgs, OnToolCall, ToolLoopLimits,
};
use bailongma_core::llm::types::{ChatMessage, ChatRole, StreamEvent};
use bailongma_core::memory::injector::{ContextWindowConfig, InjectorContext};
use bailongma_core::memory::messages::LlmRole;
use bailongma_core::runtime::{init as runtime_init, run_user_turn, RuntimeState};
use bailongma_core::tools::{all_tool_schemas, NativeToolExecutor, SendMessageFn};
use bailongma_core::wakeup::{coalesced_wakeup, CoalescedWakeup};
use serde_json::json;
use tokio::sync::Mutex;

/// 流式回调（chat 入口的实时打印用；serve 传 None）。
pub type StreamCallback = Arc<dyn Fn(StreamEvent) + Send + Sync>;

/// 一轮意识闭环的展示结果（chat / 测试用）。
#[derive(Debug, Clone)]
pub struct TurnReply {
    pub conversation_id: i64,
    pub ok: bool,
    pub content: String,
    pub total_calls: usize,
    pub aborted: bool,
    pub tool_name: Option<String>,
}

/// 显式服务层：统一承载 message / turn_state / LLM / 落库 / 广播 的装配。
#[derive(Clone)]
pub struct AppRuntime {
    pub db: Db,
    pub bus: EventBus,
    pub state: Arc<Mutex<RuntimeState>>,
    pub cfg: Config,
    pub tool_root: PathBuf,
    pub sandbox_bin: Option<PathBuf>,
    pub agent_name: String,
    /// chat 入口的 --api-key 临时覆盖（不写回 config.json；serve/desktop 恒 None）。
    pub api_key_override: Option<String>,
}

/// 当前轮时间戳（本地时区 ISO，与 runtime 测试的消息格式一致）。
fn now_input_ts() -> String {
    chrono::Local::now().to_rfc3339()
}

/// 定位 sandbox 子进程可执行文件（与当前可执行文件同目录）。
fn locate_sandbox_bin() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for name in ["bailongma-sandbox.exe", "bailongma-sandbox"] {
        let candidate = exe_dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 工具沙箱根：user_dir 下显式 sandbox 目录（第 1 轮审计修复——
/// 不再用 serve 进程 cwd 兜底，避免「从哪启动根就是哪」的隐式逃逸面）。
fn sandbox_root(user_dir: &Path) -> PathBuf {
    let dir = user_dir.join("sandbox");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

impl AppRuntime {
    /// 显式装配（核心装配函数）：所有入口共用同一份服务层。
    pub fn assemble(
        db: Db,
        bus: EventBus,
        state: Arc<Mutex<RuntimeState>>,
        cfg: Config,
        tool_root: PathBuf,
        sandbox_bin: Option<PathBuf>,
    ) -> Self {
        let agent_name = cfg
            .extra
            .get("agent_name")
            .and_then(|v| v.as_str())
            .unwrap_or(compat::DEFAULT_AGENT_NAME)
            .to_string();
        Self {
            db,
            bus,
            state,
            cfg,
            tool_root,
            sandbox_bin,
            agent_name,
            api_key_override: None,
        }
    }

    /// 从 user_dir 装配（config 已加载，避免二次 load）。
    pub fn from_dir(user_dir: &Path, cfg: Config) -> Result<Self> {
        let db_path = user_dir.join("data").join("jarvis.db");
        let db = Db::open(&db_path)?;
        let persist_db = db.clone();
        let bus = EventBus::new(Arc::new(move |ts, path, ty, payload| {
            brain_ui_events::insert_brain_ui_event(&persist_db, &ts, path, ty, payload);
        }));
        let state = Arc::new(Mutex::new(runtime_init(&db)?));
        let tool_root = sandbox_root(user_dir);
        let sandbox_bin = locate_sandbox_bin();
        Ok(Self::assemble(db, bus, state, cfg, tool_root, sandbox_bin))
    }

    /// 一键装配：resolve_user_dir → load_config → from_dir。
    pub fn boot(user_dir: &Path) -> Result<Self> {
        let cfg = load_config(user_dir)?;
        Self::from_dir(user_dir, cfg)
    }

    /// chat 入口：临时覆盖 API Key（不写回 config.json）。
    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        self.api_key_override = key.filter(|k| !k.trim().is_empty());
        self
    }

    /// 真实工具执行器（记忆检索 + send_message 投递 + sandbox 命令委托）——
    /// 交互轮与唤醒轮共用同一构建。
    fn build_executor(&self) -> NativeToolExecutor {
        let sender = self.bus.clone();
        let send_db = self.db.clone();
        let send_message: SendMessageFn = Arc::new(move |target: &str, content: &str| {
            let _ = conversations::insert(&send_db, "agent", target, content);
            sender.emit(
                "message_out",
                json!({
                    "from_id": target,
                    "content": content,
                    "channel": "API",
                    "timestamp": now_input_ts(),
                }),
            );
            Ok("delivered".into())
        });
        let mut executor = NativeToolExecutor::new(self.tool_root.clone())
            .with_db(self.db.clone())
            .with_send_message(send_message);
        if let Some(bin) = &self.sandbox_bin {
            executor = executor.with_sandbox(bin.clone());
        }
        executor
    }

    /// serve / desktop 入口：落库用户消息 → 异步跑意识闭环（不阻塞 HTTP 响应）。
    /// 返回 conversation_id；落库失败返回 None。
    pub fn spawn_message_turn(&self, msg: InboundMessage) -> Option<i64> {
        let conversation_id =
            conversations::insert(&self.db, "user", &msg.from_id, &msg.content).ok()?;
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime
                .run_message_turn_impl(msg, conversation_id, None, None)
                .await;
        });
        Some(conversation_id)
    }

    /// chat 入口：落库 + 同步 await 意识闭环，返回可展示的 [`TurnReply`]。
    pub async fn run_message_turn(
        &self,
        msg: InboundMessage,
        stream_cb: Option<StreamCallback>,
        tool_cb: Option<OnToolCall>,
    ) -> TurnReply {
        let conversation_id =
            match conversations::insert(&self.db, "user", &msg.from_id, &msg.content) {
                Ok(id) => id,
                Err(e) => {
                    return TurnReply {
                        conversation_id: 0,
                        ok: false,
                        content: format!("（入站消息落库失败：{e}）"),
                        total_calls: 0,
                        aborted: false,
                        tool_name: None,
                    };
                }
            };
        self.run_message_turn_impl(msg, conversation_id, stream_cb, tool_cb)
            .await
    }

    /// R3 意识闭环（交互轮）：入站消息 → 归属/注入/落库 → LLM 工具循环 → 回复落库 + 广播。
    /// Phase 1 状态机：turn_state 表全程落状态 received → running → completed / failed。
    async fn run_message_turn_impl(
        &self,
        msg: InboundMessage,
        conversation_id: i64,
        stream_cb: Option<StreamCallback>,
        tool_cb: Option<OnToolCall>,
    ) -> TurnReply {
        // 0) Phase 1：turn_state 建行（received）；失败仅告警，不阻断主流程
        let turn_id = match turn_state::create_turn(
            &self.db,
            &now_input_ts(),
            &format!("conv-{conversation_id}"),
            &msg.channel,
            &msg.from_id,
            &msg.content,
            Some(conversation_id),
            "retry",
        ) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!("[P1] turn_state 建行失败（降级继续）: {e}");
                None
            }
        };
        let finish_turn = |state: &str, err: String| {
            if let Some(id) = turn_id {
                if !err.is_empty() {
                    if let Err(e) = turn_state::set_error(&self.db, id, &err) {
                        tracing::warn!("[P1] turn_state set_error 失败: {e}");
                    }
                }
                if let Err(e) = turn_state::mark_finished(&self.db, id, state, &now_input_ts()) {
                    tracing::warn!("[P1] turn_state mark_finished({state}) 失败: {e}");
                }
            }
        };
        let reply = |ok: bool, content: String, total_calls: usize, aborted: bool, tool_name: Option<String>| {
            TurnReply {
                conversation_id,
                ok,
                content,
                total_calls,
                aborted,
                tool_name,
            }
        };

        // 1) LLM 激活检查（未激活 → 降级回复，不空转）
        let mut cfg = self.cfg.clone();
        if let Some(k) = &self.api_key_override {
            cfg.api_key = Some(k.clone());
        }
        let llm_cfg = match LlmConfig::from_config(&cfg) {
            Ok(mut c) => {
                // P3-2 模型路由：交互轮走主模型
                c.model = c.route_model("interactive");
                c
            }
            Err(e) => {
                let text = format!("（LLM 未激活，本轮无法生成回复：{e}）");
                let _ = conversations::insert(&self.db, "agent", &msg.from_id, &text);
                self.bus.emit(
                    "message_out",
                    json!({
                        "from_id": msg.from_id,
                        "content": text,
                        "channel": msg.channel,
                        "timestamp": now_input_ts(),
                        "conversation_id": conversation_id,
                    }),
                );
                finish_turn("failed", format!("llm not activated: {e}"));
                return reply(true, text, 0, false, None);
            }
        };

        // 2) 组装入站输入（对齐 parse_message_input 约定）并跑注入闭包
        let input = format!(
            "[{}] {} [{}] {}",
            msg.from_id, now_input_ts(), msg.channel, msg.content
        );
        let embedder = NoopEmbedder;
        let window = ContextWindowConfig::default();
        let ctx = InjectorContext::default();

        if let Some(id) = turn_id {
            if let Err(e) = turn_state::set_state(&self.db, id, "running") {
                tracing::warn!("[P1] turn_state->running 失败: {e}");
            }
        }

        let turn = {
            let mut guard = self.state.lock().await;
            match run_user_turn(
                &self.db,
                &embedder,
                &mut guard,
                &input,
                &msg.channel,
                "",
                &ctx,
                &window,
                &self.agent_name,
                false,
                None,
                "",
                None,
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("[R3] run_user_turn 失败: {e}");
                    let text = format!("（本轮处理失败：{e}）");
                    let _ = conversations::insert(&self.db, "agent", &msg.from_id, &text);
                    self.bus.emit(
                        "message_out",
                        json!({
                            "from_id": msg.from_id,
                            "content": text,
                            "channel": msg.channel,
                            "timestamp": now_input_ts(),
                            "conversation_id": conversation_id,
                        }),
                    );
                    finish_turn("failed", format!("run_user_turn: {e}"));
                    return reply(false, text, 0, false, None);
                }
            }
        };

        // 3) 真实工具执行器
        let executor = self.build_executor();

        // 组装 LLM 消息：LlmMessage（运行期组装）→ ChatMessage（OpenAI 线协议）
        let chat_messages: Vec<ChatMessage> = turn
            .llm_messages
            .iter()
            .map(|m| ChatMessage {
                role: match m.role {
                    LlmRole::System => ChatRole::System,
                    LlmRole::User => ChatRole::User,
                    LlmRole::Assistant => ChatRole::Assistant,
                },
                content: Some(m.content.clone()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            })
            .collect();

        let args = CallLlmArgs {
            messages: Some(chat_messages),
            tools: all_tool_schemas(),
            local_reply: false,
            must_reply: true,
            ..Default::default()
        };
        let client = reqwest::Client::new();
        let stream = real_stream_fn();
        let mut ctx = StreamContext::default();
        ctx.on_stream = stream_cb;

        // 4) LLM 工具循环（真实流式调用 + 全量工具；P1-2 防重放台账统一启用）
        let replay_guard = DbToolReplayGuard::new(self.db.clone());
        let result = call_llm(
            &client,
            &llm_cfg,
            stream.as_ref(),
            &executor,
            &args,
            &ctx,
            tool_cb,
            &ToolLoopLimits::default(),
            Some(&replay_guard),
        )
        .await;

        match result {
            Ok(r) => {
                let tool_name = r.tool_result.as_ref().map(|t| t.name.clone());
                let content = r.content.trim().to_string();
                if content.is_empty() {
                    if r.total_calls == 0 {
                        tracing::warn!("[R3] LLM 空回复且无工具调用（{conversation_id}）");
                        finish_turn("failed", "empty reply, no tool calls".to_string());
                        return reply(false, String::new(), r.total_calls, r.aborted, tool_name);
                    }
                    // 有工具调用但无文本：工具副作用已发生，按完成记账
                    finish_turn("completed", String::new());
                    return reply(true, String::new(), r.total_calls, r.aborted, tool_name);
                }
                let _ = conversations::insert(&self.db, "agent", &msg.from_id, &content);
                self.bus.emit(
                    "message_out",
                    json!({
                        "from_id": msg.from_id,
                        "content": content,
                        "channel": msg.channel,
                        "timestamp": now_input_ts(),
                        "conversation_id": conversation_id,
                        "total_tool_calls": r.total_calls,
                    }),
                );
                tracing::info!(
                    "[R3] 回复完成 conversation_id={conversation_id} tools={} chars={}",
                    r.total_calls,
                    content.chars().count()
                );
                finish_turn("completed", String::new());
                reply(true, content, r.total_calls, r.aborted, tool_name)
            }
            Err(e) => {
                tracing::error!("[R3] call_llm 失败: {e}");
                let text = format!("（生成回复时出错：{e}）");
                let _ = conversations::insert(&self.db, "agent", &msg.from_id, &text);
                self.bus.emit(
                    "message_out",
                    json!({
                        "from_id": msg.from_id,
                        "content": text,
                        "channel": msg.channel,
                        "timestamp": now_input_ts(),
                        "conversation_id": conversation_id,
                    }),
                );
                finish_turn("failed", format!("call_llm: {e}"));
                reply(false, text, 0, false, None)
            }
        }
    }

    /// 唤醒轮输入行（`[system] ts [tick] 内容`，对齐 parse_message_input 约定）。
    pub fn wakeup_input_line(ts: &str, message: &str) -> String {
        format!("[system] {ts} [tick] {message}")
    }

    /// 后台唤醒轮：合并提醒 → 一次 LLM 调用（stage='wakeup'，走 fast_model）→ 广播。
    /// LLM 未激活 / 调用失败 / 空回复时降级：把合并消息原文广播（提醒不丢，不空转）。
    async fn run_wakeup_turn(&self, wake: CoalescedWakeup) {
        let from_id = "system";
        let channel = "tick";

        // 1) LLM 激活检查：未激活 → 原文广播降级（提醒仍送达）
        let llm_cfg = match LlmConfig::from_config(&self.cfg) {
            Ok(mut c) => {
                // P3-2 模型路由：后台唤醒场景走 fast_model
                c.model = c.route_model("wakeup");
                c
            }
            Err(e) => {
                tracing::warn!("[wakeup] LLM 未激活，原文广播降级: {e}");
                self.bus.emit(
                    "message_out",
                    json!({
                        "from_id": from_id,
                        "content": wake.merged_message,
                        "channel": channel,
                        "timestamp": now_input_ts(),
                    }),
                );
                return;
            }
        };

        // 2) 组装唤醒输入并跑注入闭包
        let input = Self::wakeup_input_line(&now_input_ts(), &wake.merged_message);
        let embedder = NoopEmbedder;
        let window = ContextWindowConfig::default();
        let ctx = InjectorContext::default();

        let turn = {
            let mut guard = self.state.lock().await;
            match run_user_turn(
                &self.db,
                &embedder,
                &mut guard,
                &input,
                channel,
                "",
                &ctx,
                &window,
                &self.agent_name,
                false,
                None,
                "",
                None,
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("[wakeup] run_user_turn 失败: {e}");
                    return;
                }
            }
        };

        // 3) 真实工具执行器（与交互轮一致）
        let executor = self.build_executor();

        // 组装 LLM 消息：LlmMessage → ChatMessage（OpenAI 线协议）
        let chat_messages: Vec<ChatMessage> = turn
            .llm_messages
            .iter()
            .map(|m| ChatMessage {
                role: match m.role {
                    LlmRole::System => ChatRole::System,
                    LlmRole::User => ChatRole::User,
                    LlmRole::Assistant => ChatRole::Assistant,
                },
                content: Some(m.content.clone()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            })
            .collect();

        let args = CallLlmArgs {
            messages: Some(chat_messages),
            tools: all_tool_schemas(),
            local_reply: false,
            must_reply: true,
            ..Default::default()
        };
        let client = reqwest::Client::new();
        let stream = real_stream_fn();
        // M3：唤醒轮显式标 stage='wakeup' → 唤醒成本账本（M4 周报）开始有真实数据
        let ctx = StreamContext {
            stage: "wakeup".into(),
            ..Default::default()
        };

        // 4) LLM 工具循环（P1-2 防重放台账统一启用）
        let replay_guard = DbToolReplayGuard::new(self.db.clone());
        let result = call_llm(
            &client,
            &llm_cfg,
            stream.as_ref(),
            &executor,
            &args,
            &ctx,
            None,
            &ToolLoopLimits::default(),
            Some(&replay_guard),
        )
        .await;

        match result {
            Ok(r) => {
                let content = r.content.trim().to_string();
                if content.is_empty() {
                    // 有/无工具调用都保证提醒送达：空文本时原文兜底
                    tracing::warn!("[wakeup] LLM 空回复，原文兜底");
                    self.bus.emit(
                        "message_out",
                        json!({
                            "from_id": from_id,
                            "content": wake.merged_message,
                            "channel": channel,
                            "timestamp": now_input_ts(),
                            "total_tool_calls": r.total_calls,
                        }),
                    );
                    return;
                }
                self.bus.emit(
                    "message_out",
                    json!({
                        "from_id": from_id,
                        "content": content,
                        "channel": channel,
                        "timestamp": now_input_ts(),
                        "total_tool_calls": r.total_calls,
                    }),
                );
                tracing::info!("[wakeup] 唤醒完成 tools={}", r.total_calls);
            }
            Err(e) => {
                tracing::error!("[wakeup] call_llm 失败: {e}");
                self.bus.emit(
                    "message_out",
                    json!({
                        "from_id": from_id,
                        "content": wake.merged_message,
                        "channel": channel,
                        "timestamp": now_input_ts(),
                    }),
                );
            }
        }
    }

    /// P1-1 接线：后台唤醒循环（TICK 轮）。
    /// 周期查到期提醒 → 合并为 1 次唤醒 → run_wakeup_turn（stage='wakeup'，fast_model）。
    /// 配置项（cfg.extra）：
    ///   wakeup_interval_secs：轮询间隔（默认 60s，最小 1s）
    ///   wakeup_days：周窗口天数（默认 7）
    ///   wakeup_budget_tokens：周窗口唤醒预算（默认 0 = 闸门关闭，纯观测不拦截）
    pub fn spawn_wakeup_loop(&self) -> tokio::task::JoinHandle<()> {
        let runtime = self.clone();
        let interval_secs = self
            .cfg
            .extra
            .get("wakeup_interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(60)
            .max(1);
        let days = self
            .cfg
            .extra
            .get("wakeup_days")
            .and_then(|v| v.as_i64())
            .unwrap_or(7);
        let budget = self
            .cfg
            .extra
            .get("wakeup_budget_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                interval.tick().await;
                let now = now_input_ts();
                match coalesced_wakeup(&runtime.db, &now, days, budget) {
                    Ok(Some(wake)) => {
                        tracing::info!(
                            "[wakeup] {} 条到期提醒合并为 1 次唤醒",
                            wake.trigger_count
                        );
                        runtime.run_wakeup_turn(wake).await;
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("[wakeup] 唤醒轮失败: {e}"),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bailongma_core::db::open_database;

    fn test_runtime() -> AppRuntime {
        // app crate 无 tempfile dev-dep：用系统临时目录 + pid + 原子序号 + 时间戳隔离。
        // （并行测试同时调用时时间戳可能相同 → 目录撞车 → SQLite database is locked）
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blm_service_test_{}_{}_{}",
            std::process::id(),
            seq,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = open_database(dir.join("t.db")).unwrap();
        let bus = EventBus::new(Arc::new(|_, _, _, _| {}));
        let state = Arc::new(Mutex::new(runtime_init(&db).unwrap()));
        let mut cfg = Config::default();
        cfg.extra.insert("wakeup_interval_secs".into(), json!(1));
        AppRuntime::assemble(db, bus, state, cfg, dir.join("sandbox"), None)
    }

    fn insert_due_reminder(db: &Db, due_at: &str, task: &str) -> i64 {
        let conn = db.conn();
        conn.execute(
            "INSERT INTO reminders (user_id, due_at, task, system_message, status, source)
             VALUES (?1, ?2, ?3, ?4, 'pending', 'test')",
            rusqlite::params!["ID:000001", due_at, task, format!("sys:{task}")],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn wakeup_input_line_formats() {
        let line = AppRuntime::wakeup_input_line(
            "2026-08-11T08:00:00+08:00",
            "有 1 条到期提醒待处理：\n- [2026-08-11T08:00:00+08:00] 喂猫",
        );
        assert!(line.starts_with("[system] 2026-08-11T08:00:00+08:00 [tick] "));
        assert!(line.contains("喂猫"));
    }

    #[tokio::test]
    async fn wakeup_loop_broadcasts_merged_message_when_llm_disabled() {
        // LLM 未激活（默认 Config 无 provider）→ 降级路径：合并消息原文广播 + 提醒 fired
        let runtime = test_runtime();
        let rid = insert_due_reminder(&runtime.db, "2026-08-11T08:00:00+08:00", "喂猫提醒");
        let mut rx = runtime.bus.subscribe();

        let _handle = runtime.spawn_wakeup_loop();

        // 首个 tick 立即执行 → 广播合并消息（原文降级）
        let msg = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("应收到 message_out 广播")
            .expect("channel 不应关闭");
        assert_eq!(msg.r#type, "message_out");
        assert_eq!(msg.data["channel"], "tick");
        assert_eq!(msg.data["from_id"], "system");
        let content = msg.data["content"].as_str().unwrap();
        assert!(content.contains("1 条到期提醒"), "内容: {content}");
        assert!(content.contains("喂猫提醒"), "内容: {content}");

        // 消费后提醒标记 fired（不会重复唤醒）
        let conn = runtime.db.conn();
        let status: String = conn
            .query_row("SELECT status FROM reminders WHERE id = ?1", [rid], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "fired");
    }

    #[tokio::test]
    async fn run_message_turn_degrades_when_llm_disabled() {
        // 波2a 验收面：chat 入口走同一装配；LLM 未激活 → 降级回复 + user/agent 两行落库
        let runtime = test_runtime();
        let msg = InboundMessage {
            from_id: "ID:000001".into(),
            content: "你好".into(),
            channel: "TUI".into(),
            meta: json!({}),
        };
        let reply = runtime.run_message_turn(msg, None, None).await;
        assert!(reply.ok, "降级回复视为完成，ok 应为 true");
        assert!(reply.content.contains("LLM 未激活"), "内容: {}", reply.content);
        assert_eq!(reply.conversation_id, 1);

        let rows = conversations::recent_by_from(&runtime.db, "ID:000001", 10).unwrap();
        assert_eq!(rows.len(), 2, "user 入站 + agent 降级回复都应落库");
        assert_eq!(rows[0].role, "agent");
        assert!(rows[0].content.contains("LLM 未激活"));
        assert_eq!(rows[1].role, "user");
        assert_eq!(rows[1].content, "你好");
    }
}
