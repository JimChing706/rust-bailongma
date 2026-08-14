//! 工具能力模型（Phase 1 安全基线）。
//!
//! 计划（bailongma-multiagent-enhancement Phase 1「工具能力模型」）：
//! - 每个工具声明风险等级（Low/Medium/High/Critical）、side_effect、scopes、
//!   allowed/deny paths、output_policy（方案 §4.2 的 YAML 元数据 → Rust 结构体实现）；
//! - 文件工具携带敏感路径 denylist（.ssh / 密钥 / 配置目录），在执行器 root 约束之外
//!   再做一层纵深防御；
//! - `exec_command` 默认 require approval（Phase 1 人工确认机制的落点）；
//! - 决策逻辑在 Phase 1 的 PolicyEngine（下一步 `crate::policy`）消费；本模块只负责
//!   声明与查询，不碰 DB、不产生副作用。
//!
//! 说明：方案 §4.2 原文是 YAML 元数据。当前 workspace 无 serde_yaml 依赖（且该包已
//! 停止维护），故元数据以 Rust 内建表实现（编译期静态检查 + 零运行时解析成本）；
//! 若后续需要运维侧覆盖，可加 JSON 覆盖文件（复用已有 serde_json，不引新依赖）。

use std::str::FromStr;

/// 工具风险等级（声明顺序即严重度升序，可 `PartialOrd` 直接比较）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RiskLevel {
    /// 纯查询，无副作用
    Low,
    /// 有副作用但影响面小（沙箱内读写）
    Medium,
    /// 副作用大或可能触碰敏感面（删除 / 对外发送）
    High,
    /// 可造成系统级影响（执行命令）
    Critical,
}

impl RiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
            RiskLevel::Critical => "critical",
        }
    }
}

impl FromStr for RiskLevel {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "low" => RiskLevel::Low,
            "medium" => RiskLevel::Medium,
            "high" => RiskLevel::High,
            "critical" => RiskLevel::Critical,
            other => return Err(format!("未知风险等级: {other}")),
        })
    }
}

/// 工具副作用类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SideEffect {
    /// 无副作用（纯查询）
    Pure,
    /// 只读（文件读取 / 记忆检索）
    Read,
    /// 写入（文件写入 / 删除 / 建目录）
    Write,
    /// 网络访问
    Network,
    /// 拉起子进程 / 执行命令
    Spawn,
    /// 对外发送消息
    Send,
    /// 写记忆 / 落库
    MemoryWrite,
}

impl SideEffect {
    pub fn as_str(&self) -> &'static str {
        match self {
            SideEffect::Pure => "pure",
            SideEffect::Read => "read",
            SideEffect::Write => "write",
            SideEffect::Network => "network",
            SideEffect::Spawn => "spawn",
            SideEffect::Send => "send",
            SideEffect::MemoryWrite => "memory_write",
        }
    }
}

/// 工具影响域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// 文件系统
    Files,
    /// 网络
    Network,
    /// 记忆 / 检索
    Memory,
    /// Shell / 子进程
    Shell,
    /// 消息投递
    Messaging,
    /// 提醒 / 调度
    Schedule,
    /// 系统信息
    System,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Files => "files",
            Scope::Network => "network",
            Scope::Memory => "memory",
            Scope::Shell => "shell",
            Scope::Messaging => "messaging",
            Scope::Schedule => "schedule",
            Scope::System => "system",
        }
    }
}

/// 工具输出策略（决定结果如何回喂 LLM / 展示给用户）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputPolicy {
    /// 原样透传
    Passthrough,
    /// 脱敏（截断 / 掩码敏感字段）
    Sanitize,
    /// 摘要后返回
    Summarize,
    /// 拒绝输出
    Deny,
}

impl OutputPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutputPolicy::Passthrough => "passthrough",
            OutputPolicy::Sanitize => "sanitize",
            OutputPolicy::Summarize => "summarize",
            OutputPolicy::Deny => "deny",
        }
    }
}

impl FromStr for OutputPolicy {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "passthrough" => OutputPolicy::Passthrough,
            "sanitize" => OutputPolicy::Sanitize,
            "summarize" => OutputPolicy::Summarize,
            "deny" => OutputPolicy::Deny,
            other => return Err(format!("未知输出策略: {other}")),
        })
    }
}

/// 工具能力声明（方案 §4.2 元数据 → Rust 结构体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCapability {
    /// 工具名（与 `tools/` 注册名一致）
    pub name: &'static str,
    /// 风险等级
    pub risk_level: RiskLevel,
    /// 副作用集合
    pub side_effects: &'static [SideEffect],
    /// 影响域集合
    pub scopes: &'static [Scope],
    /// 路径白名单（None = 执行器 root 约束即白名单，不额外收窄）
    pub allowed_paths: Option<&'static [&'static str]>,
    /// 敏感路径 denylist（组件级匹配，见 [`is_path_denied`]）
    pub deny_paths: &'static [&'static str],
    /// 输出策略
    pub output_policy: OutputPolicy,
    /// 是否默认要求人工确认（exec_command 必须为 true）
    pub requires_approval: bool,
}

