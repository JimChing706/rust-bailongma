//! M2 里程碑验证：命令行 TUI 对话。
//!
//! 读取真实 `config.json`（provider=deepseek 等），用 `llm::tool_loop::call_llm`
//! 完成一轮"带工具"对话：模型可调用 `get_time` / `echo` 工具后再回复。
//!
//! 用法：
//! ```text
//! cargo run -p bailongma-app --bin chat -- --message "现在几点了？"
//! cargo run -p bailongma-app --bin chat                # 交互模式（逐行）
//! ```

use std::io::{BufRead, Write};
use std::sync::Arc;

use anyhow::Context;
use bailongma_core::config::{load_config, resolve_user_dir};
use bailongma_core::error::CoreError;
use bailongma_core::llm::caller::{LlmConfig, StreamContext};
use bailongma_core::llm::replay::DbToolReplayGuard;
use bailongma_core::llm::tool_loop::{
    call_llm, real_stream_fn, CallLlmArgs, OnToolCall, ToolExecutor, ToolLoopLimits,
};
use bailongma_core::llm::tools::{enum_param, string_param, ToolSchema};
use bailongma_core::llm::types::{StreamEvent, StreamMode};
use bailongma_core::prelude::Result as CoreResult;
use serde_json::{json, Value};

/// TUI 本地工具执行器（M2 验证用；M5 替换为完整工具能力层）。
/// 持有 `Arc<Db>`：`delegate_to_agent` / `grant_agent_delegation` 需要读写
/// agents 表（与 serve 入口共用 `data/jarvis.db`）。
struct TuiExecutor {
    db: Arc<bailongma_core::db::Db>,
}

impl ToolExecutor for TuiExecutor {
    fn execute(&self, name: &str, args: &Value) -> CoreResult<String> {
        match name {
            "get_time" => {
                let format = args["format"].as_str().unwrap_or("iso");
                let now = chrono::Local::now();
                let time = match format {
                    "unix" => now.timestamp().to_string(),
                    "human" => now.format("%Y-%m-%d %H:%M:%S").to_string(),
                    _ => now.to_rfc3339(),
                };
                Ok(json!({ "ok": true, "time": time, "format": format }).to_string())
            }
            "echo" => Ok(json!({ "ok": true, "echo": args["text"] }).to_string()),
            "delegate_to_agent" => {
                Ok(bailongma_core::agents::delegate::exec_delegate_to_agent(
                    self.db.as_ref(),
                    args,
                ))
            }
            "grant_agent_delegation" => {
                Ok(bailongma_core::agents::delegate::exec_grant_agent_delegation(
                    self.db.as_ref(),
                    args,
                ))
            }
            other => Err(CoreError::Tool(format!("TUI 暂不支持工具: {other}"))),
        }
    }
}

fn tools() -> Vec<ToolSchema> {
    let mut out = vec![
        ToolSchema::new("get_time", "获取当前时间")
            .param("format", enum_param("返回格式", &["iso", "unix", "human"]))
            .param("timezone", string_param("时区（可选，默认本地）")),
        ToolSchema::new("echo", "原样返回传入的文本")
            .required("text", string_param("要回显的文本")),
    ];
    out.push(bailongma_core::agents::delegate::delegate_to_agent_schema());
    out.push(bailongma_core::agents::delegate::grant_agent_delegation_schema());
    out
}

fn system_prompt() -> String {
    "你是小白龙，一个本地运行的桌面助手。回答用户问题前，如果需要当前时间，\
     请调用 get_time 工具；如果用户要求回显文本，请调用 echo 工具。回答简洁、友好。"
        .into()
}

/// 构造流事件回调：思考流带前缀、文本流实时打印
fn on_stream() -> Arc<dyn Fn(StreamEvent) + Send + Sync> {
    Arc::new(|ev| {
        let mut out = std::io::stdout().lock();
        match ev {
            StreamEvent::Start {
                mode: StreamMode::Think,
            } => {
                let _ = write!(out, "🧠 ");
            }
            StreamEvent::Chunk { text } => {
                let _ = write!(out, "{text}");
                let _ = out.flush();
            }
            StreamEvent::End => {
                let _ = writeln!(out);
            }
            StreamEvent::ToolPreparing { name } => {
                let _ = writeln!(out);
                let _ = writeln!(out, "🔧 正在调用工具: {name}…");
            }
            _ => {}
        }
    })
}

fn on_tool_call() -> OnToolCall {
    Arc::new(|name, args, result| {
        println!("  ├─ {name} {args}");
        println!("  └─ 结果: {result}");
    })
}

