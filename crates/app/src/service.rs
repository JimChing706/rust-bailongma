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
//!
//! 波3·片2（M1 装配收口）：`assemble` 时 `metrics::init(&db)` 挂载 LLM 观测层，
//! 交互轮（stage='interactive'）/ 唤醒轮（stage='wakeup'）的 `StreamContext`
//! 统一挂 `metrics` 采集句柄——埋点链路（CallStarted/TTFT/CallFinished/CallFailed/
//! RetryDecision，caller/retry 已埋）从此开始写真实数据到 llm_calls 三表，
//! M4 周报与唤醒成本账本获得前置输入。
//!
//! 波3·片3（M3 接线 + M4 前置）：两条 turn 管线（交互/唤醒）各挂一个
//! [`TurnSession`]——注入后统计上下文（section 命中 + context_bytes）记入
//! `llm_calls.context_bytes` / `llm_context_sections`，turn 收尾记 `llm_turns`；
//! 生成的稳定 `request_id` 贯穿 StreamContext，使 llm_calls / llm_turns /
//! llm_context_sections 三表可 JOIN（M4 周报的 context 与唤醒成本分析有真实数据）。

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
use bailongma_core::intervention::InterventionGate;
use bailongma_core::llm::caller::{LlmConfig, StreamContext};
use bailongma_core::llm::metrics::{self, FlusherHandle, MetricsCollector, TurnSession};
use bailongma_core::llm::replay::DbToolReplayGuard;
use bailongma_core::llm::tool_loop::{
    call_llm, real_stream_fn, CallLlmArgs, OnToolCall, ToolLoopLimits,
};
use bailongma_core::llm::types::{ChatMessage, ChatRole, StreamEvent};
use bailongma_core::memory::injector::{ContextWindowConfig, InjectorContext};
use bailongma_core::memory::messages::LlmRole;
use bailongma_core::runtime::{init as runtime_init, run_user_turn, RuntimeState, TurnRequest};
use bailongma_core::tools::{all_tool_schemas, NativeToolExecutor, SendMessageFn};
use bailongma_core::wakeup::{due_wakeup, CoalescedWakeup};
use crate::watchdog::{LoopSupervisor, WatchdogState};
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
    /// Q6 人工介入硬通道（config 开关；默认关闭零侵入）。
    pub intervention: Arc<InterventionGate>,
    /// M1 观测：采集句柄（每轮 StreamContext 挂载；Clone 进流路径，只做 mpsc send）。
    pub llm_metrics: MetricsCollector,
    /// M1 观测：flusher 控制句柄（保活后台任务；优雅退出时 shutdown()，M1 可接受不调）。
    pub llm_metrics_flusher: FlusherHandle,
    /// 波5：唤醒循环守护状态（心跳/重启计数；/status 探活数据源）。
    pub wakeup_watchdog: WatchdogState,
    /// 审计 A2：唤醒交付中的提醒 id（防重复唤醒）。
    /// 轮询查到到期提醒 → 置入本集合 → 交付（LLM 唤醒/广播）→ 成功后 mark_fired；
    /// 交付失败仅清集合（提醒保持 pending，下轮重试）。非空时跳过本轮轮询，
    /// 串行化保证同一批提醒不会并发唤醒。
    /// 同步 std Mutex：临界区仅 id 列表增删查，且需在同步 helper 中使用。
    pub wake_inflight: Arc<std::sync::Mutex<Vec<i64>>>,
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
        // S1（审计修复）：用真实 sandbox 根显式初始化全局审批门。
        // 此前仅 server.rs 调 set_global_on_request（隐含 global() 的 temp_dir 兜底），
        // 导致审批门以临时目录为 PolicyEngine 文件边界且语义不透明；chat/serve/desktop
        // 三入口共用本装配 → 这里初始化即覆盖全部生产路径。init_global 幂等（OnceLock）。
        bailongma_core::approval::init_global(tool_root.clone());
        // Q6 人工介入硬通道：随 config 开关装配（默认关闭 = 零侵入）
        let intervention = Arc::new(InterventionGate::new(cfg.intervention.enabled));
        // M1 装配收口：挂载 LLM 观测层（埋点链路已在 caller/retry 就位，此处补齐采集端）。
        // init 需在 tokio runtime 内（内部 spawn flusher）；所有构造路径（chat/serve/desktop）
        // 均从 async 入口进入，测试用 #[tokio::test]，满足该前提。
        let (llm_metrics, llm_metrics_flusher) = metrics::init(db.clone());
        Self {
            db,
            bus,
            state,
            cfg,
            tool_root,
            sandbox_bin,
            agent_name,
            api_key_override: None,
            intervention,
            llm_metrics,
            llm_metrics_flusher,
            wakeup_watchdog: WatchdogState::default(),
            wake_inflight: Arc::new(std::sync::Mutex::new(Vec::new())),
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
            // S1（审计修复）：生产装配必须接线全局审批门。
            // 此前 with_approval 仅存在于测试——approval=None 时 guard_approval 直接放行，
            // exec_command / delete_file / delegate_to_agent 在生产中全部免人工确认。
            // 现与 HTTP POST /approval（提交到同一 global()）闭环：高危工具过 120s
            // fail-closed 审批门（交互轮/唤醒轮共用，唤醒轮无人值守时按超时拒绝）。
            .with_approval(bailongma_core::approval::global())
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
        // 审计 A3 修复：内层 spawn 捕获 panic（JoinError）而不是让裸 panic 传播。
        // 此前 run_message_turn_impl 若有 panic 会直接打到 tokio 全局 panic hook
        // （默认 hook 会 print 但*不杀进程*——风险在 panic 发生在持有非 UnwindSafe
        // 资源处，且日志无任何「消息轮异常」上下文，故障不可观测）。
        // 内层任务 panic 时外层 await JoinHandle 得到 Err(JoinError)，转为结构化日志，
        // 并保证该消息轮不会静默中止（恢复点：下一条消息照常进入新轮）。
        tokio::spawn(async move {
            let inner = tokio::spawn(async move {
                runtime
                    .run_message_turn_impl(msg, conversation_id, None, None)
                    .await;
            });
            if let Err(e) = inner.await {
                if e.is_panic() {
                    tracing::error!("[消息轮] run_message_turn panic（已捕获，不影响后续消息）");
                } else if e.is_cancelled() {
                    tracing::warn!("[消息轮] run_message_turn 被取消");
                }
            }
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
        // A4（审计修复）：消息级幂等——客户端/网桥可通过 meta.idempotency_key 声明
        // 消息唯一标识；入口校验：同 key 已存在且终态 → 直接返回已处理结果，
        // 不重复执行（重复发消息/重复扣费）。缺省 key 时保持现状（会话级占位）。
        let idem_key: String = msg
            .meta
            .get("idempotency_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if !idem_key.is_empty() {
            if let Ok(Some(existing)) = turn_state::find_by_idempotency_key(&self.db, &idem_key) {
                if matches!(
                    existing.state.as_str(),
                    "completed" | "failed" | "cancelled"
                ) {
                    tracing::info!(
                        key = %idem_key,
                        state = %existing.state,
                        "[A4] 幂等命中：消息已处理，跳过重复执行"
                    );
                    return TurnReply {
                        conversation_id,
                        ok: true,
                        content: format!("（幂等命中：消息 {idem_key} 已处理，跳过重复执行）"),
                        total_calls: 0,
                        aborted: false,
                        tool_name: None,
                    };
                }
            }
        }
        let turn_key = if idem_key.is_empty() {
            format!("conv-{conversation_id}")
        } else {
            idem_key
        };
        let turn_id = match turn_state::create_turn(
            &self.db,
            &now_input_ts(),
            &turn_key,
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
            match run_user_turn(TurnRequest {
                db: &self.db,
                embedder: &embedder,
                state: &mut guard,
                input: &input,
                channel: &msg.channel,
                input_hint: "",
                ctx: &ctx,
                window: &window,
                agent_name: &self.agent_name,
                has_active_task: false,
                task: None,
                system_prompt: "",
                msg: None,
            })
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

        // 3) M3 接线（片3）：turn 观测会话 —— 注入后统计上下文（section 命中 +
        // context_bytes）记入 llm_calls.context_bytes / llm_context_sections，
        // turn 收尾记 llm_turns；request_id 贯穿 StreamContext，
        // 使 llm_calls / llm_turns / llm_context_sections 三表可 JOIN。
        let mut session = TurnSession::begin(self.llm_metrics.clone());
        session.record_context_stats(&turn.injection);
        let rid = session.request_id().to_string();

        // 4) 真实工具执行器
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
            intervention: Some(self.intervention.clone()),
            round_request_id_seed: Some(rid.clone()),
            ..Default::default()
        };
        let client = reqwest::Client::new();
        let stream = real_stream_fn();
        // M1 装配收口 + M3：交互轮挂采集句柄 + stage='interactive' + turn 稳定 request_id
        let ctx = StreamContext {
            on_stream: stream_cb,
            metrics: Some(self.llm_metrics.clone()),
            stage: "interactive".into(),
            request_id: Some(rid),
            ..Default::default()
        };

        // 5) LLM 工具循环（真实流式调用 + 全量工具；P1-2 防重放台账统一启用）
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
                        session.finish(turn.outcome.event, false, 0);
                        return reply(false, String::new(), r.total_calls, r.aborted, tool_name);
                    }
                    // 有工具调用但无文本：工具副作用已发生，按完成记账
                    finish_turn("completed", String::new());
                    session.finish(turn.outcome.event, false, r.total_calls as u32);
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
                session.finish(turn.outcome.event, false, r.total_calls as u32);
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
                session.finish(turn.outcome.event, false, 0);
                reply(false, text, 0, false, None)
            }
        }
    }

    /// 审计 A2：唤醒交付成功 → 消费提醒（mark_fired）+ 清 in-flight。
    /// 广播/LLM 内容已送达即视为交付成功（原文兜底也算送达，提醒不丢）。
    fn wakeup_delivered(&self, wake: &CoalescedWakeup) {
        if let Err(e) =
            bailongma_core::db::repositories::reminders::mark_fired(&self.db, &wake.reminder_ids, &now_input_ts())
        {
            // mark_fired 失败：提醒仍是 pending，下轮会重复唤醒（可接受，不丢消息）
            tracing::warn!("[wakeup] mark_fired 失败（下轮可能重复唤醒）: {e}");
        }
        self.wake_inflight.lock().unwrap().clear();
    }

    /// 审计 A2：交付失败 → 仅清 in-flight，提醒保持 pending（下轮自动重试）。
    fn wakeup_dropped(&self) {
        self.wake_inflight.lock().unwrap().clear();
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
                self.wakeup_delivered(&wake); // 审计 A2：原文已送达 → 消费提醒
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
            match run_user_turn(TurnRequest {
                db: &self.db,
                embedder: &embedder,
                state: &mut guard,
                input: &input,
                channel,
                input_hint: "",
                ctx: &ctx,
                window: &window,
                agent_name: &self.agent_name,
                has_active_task: false,
                task: None,
                system_prompt: "",
                msg: None,
            })
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("[wakeup] run_user_turn 失败: {e}");
                    // 审计 A2：交付失败 → 不消费提醒（保持 pending 下轮重试）
                    self.wakeup_dropped();
                    return;
                }
            }
        };

        // 3) M3 接线（片3）：唤醒轮同样挂 turn 观测会话（stage='wakeup' 成本账本 +
        // 上下文统计；turn 收尾记 llm_turns.is_tick=1）
        let mut session = TurnSession::begin(self.llm_metrics.clone());
        session.record_context_stats(&turn.injection);
        let rid = session.request_id().to_string();

        // 4) 真实工具执行器（与交互轮一致）
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
            intervention: Some(self.intervention.clone()),
            round_request_id_seed: Some(rid.clone()),
            ..Default::default()
        };
        let client = reqwest::Client::new();
        let stream = real_stream_fn();
        // M1 装配收口 + M3：唤醒轮挂采集句柄 + stage='wakeup' + turn 稳定 request_id
        let ctx = StreamContext {
            stage: "wakeup".into(),
            metrics: Some(self.llm_metrics.clone()),
            request_id: Some(rid),
            ..Default::default()
        };

        // 5) LLM 工具循环（P1-2 防重放台账统一启用）
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
                    session.finish(turn.outcome.event, true, r.total_calls as u32);
                    self.wakeup_delivered(&wake); // 审计 A2：原文兜底已送达 → 消费
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
                session.finish(turn.outcome.event, true, r.total_calls as u32);
                self.wakeup_delivered(&wake); // 审计 A2：LLM 内容已送达 → 消费
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
                session.finish(turn.outcome.event, true, 0);
                self.wakeup_delivered(&wake); // 审计 A2：原文兜底已送达 → 消费
            }
        }
    }

    /// P1-1 接线：后台唤醒循环（TICK 轮）。
    /// 周期查到期提醒 → 合并为 1 次唤醒 → run_wakeup_turn（stage='wakeup'，fast_model）。
    /// 配置项（cfg.extra）：
    ///   wakeup_interval_secs：轮询间隔（默认 60s，最小 1s）
    ///   wakeup_days：周窗口天数（默认 7）
    ///   wakeup_budget_tokens：周窗口唤醒预算（默认 0 = 闸门关闭，纯观测不拦截）
    /// 波5（唤醒可靠性）：外层套 LoopSupervisor 守护——panic/退出自动重启
    /// （指数退避）、心跳超时假死自愈（abort+重启）、/status 探活。
    /// 新增配置：wakeup_watchdog_timeout_secs（心跳超时，默认 180s，最小 5s）、
    /// wakeup_watchdog_backoff_secs（重启退避基数，默认 1s）。
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
        let watchdog_timeout = Duration::from_secs(
            self.cfg
                .extra
                .get("wakeup_watchdog_timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(180)
                .max(5),
        );
        let backoff_base = Duration::from_secs(
            self.cfg
                .extra
                .get("wakeup_watchdog_backoff_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(1),
        );

        let worker = {
            let runtime = runtime.clone();
            move |wstate: WatchdogState| {
                let runtime = runtime.clone();
                async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
                    loop {
                        interval.tick().await;
                        wstate.beat();
                        let now = now_input_ts();
                        // 审计 A2：上一批提醒仍在交付中 → 跳过本轮（串行化防重复唤醒）。
                        // due_wakeup 只查询不消费，交付成功后才 mark_fired。
                        if !runtime.wake_inflight.lock().unwrap().is_empty() {
                            tracing::debug!("[wakeup] 上一批提醒仍在交付中，跳过本轮");
                            continue;
                        }
                        match due_wakeup(&runtime.db, &now, days, budget) {
                            Ok(Some(wake)) => {
                                tracing::info!(
                                    "[wakeup] {} 条到期提醒合并为 1 次唤醒",
                                    wake.trigger_count
                                );
                                runtime
                                    .wake_inflight
                                    .lock()
                                    .unwrap()
                                    .extend(wake.reminder_ids.iter().copied());
                                runtime.run_wakeup_turn(wake).await;
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!("[wakeup] 唤醒轮失败: {e}"),
                        }
                    }
                }
            }
        };

        // 波5：每次重启落 brain_ui_events（自愈事件可观测）+ error 日志
        let on_restart = {
            let db = runtime.db.clone();
            move |reason: &str| {
                tracing::error!("[wakeup] 循环重启（{reason}），watchdog 已拉起新循环");
                brain_ui_events::insert_brain_ui_event(
                    &db,
                    &now_input_ts(),
                    "l2",
                    "wakeup_restart",
                    &json!({ "reason": reason }),
                );
            }
        };

        LoopSupervisor::spawn(
            self.wakeup_watchdog.clone(),
            worker,
            watchdog_timeout,
            backoff_base,
            on_restart,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bailongma_core::db::open_database;
    use bailongma_core::llm::metrics::MetricEvent;

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

    #[tokio::test]
    async fn production_executor_wires_global_approval() {
        // S1（审计修复）：生产 executor 必须接线全局审批门；exec_command 必须
        // 产生挂起审批请求（而非 approval=None 直接放行的旧行为）。
        let runtime = test_runtime();
        let ex = runtime.build_executor();
        assert!(
            ex.approval.is_some(),
            "S1 修复：生产 executor 必须挂载全局审批门"
        );

        // 核心语义：exec_command 走全局门 → 出现新的挂起请求 → deny 后按拒绝返回
        let gate = bailongma_core::approval::global();
        let before: Vec<String> = gate.pending_ids();
        let g2 = gate.clone();
        let handle = std::thread::spawn(move || {
            g2.guard_tool_call("exec_command", "whoami").unwrap()
        });
        let mut id: Option<String> = None;
        for _ in 0..200 {
            if let Some(nid) = gate.pending_ids().iter().find(|i| !before.contains(i)) {
                id = Some(nid.clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let id = id.expect("exec_command 应产生挂起审批请求（而非生产免审批放行）");
        gate.submit(&id, "deny").unwrap();
        assert!(
            matches!(
                handle.join().unwrap(),
                bailongma_core::approval::GuardResult::Denied(_)
            ),
            "deny 后 guard 应按拒绝返回"
        );
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
    async fn wakeup_watchdog_heartbeat_visible_after_first_tick() {
        // 波5：健康循环心跳新鲜、零重启；快照可探活
        let runtime = test_runtime();
        insert_due_reminder(&runtime.db, "2026-08-11T08:00:00+08:00", "探活测试");
        let mut rx = runtime.bus.subscribe();
        let _handle = runtime.spawn_wakeup_loop();
        let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("首个 tick 应广播")
            .expect("channel 不应关闭");
        assert_eq!(runtime.wakeup_watchdog.restart_count(), 0);
        assert!(runtime.wakeup_watchdog.heartbeat_age() < Duration::from_secs(5));
        let snap = runtime.wakeup_watchdog.snapshot();
        assert_eq!(snap["restart_count"], 0);
        assert!(snap["last_heartbeat"].is_string());
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

    #[tokio::test]
    async fn metrics_collector_wired_into_runtime_and_persists() {
        // 波3·片2 验收：assemble 已挂载观测层 —— 采集句柄记录的事件
        // 经 flusher 落库到 llm_calls / llm_metrics_daily（真实装配，非 core 单测）
        let runtime = test_runtime();
        let rid = format!(
            "service-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        runtime.llm_metrics.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-12T10:00:00+08:00".into(),
            stage: "interactive".into(),
        });
        runtime.llm_metrics.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 900,
            total_tokens: 64,
            cached_tokens: 16,
            usage_raw: "{}".into(),
            aborted: false,
        });
        // 幂等验证：同 request_id 重放终态 → 仍只有一行
        runtime.llm_metrics.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 900,
            total_tokens: 64,
            cached_tokens: 16,
            usage_raw: "{}".into(),
            aborted: false,
        });
        runtime.llm_metrics_flusher.flush_now().await;

        let (n, total, cached, reason): (i64, Option<i64>, Option<i64>, String) = runtime
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*), total_tokens, cached_tokens, finish_reason
                 FROM llm_calls WHERE request_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "同 request_id 必须只有一行（幂等）");
        assert_eq!(total, Some(64));
        assert_eq!(cached, Some(16));
        assert_eq!(reason, "done");

        let calls: i64 = runtime
            .db
            .conn()
            .query_row("SELECT total_calls FROM llm_metrics_daily", [], |r| r.get(0))
            .unwrap();
        assert_eq!(calls, 1, "日聚合 total_calls 只计一次");
    }

    #[tokio::test]
    async fn m3_turn_session_persists_context_and_turn_via_assembly() {
        // 波3·片3 验收：TurnSession（两条 turn 管线实际使用的观测会话）在真实装配下
        // 走完整 turn 事件链 → llm_calls / llm_context_sections / llm_turns 三表
        // JOIN 可用（M3 验收核心：context_bytes 与 turn 归属可关联）。
        let runtime = test_runtime();
        let mut session = TurnSession::begin(runtime.llm_metrics.clone());
        let rid = session.request_id().to_string();

        // 模拟注入输出（含 2 个非空 section）
        let mut injection = bailongma_core::memory::injector::InjectorOutput::default();
        injection.directions.push("继续收口主链".into());
        injection.tools.push("web_search".into());
        let stats = session.record_context_stats(&injection);
        assert!(stats.sections_hit >= 2, "2 个非空 section 应命中");
        assert!(stats.context_bytes > 0);

        // LLM 调用（stage=interactive，request_id 与 turn 一致——service 接线的形态）
        runtime.llm_metrics.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-12T10:00:00+08:00".into(),
            stage: "interactive".into(),
        });
        runtime.llm_metrics.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 1200,
            total_tokens: 80,
            cached_tokens: 20,
            usage_raw: "{}".into(),
            aborted: false,
        });
        session.finish("created", false, 1);
        runtime.llm_metrics_flusher.flush_now().await;

        // llm_calls.context_bytes 已关联
        let ctx_bytes: Option<i64> = runtime
            .db
            .conn()
            .query_row(
                "SELECT context_bytes FROM llm_calls WHERE request_id = ?1",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ctx_bytes, Some(stats.context_bytes as i64));

        // section 明细 JOIN llm_calls（M3 验收核心）
        let joins: i64 = runtime
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM llm_context_sections s
                 JOIN llm_calls c ON c.request_id = s.request_id
                 WHERE c.request_id = ?1",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(joins, stats.sections_hit as i64, "section 明细都应 JOIN 到 llm_calls");

        // turn 级记录（attribution / is_tick / sections_hit）
        let (attribution, is_tick, sections_hit): (String, i64, Option<i64>) = runtime
            .db
            .conn()
            .query_row(
                "SELECT attribution, is_tick, sections_hit FROM llm_turns WHERE turn_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(attribution, "created");
        assert_eq!(is_tick, 0, "交互轮 is_tick=0");
        assert_eq!(sections_hit, Some(stats.sections_hit as i64));
    }
}
