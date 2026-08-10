//! PolicyEngine（Phase 1 安全基线）。
//!
//! 计划（bailongma-multiagent-enhancement Phase 1「PolicyEngine 初版 policy」）：
//! - 集中决策入口：`check_tool_call` / `check_file_access` / `check_network_access` /
//!   `check_memory_access` / `check_output_release`；
//! - 输出：allow / deny / require_user_approval / sanitize / limit_scope；
//! - 每次决策写入审计日志（内存环形，后续接线可落 DB）。
//!
//! 设计约束：
//! - **纯决策模块**：不访问文件系统、不联网、不产生副作用，决策只依赖入参 + 静态策略表；
//!   这样调用方（tool_loop / 人工确认前端）可以随意重放、dry-run，不会有二次效应。
//! - **fail-closed**：任何无法识别 / 无法判定的输入一律 `Deny`，宁可误伤不可漏放。
//! - 能力声明（风险等级 / denylist / output_policy）来自 `crate::capability`；
//!   本模块只消费，不重复定义。
//!
//! 接线说明：`ToolExecutor` 当前在 `tools/mod.rs` 直接分发执行；Phase 1 收尾时
//! tool_loop 会在调用前先过本引擎（`require_user_approval` 挂起等待人工确认）。

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::capability::{builtin, is_path_denied, trust_tier, CallerTrust, TrustTier};

/// 决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// 放行
    Allow,
    /// 拒绝（reason 供审计 / 回显）
    Deny(String),
    /// 需要人工确认（Phase 1 人工确认机制的落点）
    RequireApproval(String),
    /// 脱敏后放行（redacted 为脱敏后内容）
    Sanitize { note: String, redacted: String },
    /// 限缩作用域后放行（scope 说明限缩后的边界）
    LimitScope { note: String, scope: String },
}

impl PolicyDecision {
    /// 是否允许继续（Allow / Sanitize / LimitScope 都算通过）。
    pub fn is_allowed(&self) -> bool {
        matches!(
            self,
            PolicyDecision::Allow
                | PolicyDecision::Sanitize { .. }
                | PolicyDecision::LimitScope { .. }
        )
    }

    /// 决策摘要（审计 / 日志用）。
    pub fn summary(&self) -> String {
        match self {
            PolicyDecision::Allow => "allow".into(),
            PolicyDecision::Deny(r) => format!("deny: {r}"),
            PolicyDecision::RequireApproval(r) => format!("require_approval: {r}"),
            PolicyDecision::Sanitize { note, .. } => format!("sanitize: {note}"),
            PolicyDecision::LimitScope { note, scope } => {
                format!("limit_scope: {note} ({scope})")
            }
        }
    }
}

/// 审计条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Unix 毫秒时间戳
    pub ts_ms: u64,
    /// 被检查的动作（工具名 / 路径 / url / 操作名）
    pub action: String,
    /// 决策摘要
    pub decision: String,
}

/// 网络访问 denylist（云元数据 / 本服务端口 / 保留地址）。
/// Phase 1 只做静态名单；DNS 解析留待 Phase 4 网络策略细化。
const NETWORK_DENY_EXACT: &[&str] = &[
    "169.254.169.254",          // AWS/GCP/Azure 云元数据
    "metadata.google.internal", // GCP 元数据域名
    "metadata.azure.internal",  // Azure 元数据域名
];

const NETWORK_PRIVATE_PREFIXES: &[&str] = &[
    "127.", "10.", "192.168.", "172.16.", "172.17.", "172.18.", "172.19.",
    "172.20.", "172.21.", "172.22.", "172.23.", "172.24.", "172.25.",
    "172.26.", "172.27.", "172.28.", "172.29.", "172.30.", "172.31.",
    "::1", "fe80:",
];

/// 输出脱敏扫描模式（命中即脱敏，不做正则依赖）。
/// 覆盖：PEM 私钥 / 云密钥 / 常见 token 前缀 / 超长 hex。
const SECRET_PATTERNS: &[(&str, &str)] = &[
    ("BEGIN [A-Z ]*PRIVATE KEY", "PRIVATE_KEY"),
    ("AKIA[0-9A-Z]{16}", "AWS_KEY"),
    ("sk-[A-Za-z0-9]{16,}", "API_TOKEN"),
    ("ghp_[A-Za-z0-9]{20,}", "GITHUB_TOKEN"),
    ("xox[baprs]-[A-Za-z0-9-]{10,}", "SLACK_TOKEN"),
];