/// 构造 LlmConfig：key 优先级 = override 参数 > config.json
/// （与 Node 版 fromEnv() 优先于存储配置一致；不写回 config.json）
fn build_llm_config(
    cfg: &bailongma_core::config::Config,
    api_key_override: Option<&str>,
) -> anyhow::Result<LlmConfig> {
    use bailongma_core::llm::providers::{get_provider_config, normalize_model};

    let provider = cfg.provider.trim().to_string();
    if provider.is_empty() {
        return Err(anyhow::anyhow!("config.json 未指定 provider"));
    }
    let api_key = match api_key_override {
        Some(k) if !k.trim().is_empty() => k.trim().to_string(),
        _ => cfg.api_key.clone().unwrap_or_default(),
    };
    if api_key.is_empty() {
        return Err(anyhow::anyhow!(
            "LLM 未激活（apiKey 为空）。请用 --api-key 参数或 DEEPSEEK_API_KEY 环境变量提供"
        ));
    }
    let base_url = cfg
        .base_url
        .clone()
        .unwrap_or_else(|| get_provider_config(&provider).base_url.to_string());
    let model = normalize_model(cfg.model.as_deref(), &provider);
    Ok(LlmConfig {
        provider,
        model,
        api_key,
        base_url,
    })
}

/// 跑一轮对话（非交互）
async fn run_once(message: &str, api_key_override: Option<&str>) -> anyhow::Result<()> {
    let user_dir = resolve_user_dir().context("解析用户目录失败")?;
    let cfg = load_config(&user_dir).context("加载 config.json 失败")?;
    let llm = build_llm_config(&cfg, api_key_override)?;

    // 打开数据库（与 serve 入口共用 `data/jarvis.db`，保证 agent 表 / 授权状态一致）
    let db_path = user_dir.join("data").join("jarvis.db");
    let db = Arc::new(bailongma_core::db::Db::open(&db_path)?);

    println!("┌─ Bailongma TUI (M2 里程碑验证)");
    println!(
        "│  provider: {} | model: {} | base: {}",
        llm.provider, llm.model, llm.base_url
    );
    println!("│  用户: {message}");
    println!("└────────────────────────────────");

    let ctx = StreamContext {
        on_stream: Some(on_stream()),
        ..Default::default()
    };

    let args = CallLlmArgs {
        system_prompt: system_prompt(),
        message: message.to_string(),
        temperature: cfg.temperature,
        thinking: cfg.thinking,
        tools: tools(),
        local_reply: true,
        must_reply: true,
        ..Default::default()
    };

    // ── P1-2 工具防重放：同逻辑请求（request_id+round+tool）复用台账结果，不重复执行 ──
    let replay_guard = DbToolReplayGuard::new((*db).clone());
    let result = call_llm(
        &reqwest::Client::new(),
        &llm,
        real_stream_fn().as_ref(),
        &TuiExecutor {
            db: Arc::clone(&db),
        },
        &args,
        &ctx,
        Some(on_tool_call()),
        &ToolLoopLimits::default(),
        Some(&replay_guard),
    )
    .await
    .context("工具循环调用失败")?;

    println!();
    if result.aborted {
        println!("⏹ 调用被中止");
    }
    if let Some(tr) = &result.tool_result {
        println!("💬 最终回复: {}", result.content);
        println!(
            "🔧 工具轮次: {}/{}（最后执行 {}）",
            result.total_calls, 1, tr.name
        );
    } else {
        println!("💬 回复: {}", result.content);
        println!("🔧 工具轮次: {}", result.total_calls);
    }
    Ok(())
}

/// 交互模式：逐行读取 stdin
async fn run_interactive(api_key_override: Option<&str>) -> anyhow::Result<()> {
    println!("Bailongma TUI 交互模式（Ctrl+C / 输入 exit 退出）");
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "exit" || line == "quit" || line == "q" {
            break;
        }
        if let Err(e) = run_once(&line, api_key_override).await {
            eprintln!("错误: {e:#}");
        }
        println!();
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut message: Option<String> = None;
    let mut api_key: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--message" | "-m" => {
                message = Some(args.next().context("--message 缺少参数值")?);
            }
            "--api-key" => {
                api_key = Some(args.next().context("--api-key 缺少参数值")?);
            }
            "--help" | "-h" => {
                println!("用法: chat [--message \"问题\"] [--api-key <key>]");
                println!("  --api-key 临时覆盖 API Key（不写回 config.json）；");
                println!("  也支持 DEEPSEEK_API_KEY 环境变量；不带参数进入交互模式");
                return Ok(());
            }
            other => return Err(anyhow::anyhow!("未知参数: {other}")),
        }
    }

    // 环境变量兜底：DEEPSEEK_API_KEY（对齐 Node fromEnv 逻辑）
    let api_key = api_key.or_else(|| std::env::var("DEEPSEEK_API_KEY").ok());

    match message {
        Some(msg) => run_once(&msg, api_key.as_deref()).await,
        None => run_interactive(api_key.as_deref()).await,
    }
}
