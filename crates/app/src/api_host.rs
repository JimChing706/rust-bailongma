use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bailongma_core::api::events::EventBus;
use bailongma_core::api::routes::{ApiState, InboundMessage, InboundQueued};
use bailongma_core::api::server::ApiServer;
use bailongma_core::compat;
use bailongma_core::config::{load_config, resolve_user_dir, Config};
use bailongma_core::db::repositories::{brain_ui_events, conversations};
use bailongma_core::db::Db;
use bailongma_core::embedding::NoopEmbedder;
use bailongma_core::error::{CoreError, Result};
use bailongma_core::llm::caller::{LlmConfig, StreamContext};
use bailongma_core::llm::tool_loop::{
    call_llm, real_stream_fn, CallLlmArgs, ToolLoopLimits,
};
use bailongma_core::llm::types::{ChatMessage, ChatRole};
use bailongma_core::logging::{init_logging, LogConfig};
use bailongma_core::memory::injector::{ContextWindowConfig, InjectorContext};
use bailongma_core::memory::messages::LlmRole;
use bailongma_core::runtime::{init as runtime_init, run_user_turn, RuntimeState};
use bailongma_core::tools::{all_tool_schemas, NativeToolExecutor, SendMessageFn};
use serde_json::json;
use tokio::sync::Mutex;

pub fn app_url() -> String {
    format!("http://127.0.0.1:{}/", compat::DEFAULT_API_PORT)
}

pub fn status_url() -> String {
    format!("http://127.0.0.1:{}/status", compat::DEFAULT_API_PORT)
}