/// 默认敏感路径 denylist（与 `capability::SENSITIVE_DENY` 对齐并补充）。
const POLICY_DENY_PATHS: &[&str] = &[
    ".ssh", "id_rsa", "id_ed25519", ".env", "credentials", "secret",
    "id_dsa", "id_ecdsa", ".pem", ".key",
];

/// 记忆写操作的策略：默认需人工确认。
const MEMORY_WRITE_REQUIRES_APPROVAL: bool = true;

/// 策略引擎。
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    /// 文件操作白名单根（对齐 tools 的 root 约束）
    workspace_root: PathBuf,
    /// 审计轨迹（内存；后续接线落 DB）
    audit: Vec<AuditEntry>,
    /// 审计轨迹上限（环形裁剪）
    audit_cap: usize,
}

impl PolicyEngine {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            audit: Vec::new(),
            audit_cap: 10_000,
        }
    }

    // ── 审计 ──

    fn record(&mut self, action: &str, decision: &PolicyDecision) {
        self.audit.push(AuditEntry {
            ts_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            action: action.to_string(),
            decision: decision.summary(),
        });
        if self.audit.len() > self.audit_cap {
            let excess = self.audit.len() - self.audit_cap;
            self.audit.drain(..excess);
        }
    }

    /// 审计轨迹（只读）。
    pub fn audit_trail(&self) -> &[AuditEntry] {
        &self.audit
    }

    // ── 工具调用 ──

    /// 检查工具调用：未知工具一律拒绝（fail-closed）；能力声明需要确认且未授权 → 挂起。
    /// `approved` = 该工具本轮是否已获得用户确认（人工确认机制回传后置 true）。
    pub fn check_tool_call(&mut self, name: &str, approved: bool) -> PolicyDecision {
        // 默认按终端用户来源评估（保持旧行为语义）
        self.check_tool_call_with_caller(name, CallerTrust::User, approved)
    }

    /// 检查工具调用（P2-2 信任分层版）：未知工具一律拒绝（fail-closed）；
    /// 需确认工具按来源分层——System 内部自动化直接放行，User/Agent
    /// 必须已获人工确认；已确认一律放行。
    pub fn check_tool_call_with_caller(
        &mut self,
        name: &str,
        caller: CallerTrust,
        approved: bool,
    ) -> PolicyDecision {
        let decision = match builtin(name) {
            None => PolicyDecision::Deny(format!("未知工具: {name}")),
            Some(cap) if cap.needs_approval() => {
                if caller == CallerTrust::System || approved {
                    PolicyDecision::Allow
                } else {
                    PolicyDecision::RequireApproval(format!(
                        "工具 {name}（风险 {}）需人工确认",
                        cap.risk_level.as_str()
                    ))
                }
            }
            Some(_) => PolicyDecision::Allow,
        };
        self.record(&format!("tool_call:{name}"), &decision);
        decision
    }

    /// 工具信任等级查询（供上层展示 / 决策前分级）。
    pub fn tool_trust_tier(&self, name: &str) -> TrustTier {
        trust_tier(name)
    }

    // ── 文件访问 ──

    /// 检查文件路径访问：必须在 workspace root 内（组件级），且不命中敏感 denylist。
    /// 纯路径判定，不触碰文件系统（路径可尚不存在）。
    pub fn check_file_access(&mut self, path: &str) -> PolicyDecision {
        let decision = self.file_decision(path);
        self.record(&format!("file_access:{path}"), &decision);
        decision
    }

    fn file_decision(&self, path: &str) -> PolicyDecision {
        let full = self.workspace_root.join(path);
        if !is_within(&self.workspace_root, &full) {
            return PolicyDecision::Deny(format!("路径越界（不在 workspace 内）: {path}"));
        }
        if is_path_denied(path, POLICY_DENY_PATHS) {
            return PolicyDecision::Deny(format!("路径命中敏感 denylist: {path}"));
        }
        PolicyDecision::Allow
    }

    // ── 网络访问 ──

    /// 检查网络访问：云元数据 / 本服务端口一律拒绝；私网地址段需人工确认；公网放行。
    pub fn check_network_access(&mut self, url: &str) -> PolicyDecision {
        let decision = network_decision(url);
        self.record(&format!("network:{url}"), &decision);
        decision
    }

    // ── 记忆访问 ──

    /// 检查记忆访问：读放行；写默认需人工确认（系统级写例外由调用方以 `system:true` 传入）。
    pub fn check_memory_access(&mut self, op: &str, system: bool) -> PolicyDecision {
        let decision = if op == "read" {
            PolicyDecision::Allow
        } else if system {
            PolicyDecision::Allow
        } else if MEMORY_WRITE_REQUIRES_APPROVAL {
            PolicyDecision::RequireApproval(format!("记忆写入（{op}）需人工确认"))
        } else {
            PolicyDecision::Allow
        };
        self.record(&format!("memory:{op}"), &decision);
        decision
    }

    // ── 输出释放 ──

    /// 检查输出内容（回喂 LLM / 展示给用户 / 对外发送前）：扫描密钥模式，命中则脱敏。
    /// 返回 `Sanitize` 时调用方必须使用 `redacted` 内容替换原文。
    pub fn check_output_release(&mut self, content: &str) -> PolicyDecision {
        let decision = sanitize_content(content);
        self.record(&format!("output_release:{} bytes", content.len()), &decision);
        decision
    }

    /// 当前 workspace 根（供调用方展示 / 调试）。
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }
}