impl ToolCapability {
    /// 是否要求人工确认：显式声明 或 风险等级 ≥ High（删除 / 执行命令强制确认）。
    pub fn needs_approval(&self) -> bool {
        self.requires_approval || self.risk_level >= RiskLevel::High
    }
}

/// 敏感路径 denylist（文件工具共用）：密钥 / 凭据 / 配置目录。
/// 注意：这些是组件级子串匹配（fail-closed 方向，宁可误伤不可漏放）。
const SENSITIVE_DENY: &[&str] = &[".ssh", "id_rsa", "id_ed25519", ".env", "credentials", "secret"];

/// 内建能力表（11 个工具，与 `tools/` 注册表一一对应）。
const BUILTIN: &[ToolCapability] = &[
    ToolCapability {
        name: "get_timestamp",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Pure],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "read_file",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::Files],
        allowed_paths: None,
        deny_paths: SENSITIVE_DENY,
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "list_dir",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::Files],
        allowed_paths: None,
        deny_paths: SENSITIVE_DENY,
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "write_file",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::Files],
        allowed_paths: None,
        deny_paths: SENSITIVE_DENY,
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "make_dir",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::Files],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "delete_file",
        risk_level: RiskLevel::High,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::Files],
        allowed_paths: None,
        deny_paths: SENSITIVE_DENY,
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "exec_command",
        risk_level: RiskLevel::Critical,
        side_effects: &[SideEffect::Spawn],
        scopes: &[Scope::Shell],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Sanitize,
        requires_approval: true,
    },
    ToolCapability {
        name: "search_memory",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::Memory],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "send_message",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Send],
        scopes: &[Scope::Messaging],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Sanitize,
        requires_approval: false,
    },
    ToolCapability {
        name: "collect_agents",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "remind",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::Schedule],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "set_reminder",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::MemoryWrite],
        scopes: &[Scope::Schedule],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },

    ToolCapability {
        name: "matter_create",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "matter_query",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },

    ToolCapability {
        name: "delegate_to_agent",
        risk_level: RiskLevel::High,
        side_effects: &[SideEffect::Spawn],
        scopes: &[Scope::Shell],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: true,
    },
    ToolCapability {
        name: "grant_agent_delegation",
        risk_level: RiskLevel::High,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: true,
    },

    // ── 第一批 sys_tools（对齐 Node capabilities：memory/task/system/process）──

    ToolCapability {
        name: "upsert_memory",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::MemoryWrite],
        scopes: &[Scope::Memory],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "probe_memory",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::Memory],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "recall_memory",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::Memory],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "merge_memories",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::MemoryWrite],
        scopes: &[Scope::Memory],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "downgrade_memory",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::MemoryWrite],
        scopes: &[Scope::Memory],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "skip_recognition",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Pure],
        scopes: &[Scope::Memory],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "skip_consolidation",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Pure],
        scopes: &[Scope::Memory],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "set_agent_name",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "set_location",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "set_tick_interval",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "find_tool",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "complete_startup_self_check",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "set_task",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "complete_task",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "update_task_step",
        risk_level: RiskLevel::Medium,
        side_effects: &[SideEffect::Write],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "exec_quick_command",
        risk_level: RiskLevel::Critical,
        side_effects: &[SideEffect::Spawn],
        scopes: &[Scope::Shell],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Sanitize,
        requires_approval: true,
    },
    ToolCapability {
        name: "list_processes",
        risk_level: RiskLevel::Low,
        side_effects: &[SideEffect::Read],
        scopes: &[Scope::System],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: false,
    },
    ToolCapability {
        name: "kill_process",
        risk_level: RiskLevel::High,
        side_effects: &[SideEffect::Spawn],
        scopes: &[Scope::Shell],
        allowed_paths: None,
        deny_paths: &[],
        output_policy: OutputPolicy::Passthrough,
        requires_approval: true,
    },
];

/// 工具信任分层（P2-2）：由能力声明推导，供 PolicyEngine 分层放行。
/// 纯声明推导，fail-closed：未知工具一律 Denied。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrustTier {
    /// 可信：纯查询 / 低风险，无需人工确认
    Trusted,
    /// 需确认：副作用大，需人工确认后放行
    Approval,
    /// 拒绝：未知工具 / 未声明能力
    Denied,
}

impl TrustTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrustTier::Trusted => "trusted",
            TrustTier::Approval => "approval",
            TrustTier::Denied => "denied",
        }
    }
}

/// 调用来源信任等级（P2-2）：同一工具因来源不同获得不同放行策略。
/// System = 系统内部自动化（可放行需确认工具）；User = 终端用户直接指令；
/// Agent = LLM Agent 自主调用（与用户同权，需确认工具一律走人工确认）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallerTrust {
    System,
    User,
    Agent,
}

