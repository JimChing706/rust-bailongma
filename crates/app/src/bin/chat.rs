//! M2 里程碑验证：命令行 TUI 对话。
//!
//! 波2a 起：不再维护独立装配（executor / tools / LLM 配置全部删除），
//! 与 serve / desktop 共用 [`bailongma_app::service::AppRuntime`] 同一装配：
//! 入站消息 → 落库 → turn_state 状态机 → 归属/注入 → LLM 工具循环（全量工具）
//! → 回复落库 + 广播。`--api-key` 为临时覆盖（不写回 config.json）。
//!
//! 用法：
//! ```text
//! cargo run -p bailongma-app --bin chat -- --message "现在几点了？"
//! cargo run -p bailongma-app --bin chat                # 交互模式（逐行）
//! ```

use std::io::{BufRead, Write};
use std::sync::Arc;

use anyhow::Context;
use bailongma_app::service::{AppRuntime, TurnReply};
use bailongma_core::api::routes::InboundMessage;
use bailongma_core::config::resolve_user_dir;
use bailongma_core::llm::tool_loop::OnToolCall;
use bailongma_core::llm::types::{StreamEvent, StreamMode};
use serde_json::json;

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

/// 跑一轮对话（非交互；走 AppRuntime 同一意识闭环）
async fn run_once(runtime: &AppRuntime, message: &str) -> anyhow::Result<()> {
    println!("┌─ Bailongma TUI");
    println!(
        "│  provider: {} | model: {}",
        runtime.cfg.provider,
        runtime.cfg.model.as_deref().unwrap_or("default")
    );
    println!("│  用户: {message}");
    println!("└────────────────────────────────");

    let msg = InboundMessage {
        from_id: "ID:000001".into(),
        content: message.to_string(),
        channel: "TUI".into(),
        meta: json!({}),
    };
    let reply: TurnReply = runtime
        .run_message_turn(msg, Some(on_stream()), Some(on_tool_call()))
        .await;

    println!();
    if !reply.ok {
        println!("❌ 本轮失败: {}", reply.content);
        return Ok(());
    }
    if reply.content.is_empty() {
        println!("💬 （本轮无文本回复）");
    } else {
        println!("💬 回复: {}", reply.content);
    }
    println!("🔧 工具轮次: {}", reply.total_calls);
    if let Some(name) = &reply.tool_name {
        println!("  最后执行: {name}");
    }
    if reply.aborted {
        println!("⏹ 调用被中止");
    }
    Ok(())
}

/// 交互模式：逐行读取 stdin
async fn run_interactive(runtime: &AppRuntime) -> anyhow::Result<()> {
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
        if let Err(e) = run_once(runtime, &line).await {
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

    // 波2a：三入口共用 AppRuntime 装配
    let user_dir = resolve_user_dir().context("解析用户目录失败")?;
    let runtime = AppRuntime::boot(&user_dir)
        .context("装配 AppRuntime 失败")?
        .with_api_key(api_key);

    match message {
        Some(msg) => run_once(&runtime, &msg).await,
        None => run_interactive(&runtime).await,
    }
}