// ─────────────────────────────────────────────────────────────
// 内部工具函数
// ─────────────────────────────────────────────────────────────

/// 组件级前缀判定：`child` 是否在 `root` 内（含相等）。
///
/// fail-closed 要点：
/// - 任何 `..`（ParentDir）组件直接拒绝——`C:\workspace\..\outside.txt`
///   这类逃逸不做文件系统规范化也能被纯组件判定拦住；
/// - 不同盘符 / 同前缀兄弟目录（workspace vs workspace2）因组件不匹配被拒绝。
fn is_within(root: &Path, child: &Path) -> bool {
    if child.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let mut r = root.components();
    let mut c = child.components();
    loop {
        match (r.next(), c.next()) {
            (Some(rr), Some(cc)) => {
                if rr != cc {
                    return false;
                }
            }
            (Some(_), None) => return false, // child 比 root 短 → 在 root 之上
            (None, _) => return true,        // root 是 child 前缀（含相等）
        }
    }
}

/// 从 URL 提取 host（不含端口、不含 IPv6 方括号）。不解析 DNS、不做网络请求。
fn host_of(url: &str) -> Option<&str> {
    let rest = url.split("://").nth(1)?;
    let host_port = rest.split(['/', '?', '#']).next()?;
    // 剔除端口：仅当尾部 `:` 之后是纯数字段（IPv4 端口）时剥离；
    // IPv6 `[::1]:8080` 会先剥出 `[::1]`，再去方括号。
    let no_port = match host_port.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host_port,
    };
    Some(no_port.trim_start_matches('[').trim_end_matches(']'))
}

fn is_private_host(host: &str) -> bool {
    NETWORK_PRIVATE_PREFIXES
        .iter()
        .any(|p| host.starts_with(p))
}

fn network_decision(url: &str) -> PolicyDecision {
    let Some(host) = host_of(url) else {
        return PolicyDecision::Deny(format!("无法解析 URL: {url}"));
    };
    if NETWORK_DENY_EXACT
        .iter()
        .any(|d| host.eq_ignore_ascii_case(d))
    {
        return PolicyDecision::Deny(format!("网络目标被策略拒绝（云元数据/保留地址）: {host}"));
    }
    if is_private_host(host) {
        return PolicyDecision::RequireApproval(format!(
            "网络目标为私网/本机地址段，需人工确认: {host}"
        ));
    }
    PolicyDecision::Allow
}

/// 密钥模式脱敏：将命中片段替换为 `[REDACTED:<kind>]`。
fn sanitize_content(content: &str) -> PolicyDecision {
    let mut redacted = content.to_string();
    let mut hit: Option<&str> = None;
    for (pat, kind) in SECRET_PATTERNS {
        // 不做正则引擎：按分隔词扫描 + 长 token 启发式
        if pattern_hits(&redacted, pat) {
            redacted = redact_line_hits(&redacted, kind);
            hit = Some(kind);
        }
    }
    // 超长连续 hex/base64（>=64 字符）启发式兜底
    if long_token_hits(&redacted) {
        redacted = redact_long_tokens(&redacted);
        hit = Some("LONG_TOKEN");
    }
    match hit {
        Some(kind) => PolicyDecision::Sanitize {
            note: format!("输出包含疑似密钥（{kind}），已脱敏"),
            redacted,
        },
        None => PolicyDecision::Allow,
    }
}

