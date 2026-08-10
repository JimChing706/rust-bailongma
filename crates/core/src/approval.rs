//! ApprovalGate —— 高风险工具人工确认门（Phase 1 安全基线）。
//!
//! 计划（bailongma-multiagent-enhancement Phase 1「人工确认机制」）：
//! 高风险工具调用产生 approval 请求 → WS/scene 面板推送（choice 卡片）→
//! 用户选择（允许一次 / 本会话 / 拒绝 / 改参）→ 回传闭环。
//!
//! 实现要点：
//! - **全同步**（`std::sync::mpsc`）：执行端（工具循环）阻塞等待用户抉择；
//!   HTTP 处理器 `submit` 是非阻塞的，异步侧调用无副作用。
//! - **决策复用 PolicyEngine**：`guard_tool_call` 内部走
//!   `PolicyEngine::check_tool_call(tool, session_approved)`——
//!   Allow 直接放行、RequireApproval 才挂起、未知工具 fail-closed 拒绝。
//! - **场景广播解耦**：`on_request` 回调由 API 层注入（server.rs 桥接 SceneStore），
//!   本模块不依赖 scene。
//! - **全局单例**：进程级 `global()`（OnceLock），执行器与 HTTP 处理器共享；
//!   实例 API 保留给单元测试独立构造。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{CoreError, Result};
use crate::policy::{PolicyDecision, PolicyEngine};

/// 单次审批等待上限：用户 120 秒内未抉择 → 按拒绝处理（fail-closed）。
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// 用户抉择（回传值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalChoice {
    /// 仅放行这一次
    AllowOnce,
    /// 本会话内放行同类工具
    AllowSession,
    /// 拒绝
    Deny,
    /// 修改参数后重试（Phase 2 实现，当前按拒绝返回）
    Modify(String),
}

impl ApprovalChoice {
    /// 从 HTTP 表单值解析（未知值视为改参意图）。
    pub fn parse(s: &str) -> Self {
        match s {
            "allow_once" => Self::AllowOnce,
            "allow_session" => Self::AllowSession,
            "deny" => Self::Deny,
            other => Self::Modify(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowSession => "allow_session",
            Self::Deny => "deny",
            Self::Modify(_) => "modify",
        }
    }
}

/// 待确认请求（场景卡片 / 审计用）。
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub id: String,
    pub tool: String,
    pub detail: String,
    pub created_ms: u64,
}

/// `guard_tool_call` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardResult {
    /// 放行执行
    Proceed,
    /// 拒绝（reason 给调用方回显）
    Denied(String),
}

struct GateInner {
    /// 挂起的请求：id → 通道发送端
    pending: HashMap<String, Sender<ApprovalChoice>>,
    /// 决策引擎（含审计轨迹）
    policy: PolicyEngine,
    /// 本会话已获准的工具名
    session_approved: HashSet<String>,
}

/// 人工确认门。
pub struct ApprovalGate {
    inner: Mutex<GateInner>,
    /// 等待超时（测试可缩短）
    timeout: Duration,
    /// 请求创建时的场景回调（API 层注入；None = 未接线 UI，仅超时拒绝）
    on_request: Mutex<Option<Arc<dyn Fn(&ApprovalRequest) + Send + Sync>>>,
    /// id 序号（时间戳 + 自增）
    seq: Mutex<u64>,
}

