//! ToolTrace —— 工具调用全链路可观测轨迹（Phase 2）。
//!
//! 覆盖三个 stage：
//! - `guard`：策略判定（allow / deny / require_approval）
//! - `approval`：人工确认结果（approved / modify / denied / timeout）
//! - `execute`：实际执行结果（ok / timeout / err + 耗时）
//!
//! 内存环形缓冲（cap 默认 10000），进程级单例 `global()`；
//! 工具调用台账落库由 `llm_tool_calls`（工具循环层）承接，本层专注决策链路。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const TRACE_CAP: usize = 10_000;

/// 单条轨迹记录。
#[derive(Debug, Clone)]
pub struct ToolTrace {
    pub id: u64,
    pub ts_ms: u64,
    pub tool: String,
    /// guard / approval / execute
    pub stage: String,
    /// allow / deny / require_approval / approved / modify / denied / timeout / ok / err
    pub decision: String,
    pub detail: String,
    pub duration_ms: u64,
    pub ok: bool,
}

pub struct TraceStore {
    inner: Mutex<TraceInner>,
}

struct TraceInner {
    buf: VecDeque<ToolTrace>,
    cap: usize,
    seq: u64,
}

impl TraceStore {
    pub fn new() -> Self {
        Self::with_cap(TRACE_CAP)
    }

    pub fn with_cap(cap: usize) -> Self {
        Self {
            inner: Mutex::new(TraceInner {
                buf: VecDeque::new(),
                cap: cap.max(1),
                seq: 0,
            }),
        }
    }

    /// 记录一条轨迹，返回 id。
    pub fn record(
        &self,
        tool: &str,
        stage: &str,
        decision: &str,
        detail: &str,
        duration_ms: u64,
        ok: bool,
    ) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        let id = inner.seq;
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        inner.buf.push_back(ToolTrace {
            id,
            ts_ms,
            tool: tool.to_string(),
            stage: stage.to_string(),
            decision: decision.to_string(),
            detail: detail.to_string(),
            duration_ms,
            ok,
        });
        while inner.buf.len() > inner.cap {
            inner.buf.pop_front();
        }
        id
    }

    /// 最近 N 条（时间倒序）；tool 非空时过滤。
    pub fn recent(&self, limit: usize, tool: &str) -> Vec<ToolTrace> {
        let inner = self.inner.lock().unwrap();
        inner
            .buf
            .iter()
            .rev()
            .filter(|t| tool.is_empty() || t.tool == tool)
            .take(limit.max(1))
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().buf.len()
    }
}

impl Default for TraceStore {
    fn default() -> Self {
        Self::new()
    }
}

// ── 进程级全局单例 ──

static GLOBAL: OnceLock<TraceStore> = OnceLock::new();

pub fn global() -> &'static TraceStore {
    GLOBAL.get_or_init(TraceStore::new)
}

// ── 测试 ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_recent_roundtrip() {
        let s = TraceStore::new();
        s.record("exec_command", "guard", "require_approval", "dir", 0, true);
        s.record("exec_command", "approval", "approved", "", 3, true);
        s.record("exec_command", "execute", "ok", "", 42, true);
        assert_eq!(s.len(), 3);
        let all = s.recent(10, "");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].stage, "execute");
        assert_eq!(all[0].decision, "ok");
    }

    #[test]
    fn recent_filters_by_tool_and_limits() {
        let s = TraceStore::new();
        s.record("exec_command", "guard", "allow", "", 0, true);
        s.record("read_file", "guard", "allow", "", 0, true);
        s.record("write_file", "guard", "allow", "", 0, true);
        let cmd = s.recent(10, "exec_command");
        assert_eq!(cmd.len(), 1);
        let all = s.recent(2, "");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn cap_trims_oldest() {
        let s = TraceStore::with_cap(3);
        for i in 0..5 {
            s.record("t", "guard", &format!("d{}", i), "", 0, true);
        }
        assert_eq!(s.len(), 3);
        let all = s.recent(10, "");
        assert_eq!(all[0].decision, "d4");
        assert_eq!(all[2].decision, "d2");
    }
}