pub async fn is_local_server_ready() -> bool {
    let client = reqwest::Client::new();
    let Ok(resp) = client
        .get(status_url())
        .timeout(Duration::from_secs(2))
        .send()
        .await
    else {
        return false;
    };

    let Ok(value) = resp.json::<serde_json::Value>().await else {
        return false;
    };

    value
        .get("running")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub async fn wait_until_ready(timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if is_local_server_ready().await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CoreError::Api(format!(
                "等待本地 API 服务就绪超时（{}s）",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
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

/// R3 意识闭环：入站消息 → 归属/注入/落库 → LLM 工具循环 → 回复落库 + 广播。
async fn run_conscious_turn(
    db: Db,
    bus: EventBus,
    state: Arc<Mutex<RuntimeState>>,
    cfg: Config,
    tool_root: PathBuf,
    sandbox_bin: Option<PathBuf>,
    msg: InboundMessage,
    conversation_id: i64,
) {
    // 1) LLM 激活检查（未激活 → 降级回复，不空转）
    let llm_cfg = match LlmConfig::from_config(&cfg) {
        Ok(c) => c,
        Err(e) => {
            let reply = format!("（LLM 未激活，本轮无法生成回复：{e}）");
            let _ = conversations::insert(&db, "agent", &msg.from_id, &reply);
            bus.emit(
                "message_out",
                json!({
                    "from_id": msg.from_id,
                    "content": reply,
                    "channel": msg.channel,
                    "timestamp": now_input_ts(),
                    "conversation_id": conversation_id,
                }),
            );
            return;
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
    let agent_name = cfg
        .extra
        .get("agent_name")
        .and_then(|v| v.as_str())
        .unwrap_or(compat::DEFAULT_AGENT_NAME)
        .to_string();

    let turn = {
        let mut guard = state.lock().await;
        match run_user_turn(
            &db,
            &embedder,
            &mut guard,
            &input,
            &msg.channel,
            "",
            &ctx,
            &window,
            &agent_name,
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
                let reply = format!("（本轮处理失败：{e}）");
                let _ = conversations::insert(&db, "agent", &msg.from_id, &reply);
                bus.emit(
                    "message_out",
                    json!({
                        "from_id": msg.from_id,
                        "content": reply,
                        "channel": msg.channel,
                        "timestamp": now_input_ts(),
                        "conversation_id": conversation_id,
                    }),
                );
                return;
            }
        }
    };

    // 3) 真实工具执行器：记忆检索 + send_message 投递 + sandbox 命令委托
    let sender = bus.clone();
    let send_db = db.clone();
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
    let mut executor = NativeToolExecutor::new(tool_root)
        .with_db(db.clone())
        .with_send_message(send_message);
    if let Some(bin) = sandbox_bin {
        executor = executor.with_sandbox(bin);
    }

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
    let ctx = StreamContext::default();

    // 4) LLM 工具循环（真实流式调用 + 9 个真实工具）
    let result = call_llm(
        &client,
        &llm_cfg,
        stream.as_ref(),
        &executor,
        &args,
        &ctx,
        None,
        &ToolLoopLimits::default(),
        None,
    )
    .await;

    match result {
        Ok(r) => {
            let content = r.content.trim().to_string();
            if content.is_empty() {
                if r.total_calls == 0 {
                    tracing::warn!("[R3] LLM 空回复且无工具调用（{conversation_id}）");
                }
                return;
            }
            let _ = conversations::insert(&db, "agent", &msg.from_id, &content);
            bus.emit(
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
        }
        Err(e) => {
            tracing::error!("[R3] call_llm 失败: {e}");
            let reply = format!("（生成回复时出错：{e}）");
            let _ = conversations::insert(&db, "agent", &msg.from_id, &reply);
            bus.emit(
                "message_out",
                json!({
                    "from_id": msg.from_id,
                    "content": reply,
                    "channel": msg.channel,
                    "timestamp": now_input_ts(),
                    "conversation_id": conversation_id,
                }),
            );
        }
    }
}

pub async fn run_api_server() -> Result<()> {
    run_api_server_on(compat::DEFAULT_API_PORT).await
}

/// 第 2 轮审计验证：支持自定义端口侧跑（不与运行中的桌面实例冲突）。
/// 第 3 轮审计验证：启动时强制执行 LAN 暴露 fail-closed 检查——
/// `network.allowLanAccess=true` 必须配置 `BAILONGMA_API_TOKEN`，否则拒绝启动。
pub async fn run_api_server_on(port: u16) -> Result<()> {
    if let Err(e) = init_logging(&LogConfig::default()) {
        eprintln!("[fatal] 日志初始化失败: {e}");
        std::process::exit(1);
    }

    let user_dir = resolve_user_dir()?;
    let cfg = load_config(&user_dir)?;

    // ── 第 3 轮审计检查项：LAN 暴露 fail-closed ──
    // 运行中桌面实例曾以 0.0.0.0:3721 监听且无 token（网内任意设备可直连 /message）。
    // 现在：开 LAN 必须配 token，否则启动即失败；仅回环（未开 LAN）不受影响。
    let token = std::env::var("BAILONGMA_API_TOKEN").ok();
    let token_configured = !token.as_deref().map(str::trim).unwrap_or("").is_empty();
    let lan = cfg.allow_lan_access();
    bailongma_core::api::security::lan_exposure_check(lan, token_configured)
        .map_err(CoreError::Api)?;

    let db_path = user_dir.join("data").join("jarvis.db");
    tracing::info!("数据库: {}", db_path.display());
    let db = Db::open(&db_path)?;

    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        bailongma_core::agents::collect_agents(&db),
    )
    .await
    {
        Ok(results) => {
            let found = results.iter().filter(|a| a.available).count();
            tracing::info!("[startup] agents 扫描完成: {found}/{} 可用", results.len());
        }
        Err(_) => tracing::warn!("[startup] agents 扫描超时（15s）"),
    }

    let persist_db = db.clone();
    let bus = EventBus::new(Arc::new(move |ts, path, ty, payload| {
        brain_ui_events::insert_brain_ui_event(&persist_db, &ts, path, ty, payload);
    }));

    // R3：真实意识闭环 —— 入站 → 落库 → 异步 LLM 工具循环 → 回复落库/广播
    let state = Arc::new(Mutex::new(runtime_init(&db)?));
    let inbound_db = db.clone();
    let inbound_bus = bus.clone();
    let inbound_state = state.clone();
    let inbound_cfg = cfg.clone();
    // 工具沙箱根：user_dir/sandbox（显式目录，不再用进程 cwd）
    let tool_root = sandbox_root(&user_dir);
    tracing::info!("[R3] 工具沙箱根: {}", tool_root.display());
    let sandbox_bin = locate_sandbox_bin();
    if let Some(bin) = &sandbox_bin {
        tracing::info!("[R3] sandbox 子进程: {}", bin.display());
    } else {
        tracing::warn!("[R3] 未找到 sandbox 子进程，exec_command 将直接执行");
    }

    let inbound = Arc::new(move |msg: InboundMessage| {
        // 1) 落库用户消息 → 真实 conversation_id
        let conversation_id = match conversations::insert(&inbound_db, "user", &msg.from_id, &msg.content) {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("[R3] 入站消息落库失败: {e}");
                return None;
            }
        };
        tracing::info!(
            "[R3] 入站 conversation_id={conversation_id} from={} channel={} content={:?}",
            msg.from_id,
            msg.channel,
            msg.content
        );

        // 2) 异步跑意识链路（不阻塞 HTTP 响应）
        let db = inbound_db.clone();
        let bus = inbound_bus.clone();
        let state = inbound_state.clone();
        let cfg = inbound_cfg.clone();
        let tool_root = tool_root.clone();
        let sandbox_bin = sandbox_bin.clone();
        tokio::spawn(async move {
            run_conscious_turn(db, bus, state, cfg, tool_root, sandbox_bin, msg, conversation_id).await;
        });

        Some(InboundQueued { conversation_id })
    });

    let status = Arc::new(|| json!({ "running": true }));

    let agent_name = cfg
        .extra
        .get("agent_name")
        .and_then(|v| v.as_str())
        .unwrap_or(compat::DEFAULT_AGENT_NAME)
        .to_string();

    let state = ApiState::new(
        db,
        bus,
        inbound,
        Arc::new(move || agent_name.clone()),
        status,
    );
    if token_configured {
        tracing::info!("[API] BAILONGMA_API_TOKEN 已配置：/message 强制 token 校验");
    } else {
        tracing::warn!(
            "[API] BAILONGMA_API_TOKEN 未配置：仅回环监听（127.0.0.1）+ 限流保护；\
             如需局域网访问请配置 token 后重启"
        );
    }
    let server = ApiServer::new(state, lan, token);

    let host = if lan { "0.0.0.0" } else { "127.0.0.1" };
    server.serve(host, port).await?;
    Ok(())
}
