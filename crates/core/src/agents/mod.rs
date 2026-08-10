//! 本地 AI Agent 探测与委托注册（对齐 `src/agents/registry.js` + `detector.js`）。
//!
//! - [`detector`]      探测层：4 个探针（claude-code/codex/hermes/openclaw）+ 工具函数
//! - [`collect_agents`] 启动入口：探测 → 写库（对齐 `collectAgents`）
//! - [`delegation_discovery`] 一次性本地 Agent 发现上下文（对齐 `buildDelegationDiscoveryContext`）
//!
//! prompt 块生成（AI Collaborators / 一次性发现文本的纯函数半）在
//! [`crate::memory::agent_registry`]；DB 仓库（查询/upsert/委托权限）在
//! [`crate::db::repositories::agents`]。

pub mod delegate;
pub mod detector;

use std::time::Duration;

use crate::db::models::NewKnownAgent;
use crate::db::Db;
use crate::memory::agent_registry::build_delegation_discovery_text;

pub use detector::{detect_agents, DetectedAgent, ProbeResult};

/// 启动扫描预算：对齐 Node `withStartupTimeout(collectAgents(), 15000)` 的 15s 窗口，
/// 略小于 15s 以给外层超时留余量；预算内未完成的探针降级为不可用。
pub const DEFAULT_SCAN_BUDGET: Duration = Duration::from_secs(12);

/// 扫描本机全部 Agent 并写入 `known_agents`（对齐 Node `collectAgents`：
/// detectAgents → saveAgents + 可用数统计日志），带默认启动预算。
///
/// 单个探针失败不中断整轮；写库失败仅记 warn（best-effort，对齐 Node try/catch）。
/// 返回探测结果列表（含不可用项；调用方可据 `available` 过滤）。
pub async fn collect_agents(db: &Db) -> Vec<DetectedAgent> {
    collect_agents_with_budget(db, DEFAULT_SCAN_BUDGET).await
}

/// 带显式预算的探测 + 写库（测试/工具场景可用更小预算快速截断 WSL 探测）。
pub async fn collect_agents_with_budget(db: &Db, budget: Duration) -> Vec<DetectedAgent> {
    tracing::info!("[agents] 开始扫描本地 AI Agent...");
    let results = detector::detect_agents_with_budget(budget).await;
    let found = results.iter().filter(|a| a.available).count();

    let new_agents: Vec<NewKnownAgent> = results
        .iter()
        .map(DetectedAgent::to_new_known_agent)
        .collect();
    if let Err(e) = crate::db::repositories::agents::upsert_agents(db, &new_agents) {
        tracing::warn!("[agents] 写库失败: {e}");
    } else {
        tracing::info!(
            "[agents] 扫描完成：发现 {found}/{} 个可用 Agent",
            results.len()
        );
    }
    results
}

/// 一次性本地 Agent 发现上下文（对齐 Node `buildDelegationDiscoveryContext`）。
///
/// - 已递过（`agent_delegation_asked=true`）→ None；
/// - 无可用 Agent → 也立即 mark（避免每个 TICK 重复扫描同一事实），返回 None；
/// - 有可用 Agent → mark 后返回发现文本（注入主模型一次，不重复）。
///
/// 调用方把返回文本并入注入上下文即可；`mark_delegation_asked` 副作用在本函数内完成
/// （对齐 Node 注入后立即落盘语义）。
pub fn delegation_discovery(db: &Db) -> Option<String> {
    let asked = crate::db::repositories::agents::has_delegation_been_asked(db).unwrap_or(false);
    let available = crate::db::repositories::agents::get_available_agents(db).unwrap_or_default();
    // 无论是否有可用 Agent，都落盘 mark（对齐 Node 两条分支均 mark）
    if !asked {
        if let Err(e) = crate::db::repositories::agents::mark_delegation_asked(db) {
            tracing::warn!("[agents] mark delegation asked 失败: {e}");
        }
    }
    build_delegation_discovery_text(asked, &available)
}

// ── 测试（真实探测；不断言 available 具体值，只验证闭环形状） ───────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        open_database(path).unwrap()
    }

    #[tokio::test]
    async fn collect_agents_detects_and_persists() {
        let db = test_db();
        // 3s 预算：快速完成（WSL 冷启动探测被预算截断为不可用，不影响形状）
        let results = collect_agents_with_budget(&db, Duration::from_secs(3)).await;

        // 形状：4 个探针全部有结果（可用与否取决于本机环境；预算截断也降级为结果）
        assert_eq!(results.len(), 4);
        let ids: Vec<&str> = results.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-code", "codex", "hermes", "openclaw"]);

        // 已写库：全部行都在（含不可用项）
        let all = crate::db::repositories::agents::get_all_agents(&db).unwrap();
        assert_eq!(all.len(), 4);

        // detected_at 已落库（非空）
        for a in &all {
            assert!(!a.detected_at.is_empty(), "{} 缺 detected_at", a.id);
        }

        // 幂等：再跑一遍不产生新行（upsert 覆盖）
        collect_agents_with_budget(&db, Duration::from_secs(3)).await;
        let all2 = crate::db::repositories::agents::get_all_agents(&db).unwrap();
        assert_eq!(all2.len(), 4);
    }

    #[test]
    fn delegation_discovery_marks_once_and_returns_text_or_none() {
        let db = test_db();
        // 第一次：无可用 agent → 返回 None，但已 mark（对齐 Node 无 agent 也 mark）
        assert!(delegation_discovery(&db).is_none());
        assert!(crate::db::repositories::agents::has_delegation_been_asked(&db).unwrap());

        // 第二次：已 mark → 恒 None
        assert!(delegation_discovery(&db).is_none());
    }

    #[test]
    fn delegation_discovery_with_available_agent_returns_names() {
        let db = test_db();
        crate::db::repositories::agents::upsert_agents(
            &db,
            &[NewKnownAgent {
                id: "codex".into(),
                name: "Codex".into(),
                description: String::new(),
                available: true,
                version: None,
                invoke_type: Some("cli".into()),
                invoke_cmd: Some("codex".into()),
                invoke_args: Vec::new(),
                notes: String::new(),
                docs_url: None,
                docs_search_query: None,
                detected_at: None,
            }],
        )
        .unwrap();
        let text = delegation_discovery(&db).expect("未 asked 且有可用 agent → 应返回文本");
        assert!(text.contains("[One-time environment discovery]"));
        assert!(text.contains("Codex"));
        // mark 已落库：再调返回 None
        assert!(delegation_discovery(&db).is_none());
    }
}
