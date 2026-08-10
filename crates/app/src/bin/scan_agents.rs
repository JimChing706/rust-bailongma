//! 本地 AI Agent 扫描 → 写库（真实数据闭环验证；对齐 Node `collectAgents`）。
//!
//! 用法：
//! ```text
//! cargo run -p bailongma-app --bin scan_agents
//! BAILONGMA_USER_DIR=<dir> cargo run -p bailongma-app --bin scan_agents   # 指定用户目录
//! ```
//!
//! 行为：运行全部探针（默认 12s 扫描预算，对齐启动超时语义）→ upsert
//! `known_agents` → 打印探测结果 JSON + 写库读回。退出码 0（单个探针失败
//! 不致命，降级为 `available: false` 照常写库）。

use bailongma_core::config::resolve_user_dir;
use bailongma_core::db::repositories::agents;
use bailongma_core::db::Db;
use bailongma_core::error::Result;
use bailongma_core::logging::{init_logging, LogConfig};

#[tokio::main]
async fn main() -> Result<()> {
    if let Err(e) = init_logging(&LogConfig::default()) {
        eprintln!("[fatal] 日志初始化失败: {e}");
        std::process::exit(1);
    }

    // ── 用户目录 + 数据库（与 Node 版共用 jarvis.db；扫描不依赖配置） ──
    let user_dir = resolve_user_dir()?;
    let db_path = user_dir.join("data").join("jarvis.db");
    tracing::info!("数据库: {}", db_path.display());
    let db = Db::open(&db_path)?;

    // ── 探测 → 写库（闭环主体） ──
    let results = bailongma_core::agents::collect_agents(&db).await;
    let found = results.iter().filter(|a| a.available).count();
    println!("[scan] 探测结果（{found}/{} 可用）：", results.len());
    println!("{}", serde_json::to_string_pretty(&results)?);

    // ── 写库读回：证明「探测 → 写库 → 可查」闭环 ──
    let all = agents::get_all_agents(&db)?;
    println!("\n[scan] known_agents 读回（{} 行）：", all.len());
    for a in &all {
        println!(
            "  {:<12} available={:<5} version={:<14} invoke={} {}   notes={}   detected={}",
            a.id,
            a.available,
            a.version.as_deref().unwrap_or("-"),
            a.invoke_type.as_deref().unwrap_or("-"),
            a.invoke_cmd.as_deref().unwrap_or("-"),
            a.notes,
            a.detected_at,
        );
    }

    // 环境事实（本机是否有 claude/codex 等）供人工核对
    Ok(())
}