impl ApprovalGate {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            inner: Mutex::new(GateInner {
                pending: HashMap::new(),
                policy: PolicyEngine::new(workspace_root),
                session_approved: HashSet::new(),
            }),
            timeout: APPROVAL_TIMEOUT,
            on_request: Mutex::new(None),
            seq: Mutex::new(0),
        }
    }

    /// 覆盖超时（测试用）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn new_id(&self) -> String {
        let mut seq = self.seq.lock().unwrap();
        *seq += 1;
        format!("ap_{}_{}", now_ms(), seq)
    }

    /// 工具调用守卫：Allow → Proceed；RequireApproval → 挂起等用户抉择；
    /// Deny / 未知工具 / 超时 → Denied。
    pub fn guard_tool_call(&self, tool: &str, detail: &str) -> Result<GuardResult> {
        let session_ok = self.inner.lock().unwrap().session_approved.contains(tool);
        let decision = {
            let mut inner = self.inner.lock().unwrap();
            inner.policy.check_tool_call(tool, session_ok)
        };
        match decision {
            PolicyDecision::Allow => Ok(GuardResult::Proceed),
            PolicyDecision::Deny(r) => Ok(GuardResult::Denied(r)),
            PolicyDecision::RequireApproval(reason) => {
                let req = ApprovalRequest {
                    id: self.new_id(),
                    tool: tool.to_string(),
                    detail: reason.clone(),
                    created_ms: now_ms(),
                };
                let (tx, rx) = mpsc::channel();
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.pending.insert(req.id.clone(), tx);
                }
                // 场景面板推送（失败不阻塞审批流程，由超时兜底）
                if let Some(cb) = self.on_request.lock().unwrap().as_ref() {
                    cb(&req);
                }
                let outcome = match rx.recv_timeout(self.timeout) {
                    Ok(ApprovalChoice::AllowOnce) => GuardResult::Proceed,
                    Ok(ApprovalChoice::AllowSession) => {
                        self.inner
                            .lock()
                            .unwrap()
                            .session_approved
                            .insert(tool.to_string());
                        GuardResult::Proceed
                    }
                    Ok(ApprovalChoice::Deny) => GuardResult::Denied("用户拒绝该操作".into()),
                    Ok(ApprovalChoice::Modify(_)) => {
                        GuardResult::Denied("改参执行暂未支持（Phase 2 落地）".into())
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        GuardResult::Denied("等待用户确认超时（120s），按拒绝处理".into())
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        GuardResult::Denied("审批通道已断开".into())
                    }
                };
                // 清理挂起条目（无论结果）
                self.inner.lock().unwrap().pending.remove(&req.id);
                Ok(outcome)
            }
            other => Ok(GuardResult::Denied(format!(
                "策略未放行: {}",
                other.summary()
            ))),
        }
    }

    /// 用户抉择回传（HTTP 处理器调用，非阻塞）。成功返回解析后的抉择。
    pub fn submit(&self, id: &str, decision: &str) -> Result<ApprovalChoice> {
        let choice = ApprovalChoice::parse(decision);
        let tx = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .pending
                .remove(id)
                .ok_or_else(|| CoreError::Other(format!("未找到待确认请求或已过期: {id}")))?
        };
        tx.send(choice.clone())
            .map_err(|_| CoreError::Other("审批接收端已关闭".into()))?;
        Ok(choice)
    }

    /// 当前挂起请求数（测试 / 状态接口用）。
    pub fn pending_count(&self) -> usize {
        self.inner.lock().unwrap().pending.len()
    }

    /// 是否本会话已获准该工具。
    pub fn is_session_approved(&self, tool: &str) -> bool {
        self.inner.lock().unwrap().session_approved.contains(tool)
    }

    /// 审计轨迹（委托 PolicyEngine）。
    pub fn audit_trail(&self) -> Vec<crate::policy::AuditEntry> {
        self.inner.lock().unwrap().policy.audit_trail().to_vec()
    }

    /// 注入场景回调（API 层桥接 SceneStore；重复调用覆盖）。
    pub fn set_on_request(&self, cb: Arc<dyn Fn(&ApprovalRequest) + Send + Sync>) {
        *self.on_request.lock().unwrap() = Some(cb);
    }

    /// 进程关闭时：拒绝所有挂起请求（防止执行端永久阻塞）。
    pub fn cancel_all(&self) {
        let inner = self.inner.lock().unwrap();
        for (_, tx) in inner.pending.iter() {
            let _ = tx.send(ApprovalChoice::Deny);
        }
    }
}

// ── 进程级全局单例（执行器与 HTTP 处理器共享） ──

static GLOBAL: OnceLock<Arc<ApprovalGate>> = OnceLock::new();

/// 初始化全局门（幂等；workspace_root 用于内部 PolicyEngine 的文件边界）。
pub fn init_global(workspace_root: PathBuf) -> Arc<ApprovalGate> {
    GLOBAL
        .get_or_init(|| Arc::new(ApprovalGate::new(workspace_root)))
        .clone()
}

/// 取全局门（未初始化时用临时目录兜底，防御性；生产路径应先 `init_global`）。
pub fn global() -> Arc<ApprovalGate> {
    GLOBAL
        .get_or_init(|| Arc::new(ApprovalGate::new(std::env::temp_dir())))
        .clone()
}

