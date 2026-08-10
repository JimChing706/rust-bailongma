//! Bailongma 桌面壳。
//!
//! 当前入口会直接拉起原生窗口，并在需要时自动启动内嵌 API 服务，
//! 用桌面 WebView 承载首页控制台，形成最小可运行的桌面应用。

#[cfg(feature = "desktop")]
use bailongma_app::desktop;
use bailongma_core::compat;
use bailongma_core::config::{load_config, resolve_user_dir};
use bailongma_core::logging::{init_logging, LogConfig};
use bailongma_core::VERSION;

fn main() -> anyhow::Result<()> {
    let log_cfg = LogConfig {
        json: std::env::var("BAILONGMA_JSON_LOG").is_ok(),
        ..LogConfig::default()
    };
    if let Err(e) = init_logging(&log_cfg) {
        eprintln!("[fatal] 日志初始化失败: {e}");
        std::process::exit(1);
    }

    let user_dir = resolve_user_dir()?;
    let cfg = load_config(&user_dir)?;

    tracing::info!("Bailongma v{VERSION} 启动 (Rust 桌面版)");
    tracing::info!("用户数据目录: {}", user_dir.display());
    tracing::info!(
        "LLM provider: {} | 局域网访问: {}",
        if cfg.is_llm_configured() {
            cfg.provider.as_str()
        } else {
            "(未配置)"
        },
        cfg.allow_lan_access()
    );
    tracing::info!("默认 API 端口: {}", compat::DEFAULT_API_PORT);

    #[cfg(feature = "desktop")]
    {
        tracing::info!("正在启动 Bailongma Desktop 原生窗口...");
        desktop::run_desktop_shell()?;
    }
    #[cfg(not(feature = "desktop"))]
    {
        tracing::warn!(
            "桌面功能未启用（构建时需 --features desktop）；请使用 chat / serve / scan_agents 入口"
        );
    }
    Ok(())
}