/// 简单子串/模式命中（无正则依赖）：`BEGIN ... PRIVATE KEY` 变体按关键词扫描。
fn pattern_hits(s: &str, pat: &str) -> bool {
    if pat.contains("BEGIN") {
        s.contains("PRIVATE KEY") && s.contains("BEGIN")
    } else if pat.starts_with("AKIA") {
        s.split_whitespace().any(|w| {
            w.len() >= 20 && w.starts_with("AKIA") && w.chars().all(|c| c.is_ascii_alphanumeric())
        })
    } else if pat.starts_with("sk-") {
        s.split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .any(|w| w.len() >= 20 && w.starts_with("sk-"))
    } else if pat.starts_with("ghp_") {
        s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|w| w.len() >= 24 && w.starts_with("ghp_"))
    } else if pat.starts_with("xox") {
        s.split_whitespace().any(|w| {
            w.len() >= 15 && (w.starts_with("xoxb-") || w.starts_with("xoxp-"))
        })
    } else {
        s.contains(pat)
    }
}

/// 将整行命中密钥的行整体替换为占位。
fn redact_line_hits(s: &str, kind: &str) -> String {
    s.lines()
        .map(|line| {
            let trimmed = line.trim();
            let has_key_marker = trimmed.contains("PRIVATE KEY")
                || trimmed.starts_with("AKIA")
                || trimmed.contains("sk-")
                || trimmed.contains("ghp_")
                || trimmed.starts_with("xoxb-")
                || trimmed.starts_with("xoxp-");
            if has_key_marker {
                format!("[REDACTED:{kind}]")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn long_token_hits(s: &str) -> bool {
    s.split_whitespace().any(|w| {
        w.len() >= 64
            && (w.chars().all(|c| c.is_ascii_hexdigit()) || is_base64ish(w))
    })
}

fn is_base64ish(w: &str) -> bool {
    w.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
}

fn redact_long_tokens(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            if w.len() >= 64
                && (w.chars().all(|c| c.is_ascii_hexdigit()) || is_base64ish(w))
            {
                format!("[REDACTED:LONG_TOKEN]({} chars)", w.len())
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ─────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 信任分层：System 可放行需确认工具；User/Agent 未确认则挂起；未知工具恒拒。
    #[test]
    fn caller_trust_tiers() {
        let mut e = PolicyEngine::new("C:/ws");
        // System 放行 exec_command（不要求 approved）
        assert_eq!(
            e.check_tool_call_with_caller("exec_command", CallerTrust::System, false),
            PolicyDecision::Allow
        );
        // Agent 未确认 → 挂起
        assert!(matches!(
            e.check_tool_call_with_caller("exec_command", CallerTrust::Agent, false),
            PolicyDecision::RequireApproval(_)
        ));
        // User 已确认 → 放行
        assert_eq!(
            e.check_tool_call_with_caller("exec_command", CallerTrust::User, true),
            PolicyDecision::Allow
        );
        // 低风险工具任何来源都放行
        assert_eq!(
            e.check_tool_call_with_caller("get_timestamp", CallerTrust::Agent, false),
            PolicyDecision::Allow
        );
        // 未知工具恒拒（即使 System + approved）
        assert!(matches!(
            e.check_tool_call_with_caller("format_c:", CallerTrust::System, true),
            PolicyDecision::Deny(_)
        ));
        // tier 查询
        assert_eq!(e.tool_trust_tier("exec_command"), TrustTier::Approval);
        assert_eq!(e.tool_trust_tier("read_file"), TrustTier::Trusted);
        assert_eq!(e.tool_trust_tier("nope"), TrustTier::Denied);
    }

    use super::*;

    fn engine() -> PolicyEngine {
        PolicyEngine::new(r"C:\workspace")
    }

    #[test]
    fn unknown_tool_denied() {
        let mut e = engine();
        let d = e.check_tool_call("rm_rf_system", false);
        assert!(matches!(d, PolicyDecision::Deny(_)));
        assert!(!d.is_allowed());
    }

    #[test]
    fn exec_requires_approval_unless_granted() {
        let mut e = engine();
        assert!(matches!(
            e.check_tool_call("exec_command", false),
            PolicyDecision::RequireApproval(_)
        ));
        assert!(matches!(
            e.check_tool_call("exec_command", true),
            PolicyDecision::Allow
        ));
        // 低风险工具无需确认
        assert!(matches!(
            e.check_tool_call("get_timestamp", false),
            PolicyDecision::Allow
        ));
    }

    #[test]
    fn file_traversal_denied() {
        let mut e = engine();
        assert!(matches!(
            e.check_file_access(r"..\..\Windows\system32\config"),
            PolicyDecision::Deny(_)
        ));
        assert!(matches!(
            e.check_file_access(r"C:\Windows\system32\sam"),
            PolicyDecision::Deny(_)
        ));
        assert!(matches!(
            e.check_file_access("notes.md"),
            PolicyDecision::Allow
        ));
        assert!(matches!(
            e.check_file_access("sub/dir/a.txt"),
            PolicyDecision::Allow
        ));
    }

    #[test]
    fn file_sensitive_denied() {
        let mut e = engine();
        assert!(matches!(
            e.check_file_access(r".ssh\id_rsa"),
            PolicyDecision::Deny(_)
        ));
        assert!(matches!(
            e.check_file_access("config/.env"),
            PolicyDecision::Deny(_)
        ));
        // 大小写不敏感
        assert!(matches!(
            e.check_file_access("backup/ID_RSA"),
            PolicyDecision::Deny(_)
        ));
    }

    #[test]
    fn network_metadata_denied() {
        let mut e = engine();
        assert!(matches!(
            e.check_network_access("http://169.254.169.254/latest/meta-data/"),
            PolicyDecision::Deny(_)
        ));
        assert!(matches!(
            e.check_network_access("http://metadata.google.internal/computeMetadata/v1/"),
            PolicyDecision::Deny(_)
        ));
    }

    #[test]
    fn network_private_requires_approval() {
        let mut e = engine();
        assert!(matches!(
            e.check_network_access("http://192.168.1.10:8000/api"),
            PolicyDecision::RequireApproval(_)
        ));
        assert!(matches!(
            e.check_network_access("http://127.0.0.1:3721/status"),
            PolicyDecision::RequireApproval(_)
        ));
    }

    #[test]
    fn network_public_allowed() {
        let mut e = engine();
        assert!(matches!(
            e.check_network_access("https://api.openai.com/v1/models"),
            PolicyDecision::Allow
        ));
    }

    #[test]
    fn memory_write_requires_approval_read_allowed() {
        let mut e = engine();
        assert!(matches!(
            e.check_memory_access("read", false),
            PolicyDecision::Allow
        ));
        assert!(matches!(
            e.check_memory_access("write", false),
            PolicyDecision::RequireApproval(_)
        ));
        assert!(matches!(
            e.check_memory_access("write", true),
            PolicyDecision::Allow
        ));
    }

    #[test]
    fn output_private_key_redacted() {
        let mut e = engine();
        let content = "key below\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n";
        let d = e.check_output_release(content);
        match d {
            PolicyDecision::Sanitize { redacted, .. } => {
                assert!(!redacted.contains("PRIVATE KEY"));
                assert!(redacted.contains("[REDACTED:PRIVATE_KEY]"));
            }
            other => panic!("期望 Sanitize，得到 {other:?}"),
        }
    }

    #[test]
    fn output_plain_allowed() {
        let mut e = engine();
        assert!(matches!(
            e.check_output_release("今天天气不错，笔记 42 条"),
            PolicyDecision::Allow
        ));
    }

    #[test]
    fn audit_trail_recorded() {
        let mut e = engine();
        e.check_tool_call("exec_command", false);
        e.check_file_access(r"..\escape.txt");
        e.check_output_release("-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END-----");
        let trail = e.audit_trail();
        assert_eq!(trail.len(), 3);
        assert!(trail[0].action.starts_with("tool_call:"));
        assert!(trail[1].decision.starts_with("deny:"));
        assert!(trail[2].decision.starts_with("sanitize:"));
        // 环形裁剪
        let mut big = engine();
        for i in 0..11_000 {
            big.check_tool_call(&format!("tool{i}"), false);
        }
        assert!(big.audit_trail().len() <= 10_000);
    }

    /// 越界判定边界：同前缀兄弟目录不应误判（对齐 sandbox 组件级策略）。
    #[test]
    fn is_within_boundaries() {
        let root = Path::new(r"C:\workspace");
        assert!(is_within(root, Path::new(r"C:\workspace")));
        assert!(is_within(root, Path::new(r"C:\workspace\a\b.txt")));
        assert!(!is_within(root, Path::new(r"C:\workspace2\a.txt")));
        assert!(!is_within(root, Path::new(r"C:\workspace\..\outside.txt")));
        assert!(!is_within(root, Path::new(r"D:\workspace\a.txt")));
    }
}