/// 给全局门注入场景回调。
pub fn set_global_on_request(cb: Arc<dyn Fn(&ApprovalRequest) + Send + Sync>) {
    global().set_on_request(cb);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn gate() -> Arc<ApprovalGate> {
        Arc::new(
            ApprovalGate::new(PathBuf::from(r"C:\workspace"))
                .with_timeout(Duration::from_millis(150)),
        )
    }

    /// 等一个挂起请求出现，返回其 id。
    fn wait_pending(g: &ApprovalGate) -> String {
        for _ in 0..100 {
            let ids: Vec<String> = {
                let inner = g.inner.lock().unwrap();
                inner.pending.keys().cloned().collect()
            };
            if let Some(id) = ids.first() {
                return id.clone();
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("未出现挂起请求");
    }

    #[test]
    fn low_risk_tool_passes_without_pending() {
        let g = gate();
        assert_eq!(
            g.guard_tool_call("get_timestamp", "").unwrap(),
            GuardResult::Proceed
        );
        assert_eq!(g.pending_count(), 0);
    }

    #[test]
    fn unknown_tool_fail_closed() {
        let g = gate();
        assert!(matches!(
            g.guard_tool_call("rm_rf_system", "").unwrap(),
            GuardResult::Denied(_)
        ));
    }

    #[test]
    fn exec_gate_blocks_then_allow_once() {
        let g = gate();
        let g2 = g.clone();
        let handle = thread::spawn(move || g2.guard_tool_call("exec_command", "dir").unwrap());
        let id = wait_pending(&g);
        assert_eq!(g.pending_count(), 1);
        g.submit(&id, "allow_once").unwrap();
        assert_eq!(handle.join().unwrap(), GuardResult::Proceed);
        assert_eq!(g.pending_count(), 0);
    }

    #[test]
    fn deny_rejects_exec() {
        let g = gate();
        let g2 = g.clone();
        let handle = thread::spawn(move || g2.guard_tool_call("exec_command", "dir").unwrap());
        let id = wait_pending(&g);
        g.submit(&id, "deny").unwrap();
        assert!(matches!(handle.join().unwrap(), GuardResult::Denied(_)));
    }

    #[test]
    fn allow_session_persists_for_same_tool() {
        let g = gate();
        let g2 = g.clone();
        let handle = thread::spawn(move || g2.guard_tool_call("exec_command", "dir").unwrap());
        let id = wait_pending(&g);
        g.submit(&id, "allow_session").unwrap();
        assert_eq!(handle.join().unwrap(), GuardResult::Proceed);
        assert!(g.is_session_approved("exec_command"));
        // 第二次直接放行，不产生挂起
        assert_eq!(
            g.guard_tool_call("exec_command", "dir").unwrap(),
            GuardResult::Proceed
        );
        assert_eq!(g.pending_count(), 0);
    }

    #[test]
    fn timeout_denies_and_cleans_pending() {
        let g = gate(); // 150ms 超时
        let g2 = g.clone();
        let start = std::time::Instant::now();
        let result = g2.guard_tool_call("exec_command", "dir").unwrap();
        assert!(start.elapsed() >= Duration::from_millis(100));
        assert!(matches!(result, GuardResult::Denied(r) if r.contains("超时")));
        assert_eq!(g.pending_count(), 0);
    }

    #[test]
    fn unknown_submit_errors() {
        let g = gate();
        assert!(g.submit("nope", "allow_once").is_err());
    }

    #[test]
    fn choice_parse_roundtrip() {
        assert_eq!(ApprovalChoice::parse("allow_once"), ApprovalChoice::AllowOnce);
        assert_eq!(ApprovalChoice::parse("deny"), ApprovalChoice::Deny);
        assert_eq!(
            ApprovalChoice::parse("anything_else"),
            ApprovalChoice::Modify("anything_else".into())
        );
    }

    #[test]
    fn on_request_callback_fires() {
        let g = gate();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        g.set_on_request(Arc::new(move |req: &ApprovalRequest| {
            seen2.lock().unwrap().push(req.tool.clone());
        }));
        let g2 = g.clone();
        let handle = thread::spawn(move || g2.guard_tool_call("exec_command", "dir").unwrap());
        let id = wait_pending(&g);
        g.submit(&id, "allow_once").unwrap();
        assert_eq!(handle.join().unwrap(), GuardResult::Proceed);
        assert_eq!(seen.lock().unwrap().as_slice(), &["exec_command".to_string()]);
    }
}
