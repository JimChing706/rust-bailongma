//! M3 里程碑验证：API 服务器装配（HTTP / SSE / WS）。
//!
//! 用法：
//! ```text
//! cargo run -p bailongma-app --bin serve
//! BAILONGMA_API_TOKEN=secret cargo run -p bailongma-app --bin serve   # 启用 token 校验
//! cargo run -p bailongma-app --bin serve -- --port 3801               # 自定义端口（侧跑验证）
//! ```
//!
//! 验证（另开终端）：
//! ```text
//! curl http://127.0.0.1:3721/status
//! curl -N http://127.0.0.1:3721/events
//! curl -X POST http://127.0.0.1:3721/message -H "Content-Type: application/json" -d '{"content":"你好"}'
//! ```
//!
//! 第 2 轮审计验证：`--port` 支持独立端口侧跑，避免与运行中的桌面实例（3721）
//! 冲突，用于 API 层实测（token 强制校验 / 限流 429）。
//!
//! 波2a：本文件只负责 HTTP 服务器装配（fail-closed 检查 / agents 扫描 / turn 恢复 /
//! 路由挂载）；意识闭环（消息 → turn_state → LLM → 落库 → 广播）统一收敛到
//! [`crate::service::AppRuntime`]，chat / serve / desktop 三入口共用同一装配。

use std::sync::Arc;
use std::time::{Duration, Instant};

use bailongma_core::api::routes::{ApiState, InboundMessage, InboundQueued};
use bailongma_core::api::server::ApiServer;
use bailongma_core::compat;
use bailongma_core::config::resolve_user_dir;
use bailongma_core::error::{CoreError, Result};
use bailongma_core::logging::{init_logging, LogConfig};
use bailongma_core::turn;
use serde_json::json;

use crate::service::AppRuntime;

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
    // 波2a：显式服务层统一装配（与 chat / desktop 共用同一 AppRuntime）
    let runtime = AppRuntime::boot(&user_dir)?;

    // ── 第 3 轮审计检查项：LAN 暴露 fail-closed ──
    // 运行中桌面实例曾以 0.0.0.0:3721 监听且无 token（网内任意设备可直连 /message）。
    // 现在：开 LAN 必须配 token，启动即失败；仅回环（未开 LAN）不受影响。
    let token = std::env::var("BAILONGMA_API_TOKEN").ok();
    let token_configured = !token.as_deref().map(str::trim).unwrap_or("").is_empty();
    let lan = runtime.cfg.allow_lan_access();
    bailongma_core::api::security::lan_exposure_check(lan, token_configured)
        .map_err(CoreError::Api)?;

    tracing::info!("数据库: {}", user_dir.join("data").join("jarvis.db").display());
    tracing::info!("[R3] 工具沙箱根: {}", runtime.tool_root.display());
    if let Some(bin) = &runtime.sandbox_bin {
        tracing::info!("[R3] sandbox 子进程: {}", bin.display());
    } else {
        tracing::warn!("[R3] 未找到 sandbox 子进程，exec_command 将直接执行");
    }

    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        bailongma_core::agents::collect_agents(&runtime.db),
    )
    .await
    {
        Ok(results) => {
            let found = results.iter().filter(|a| a.available).count();
            tracing::info!("[startup] agents 扫描完成: {found}/{} 可用", results.len());
        }
        Err(_) => tracing::warn!("[startup] agents 扫描超时（15s）"),
    }

    // ── Phase 1：启动恢复未终态 turn（received/running/waiting_approval）──
    // 按 recover_policy 决策：resume/retry → running（retry 自动 attempt+1）；
    // mark_failed / retry 超限 → failed；waiting_approval 一律保持挂起等人确认。
    match turn::recover_unfinished_turns(&runtime.db) {
        Ok(s) => tracing::info!(
            "[P1] 启动恢复: recovered={} marked_failed={} held={}",
            s.recovered,
            s.marked_failed,
            s.held
        ),
        Err(e) => tracing::error!("[P1] 启动恢复失败: {e}"),
    }

    // R3：真实意识闭环 —— 入站 → 落库 → 异步 LLM 工具循环 → 回复落库/广播
    // （AppRuntime 统一承载；本文件只负责挂路由）
    let inbound_runtime = runtime.clone();
    let inbound = Arc::new(move |msg: InboundMessage| {
        inbound_runtime
            .spawn_message_turn(msg)
            .map(|conversation_id| InboundQueued { conversation_id })
    });

    // P1-1 接线：后台唤醒循环（合并到期提醒 → 1 次唤醒 → stage='wakeup' LLM 调用）
    let _wakeup_loop = runtime.spawn_wakeup_loop();
    tracing::info!("[wakeup] 后台唤醒循环已启动");

    let status = Arc::new(|| json!({ "running": true }));

    let agent_name = runtime.agent_name.clone();
    let state = ApiState::new(
        runtime.db.clone(),
        runtime.bus.clone(),
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