impl CallerTrust {
    pub fn as_str(&self) -> &'static str {
        match self {
            CallerTrust::System => "system",
            CallerTrust::User => "user",
            CallerTrust::Agent => "agent",
        }
    }
}

/// 由工具名推导信任等级（fail-closed：未知工具 → Denied）。
pub fn trust_tier(name: &str) -> TrustTier {
    match builtin(name) {
        None => TrustTier::Denied,
        Some(cap) if cap.needs_approval() => TrustTier::Approval,
        Some(_) => TrustTier::Trusted,
    }
}

/// 按工具名查内建能力声明。
pub fn builtin(name: &str) -> Option<&'static ToolCapability> {
    BUILTIN.iter().find(|c| c.name == name)
}

/// 全部内建能力声明。
pub fn builtin_all() -> &'static [ToolCapability] {
    BUILTIN
}

/// 敏感路径命中检测：路径归一化（`\` → `/`）后做组件级匹配——
/// 任一组件等于 denylist 项，或以该项开头（覆盖 `id_rsa` → `id_rsa_backup` 这类变体）。
/// fail-closed 方向：宁可误伤，不可漏放。
pub fn is_path_denied(path: &str, deny: &[&str]) -> bool {
    let norm = path.replace('\\', "/");
    deny.iter().any(|d| {
        let d = d.to_lowercase();
        norm.split('/').any(|c| {
            let c = c.to_lowercase();
            c == d || c.starts_with(&d)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 信任分层推导：11 工具分布 + 未知工具 fail-closed。
    #[test]
    fn trust_tier_derivation() {
        // 需确认工具（High/Critical + requires_approval）→ Approval
        for n in ["exec_command", "delete_file"] {
            assert_eq!(trust_tier(n), TrustTier::Approval, "{n}");
        }
        // 纯查询 / 沙箱内读写 / 对外发送（Medium 可控，文档既定）→ Trusted
        for n in ["get_timestamp", "read_file", "write_file", "list_dir", "make_dir",
                  "search_memory", "collect_agents", "remind", "set_reminder", "send_message",
                  "matter_create", "matter_query"] {
            assert_eq!(trust_tier(n), TrustTier::Trusted, "{n}");
        }
        // 未知工具 → Denied
        assert_eq!(trust_tier("format_c:"), TrustTier::Denied);
        assert_eq!(trust_tier(""), TrustTier::Denied);
    }

    #[test]
    fn risk_ordering() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::Medium < RiskLevel::High);
        assert!(RiskLevel::High < RiskLevel::Critical);
    }

    #[test]
    fn parse_roundtrip() {
        for s in ["low", "medium", "high", "critical", "LOW", "Critical"] {
            let rl: RiskLevel = s.parse().unwrap();
            assert_eq!(rl.as_str(), s.to_lowercase());
        }
        assert!("nope".parse::<RiskLevel>().is_err());
        assert_eq!("sanitize".parse::<OutputPolicy>().unwrap(), OutputPolicy::Sanitize);
        assert!("nope".parse::<OutputPolicy>().is_err());
    }

    /// 内建能力表覆盖全部 11 个工具；关键安全约束成立。
    #[test]
    fn builtin_table_sane() {
        let names = [
            "get_timestamp", "read_file", "list_dir", "write_file", "make_dir",
            "delete_file", "exec_command", "search_memory", "send_message",
            "collect_agents", "remind", "matter_create", "matter_query",
        ];
        for n in names {
            assert!(builtin(n).is_some(), "能力表缺工具: {n}");
        }
        // exec_command 必须 Critical + require approval
        let exec = builtin("exec_command").unwrap();
        assert_eq!(exec.risk_level, RiskLevel::Critical);
        assert!(exec.requires_approval);
        assert!(exec.needs_approval());
        // delete_file ≥ High → needs_approval 自动成立
        let del = builtin("delete_file").unwrap();
        assert!(del.needs_approval());
        // 文件工具带敏感 denylist
        let read = builtin("read_file").unwrap();
        assert!(read.deny_paths.contains(&".ssh"));
        // 纯查询工具不要求确认
        assert!(!builtin("get_timestamp").unwrap().needs_approval());
        assert!(!builtin("search_memory").unwrap().needs_approval());
    }

    /// 敏感路径检测：正反例 + Windows 反斜杠归一化。
    #[test]
    fn path_deny_matches() {
        // Windows 风格路径（\ → / 归一化后命中）
        assert!(is_path_denied(r"C:\Users\x\.ssh\id_rsa", &[".ssh"]));
        // 密钥文件名变体
        assert!(is_path_denied("sandbox/id_rsa_backup", &["id_rsa"]));
        // .env 命中
        assert!(is_path_denied("config/.env", &[".env"]));
        // 普通文件不误伤
        assert!(!is_path_denied("sandbox/notes.md", &[".ssh"]));
        assert!(!is_path_denied("sandbox/README.md", &["secret"]));
        // 大小写不敏感
        assert!(is_path_denied("C:/Users/x/.SSH/config", &[".ssh"]));
    }
}
