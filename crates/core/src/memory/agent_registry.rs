//! Agent 委托注册表（对齐 `src/agents/registry.js` 的 prompt 块生成半）。
//!
//! 纯函数层：输入（可用 Agent 列表 + 委托授权状态）由调用方从
//! [`crate::db::repositories::agents`] 查询后传入，本模块只做文本组装与门控判定。
//! 本地扫描（`agents/detector.js` 的 detectAgents）属运行时探测，后续里程碑接入。

use crate::db::models::KnownAgent;

/// 构建「AI Collaborators」上下文块（对齐 `buildAgentContextBlock`）。
///
/// 仅当委托已被授权且存在可用 Agent 时返回 Some；否则 None（Node 返回空串，
/// 调用方不拼接即等价）。
pub fn build_agent_context_block(allowed: bool, agents: &[KnownAgent]) -> Option<String> {
    if !allowed || agents.is_empty() {
        return None;
    }
    let lines = agents
        .iter()
        .map(|a| {
            // 对齐 Node：cli 类用 exec_command 起，其余（URL/API 类）用 web_read 读端点
            let invoke_cmd = a.invoke_cmd.as_deref().unwrap_or("");
            let invoke = if a.invoke_type.as_deref() == Some("cli") {
                format!("exec_command(\"{invoke_cmd} ...\")")
            } else {
                format!("web_read({{ url: \"{invoke_cmd}/...\" }})")
            };
            format!(
                "- **{}** ({}): {}. Invoke: {}",
                a.name, a.id, a.description, invoke
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    Some(format!(
        "## AI Collaborators You Can Work With\n\
You have been granted command authority. For complex tasks, you may invoke the following agents through the delegate_to_agent tool:\n\
{lines}\n\
Before invoking, tell the user what you intend to have whom do, and proceed only after confirmation."
    ))
}

/// 一次性本地 Agent 发现上下文（对齐 `buildDelegationDiscoveryContext` 的文本半）。
///
/// 只把环境事实交给主模型，不替它决定是否、何时向用户提起，也不强制发消息。
/// `asked` 为 [`crate::db::repositories::agents::has_delegation_been_asked`] 的投影；
/// mark 副作用由调用方负责：`asked == false` 时无论是否返回文本都应调用
/// `mark_delegation_asked`（对齐 Node 的注入后立即落盘语义）。
pub fn build_delegation_discovery_text(asked: bool, available: &[KnownAgent]) -> Option<String> {
    if asked || available.is_empty() {
        return None;
    }
    let names = available
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join("、");
    Some(format!(
        "[One-time environment discovery] The following local AI collaborators are available: {names}. This is context, not a request to contact the user. Decide whether this capability matters to the current situation. Delegating work still requires persisted user authorization through grant_agent_delegation; discovery alone grants no authority."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, name: &str, invoke_type: &str, invoke_cmd: &str) -> KnownAgent {
        KnownAgent {
            id: id.into(),
            name: name.into(),
            description: format!("{name} 的说明"),
            available: true,
            version: Some("1.0.0".into()),
            invoke_type: Some(invoke_type.into()),
            invoke_cmd: Some(invoke_cmd.into()),
            invoke_args: Vec::new(),
            notes: String::new(),
            docs_url: None,
            docs_search_query: None,
            detected_at: "2026-08-09T00:00:00.000Z".into(),
            updated_at: "2026-08-09T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn not_allowed_returns_none() {
        let agents = vec![agent("claude-code", "Claude Code", "cli", "claude")];
        assert!(build_agent_context_block(false, &agents).is_none());
        assert!(build_agent_context_block(true, &[]).is_none());
    }

    #[test]
    fn allowed_renders_cli_and_web_invoke() {
        let agents = vec![
            agent("claude-code", "Claude Code", "cli", "claude"),
            agent("hermes", "Hermes", "api", "http://localhost:3333"),
        ];
        let block = build_agent_context_block(true, &agents).expect("allowed + non-empty");
        assert!(block.starts_with("## AI Collaborators You Can Work With"));
        assert!(block.contains("- **Claude Code** (claude-code): Claude Code 的说明. Invoke: exec_command(\"claude ...\")"));
        assert!(block.contains("- **Hermes** (hermes): Hermes 的说明. Invoke: web_read({ url: \"http://localhost:3333/...\" })"));
        assert!(block.contains("delegate_to_agent"));
    }

    #[test]
    fn discovery_text_once_only() {
        let agents = vec![agent("codex", "Codex", "cli", "codex")];
        let text = build_delegation_discovery_text(false, &agents).expect("first time");
        assert!(text.contains("Codex"));
        assert!(text.contains("[One-time environment discovery]"));
        // 已问过 → None（不再重复递）
        assert!(build_delegation_discovery_text(true, &agents).is_none());
        // 无可用 agent → None
        assert!(build_delegation_discovery_text(false, &[]).is_none());
    }
}
