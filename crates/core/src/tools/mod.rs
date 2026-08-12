//! 工具能力层（R2 真实实现）——替代 M2 阶段仅存在于测试里的 demo 执行器。
//!
//! [`NativeToolExecutor`] 实现 [`crate::llm::tool_loop::ToolExecutor`]，供 LLM
//! 工具循环调用，落地真实工具：
//!
//! | 工具 | 能力 |
//! |---|---|
//! | `get_timestamp` | 当前时间（iso / unix / human） |
//! | `read_file` | 读文件（root 约束 + 字节上限 + 读前查大小） |
//! | `write_file` | 写文件（root 约束 + 大小上限） |
//! | `list_dir` | 列目录（root 约束） |
//! | `make_dir` | 建目录（root 约束） |
//! | `delete_file` | 删文件（root 约束） |
//! | `exec_command` | 执行命令（超时强杀 + 输出截断；可委托 sandbox 子进程） |
//! | `search_memory` | 记忆检索（FTS5 关键词 + 日期窗口，注入 Db） |
//! | `send_message` | 消息投递（注入回调；未接线返回明确错误） |
//! | `collect_agents` | 列出已知 Agent（known_agents 表；tools/extra.rs） |
//! | `remind` | 查询到期提醒（reminders 表；tools/extra.rs） |
//!
//! 安全边界：文件工具全部经 [`resolve_under_root`] 约束在 `root` 内，
//! `..` 越界、绝对路径越界与同名前缀兄弟目录越界一律拒绝
//! （对齐 sandbox crate 的路径策略）。

use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::path::Component;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::agents::delegate::{
    delegate_to_agent_schema, exec_delegate_to_agent, exec_grant_agent_delegation,
    grant_agent_delegation_schema,
};
use crate::approval::{ApprovalGate, GuardResult};
use crate::capability::{builtin, CallerTrust};
use crate::db::Db;
use crate::error::{CoreError, Result};
use crate::llm::tool_loop::ToolExecutor;
use crate::llm::tools::{
    enum_param, integer_param, string_param, ToolSchema,
};

pub mod extra;
pub mod matter_tools;
pub mod validate;

// ─────────────────────────────────────────────────────────────
// 常量
// ─────────────────────────────────────────────────────────────

/// 命令默认超时（毫秒）
const DEFAULT_CMD_TIMEOUT_MS: u64 = 30_000;
/// 命令输出截断（字节）
const MAX_CMD_OUTPUT_BYTES: usize = 64 * 1024;
/// 写文件大小上限（字节）
const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;
/// 读文件默认上限（字节）
const DEFAULT_MAX_READ_BYTES: usize = 256 * 1024;
/// 读文件服务端硬上限（字节）：超过一律拒绝，不整读（内存炸弹防护）
const MAX_READ_HARD_CAP: usize = 64 * 1024 * 1024;
/// 记忆检索默认条数
const DEFAULT_MEMORY_LIMIT: u32 = 10;

// ─────────────────────────────────────────────────────────────
// 工具执行器
// ─────────────────────────────────────────────────────────────

/// send_message 投递回调：`(target_id, content) -> Result<String>`。
pub type SendMessageFn = Arc<dyn Fn(&str, &str) -> Result<String> + Send + Sync>;

/// 真实工具执行器（对齐 Node 版 capabilities/ 各工具的返回形状）。
pub struct NativeToolExecutor {
    /// 文件操作沙箱根（绝对路径）
    pub root: PathBuf,
    /// 记忆检索数据源（None 时 search_memory 返回未接线错误）
    pub db: Option<Db>,
    /// 消息投递回调（None 时 send_message 返回未接线错误）
    pub send_message: Option<SendMessageFn>,
    /// sandbox 子进程路径（Some 时 exec_command 走子进程委托；None 直接执行）
    pub sandbox_bin: Option<PathBuf>,
    /// 人工确认门（Some 时 needs_approval 工具先过审批；None = 不启用，保持旧行为）
    pub approval: Option<Arc<ApprovalGate>>,
    /// 调用来源信任分层（P2-2，Phase 1 修复 D）：System 免确认，User/Agent 需人工确认
    pub caller_trust: CallerTrust,
}

impl NativeToolExecutor {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            db: None,
            send_message: None,
            sandbox_bin: None,
            approval: None,
            caller_trust: CallerTrust::Agent,
        }
    }

    pub fn with_db(mut self, db: Db) -> Self {
        self.db = Some(db);
        self
    }

    pub fn with_send_message(mut self, cb: SendMessageFn) -> Self {
        self.send_message = Some(cb);
        self
    }

    pub fn with_sandbox(mut self, bin: PathBuf) -> Self {
        self.sandbox_bin = Some(bin);
        self
    }

    pub fn with_approval(mut self, gate: Arc<ApprovalGate>) -> Self {
        self.approval = Some(gate);
        self
    }

    pub fn with_caller_trust(mut self, trust: CallerTrust) -> Self {
        self.caller_trust = trust;
        self
    }

    /// Phase 1 修复 B+D：needs_approval 工具分发前统一过 ApprovalGate
    /// （CallerTrust 分层：System 免确认，User/Agent 需人工确认，120s 超时按拒绝）。
    /// Modified 抉择按工具替换主参数（exec_command→command，delete_file→path）。
    fn guard_approval(&self, name: &str, args: &mut Value) -> Result<()> {
        let Some(gate) = self.approval.as_ref() else {
            return Ok(());
        };
        let Some(cap) = builtin(name) else {
            return Ok(());
        };
        if !cap.needs_approval() {
            return Ok(());
        }
        let preview: String = args.to_string().chars().take(160).collect();
        let detail = format!("{name}: {preview}");
        let decision = gate.guard_tool_call_with_caller(name, &detail, self.caller_trust);
        match decision {
            Ok(GuardResult::Proceed) => Ok(()),
            Ok(GuardResult::Modified(new_val)) => {
                let key = match name {
                    "exec_command" => "command",
                    "delete_file" => "path",
                    _ => {
                        return Err(CoreError::Tool(format!(
                            "{name} 不支持改参执行（ApprovalGate modify）"
                        )));
                    }
                };
                if let Some(obj) = args.as_object_mut() {
                    obj.insert(key.into(), Value::String(new_val));
                }
                Ok(())
            }
            Ok(GuardResult::Denied(r)) => Err(CoreError::Tool(format!("{name} 被拒绝: {r}"))),
            Err(e) => Err(CoreError::Other(e.to_string())),
        }
    }

    /// 工具是否已接线（供上层决定是否把工具暴露给 LLM）。
    pub fn is_ready(&self, name: &str) -> bool {
        match name {
            "search_memory" | "collect_agents" | "remind" | "matter_create" | "matter_query"
            | "delegate_to_agent" | "grant_agent_delegation" => self.db.is_some(),
            "send_message" => self.send_message.is_some(),
            _ => true,
        }
    }

    // ── 工具实现 ──

    fn get_timestamp(&self, args: &Value) -> Result<Value> {
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("iso");
        let now = chrono::Local::now();
        let out = match format {
            "unix" => json!({ "unix": now.timestamp() }),
            "human" => json!({ "human": now.format("%Y-%m-%d %H:%M:%S").to_string() }),
            _ => json!({ "iso": now.to_rfc3339() }),
        };
        Ok(json!({ "ok": true, "time": out }))
    }

    fn read_file(&self, args: &Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Tool("read_file 缺 path".into()))?;
        let full = resolve_under_root(&self.root, Path::new(path))?;
        if !full.is_file() {
            return Err(CoreError::Tool(format!("文件不存在: {path}")));
        }
        let max_bytes = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_READ_BYTES as u64) as usize;
        // 读前查大小：超过请求上限或服务端硬上限一律拒绝，不整读（内存炸弹防护）
        let file_len = std::fs::metadata(&full)
            .map_err(|e| CoreError::Tool(format!("读取元数据失败: {e}")))?
            .len();
        if file_len > MAX_READ_HARD_CAP as u64 {
            return Err(CoreError::Tool(format!(
                "文件过大（{} 字节，服务端上限 {} 字节）",
                file_len, MAX_READ_HARD_CAP
            )));
        }
        if file_len > max_bytes as u64 {
            return Err(CoreError::Tool(format!(
                "文件过大（{} 字节，请求上限 {} 字节）",
                file_len, max_bytes
            )));
        }
        let bytes = std::fs::read(&full).map_err(|e| CoreError::Tool(format!("读取失败: {e}")))?;
        let content = String::from_utf8_lossy(&bytes).into_owned();
        Ok(json!({
            "ok": true,
            "path": path,
            "content": content,
            "bytes": bytes.len(),
            "truncated": false,
        }))
    }

    fn write_file(&self, args: &Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Tool("write_file 缺 path".into()))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Tool("write_file 缺 content".into()))?;
        if content.len() > MAX_WRITE_BYTES {
            return Err(CoreError::Tool(format!("写入超限（>{MAX_WRITE_BYTES} 字节）")));
        }
        let full = resolve_under_root(&self.root, Path::new(path))?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CoreError::Tool(format!("创建目录失败: {e}")))?;
        }
        std::fs::write(&full, content.as_bytes())
            .map_err(|e| CoreError::Tool(format!("写入失败: {e}")))?;
        Ok(json!({ "ok": true, "path": path, "bytes": content.len() }))
    }

    fn list_dir(&self, args: &Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(".");
        let full = resolve_under_root(&self.root, Path::new(path))?;
        if !full.is_dir() {
            return Err(CoreError::Tool(format!("目录不存在: {path}")));
        }
        let mut entries: Vec<Value> = Vec::new();
        for entry in std::fs::read_dir(&full)
            .map_err(|e| CoreError::Tool(format!("读取目录失败: {e}")))?
        {
            let entry = entry.map_err(|e| CoreError::Tool(format!("读取目录项失败: {e}")))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = if entry.path().is_dir() { "dir" } else { "file" };
            entries.push(json!({ "name": name, "kind": kind }));
        }
        Ok(json!({ "ok": true, "path": full.display().to_string(), "entries": entries }))
    }

    fn make_dir(&self, args: &Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Tool("make_dir 缺 path".into()))?;
        let full = resolve_under_root(&self.root, Path::new(path))?;
        std::fs::create_dir_all(&full)
            .map_err(|e| CoreError::Tool(format!("创建目录失败: {e}")))?;
        Ok(json!({ "ok": true, "path": path }))
    }

    fn delete_file(&self, args: &Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Tool("delete_file 缺 path".into()))?;
        let full = resolve_under_root(&self.root, Path::new(path))?;
        if !full.exists() {
            return Err(CoreError::Tool(format!("文件不存在: {path}")));
        }
        if full.is_dir() {
            std::fs::remove_dir_all(&full)
                .map_err(|e| CoreError::Tool(format!("删除目录失败: {e}")))?;
        } else {
            std::fs::remove_file(&full)
                .map_err(|e| CoreError::Tool(format!("删除失败: {e}")))?;
        }
        Ok(json!({ "ok": true, "path": path, "deleted": true }))
    }

    fn exec_command(&self, args: &Value) -> Result<Value> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if command.is_empty() {
            return Err(CoreError::Tool("exec_command 缺 command".into()));
        }
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_CMD_TIMEOUT_MS);
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.root.clone());
        let cwd = resolve_under_root(&self.root, &cwd)?;

        // 委托 sandbox 子进程（如果配置了）
        if let Some(bin) = &self.sandbox_bin {
            let r = self.exec_via_sandbox(bin, &command, &cwd, timeout_ms);
            return r;
        }

        // 直接执行（Windows cmd /C，Unix sh -c）
        let (program, shell_args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_string(), command.clone()])
        } else {
            ("sh", vec!["-c".to_string(), command.clone()])
        };
        let start = Instant::now();
        let mut child = Command::new(program)
            .args(&shell_args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CoreError::Tool(format!("命令启动失败: {e}")))?;

        let deadline = start + Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        if cfg!(windows) {
                            let _ = Command::new("taskkill")
                                .args(["/PID", &child.id().to_string(), "/T", "/F"])
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .status();
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(CoreError::Tool(format!("命令执行失败: {e}"))),
            }
        }

        use std::io::Read;
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut o) = child.stdout.take() {
            let mut buf = Vec::new();
            let _ = o.read_to_end(&mut buf);
            stdout = String::from_utf8_lossy(&buf).into_owned();
        }
        if let Some(mut o) = child.stderr.take() {
            let mut buf = Vec::new();
            let _ = o.read_to_end(&mut buf);
            stderr = String::from_utf8_lossy(&buf).into_owned();
        }
        let exit_code = child
            .try_wait()
            .ok()
            .flatten()
            .map(|s| s.code().unwrap_or(-1))
            .unwrap_or(-1);

        Ok(json!({
            "ok": true,
            "stdout": truncate_utf8(&stdout, MAX_CMD_OUTPUT_BYTES).0,
            "stderr": truncate_utf8(&stderr, MAX_CMD_OUTPUT_BYTES).0,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "duration_ms": start.elapsed().as_millis() as u64,
        }))
    }

    /// 经 sandbox 子进程执行（JSON-RPC over stdin/stdout，一行请求一行响应）。
    fn exec_via_sandbox(
        &self,
        bin: &Path,
        command: &str,
        cwd: &Path,
        timeout_ms: u64,
    ) -> Result<Value> {
        use std::io::{BufRead, BufReader, Write};
        use std::process::Stdio as PStdio;

        let mut child = Command::new(bin)
            .arg("--root")
            .arg(&self.root)
            .stdin(PStdio::piped())
            .stdout(PStdio::piped())
            .stderr(PStdio::null())
            .spawn()
            .map_err(|e| CoreError::Tool(format!("sandbox 启动失败: {e}")))?;

        let req = json!({
            "id": 1,
            "method": "exec",
            "params": { "command": command, "cwd": cwd.display().to_string(), "timeout_ms": timeout_ms },
        });
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Tool("sandbox stdin 不可用".into()))?;
        writeln!(stdin, "{req}")
            .map_err(|e| CoreError::Tool(format!("sandbox 请求写入失败: {e}")))?;
        drop(stdin);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Tool("sandbox stdout 不可用".into()))?;
        let mut line = String::new();
        let mut reader = BufReader::new(stdout);
        reader
            .read_line(&mut line)
            .map_err(|e| CoreError::Tool(format!("sandbox 响应读取失败: {e}")))?;
        let _ = child.wait();

        let resp: Value = serde_json::from_str(line.trim())
            .map_err(|e| CoreError::Tool(format!("sandbox 响应解析失败: {e}")))?;
        if resp.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(CoreError::Tool(
                resp.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("sandbox 执行失败")
                    .to_string(),
            ));
        }
        Ok(json!({
            "ok": true,
            "sandbox": true,
            "result": resp.get("result").cloned().unwrap_or(Value::Null),
        }))
    }

    fn search_memory(&self, args: &Value) -> Result<Value> {
        let Some(db) = &self.db else {
            return Err(CoreError::Tool(
                "search_memory 未接线（未注入 Db，当前轮不可用）".into(),
            ));
        };
        let keyword = args
            .get("keyword")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if keyword.is_empty() {
            return Err(CoreError::Tool("search_memory 缺 keyword".into()));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MEMORY_LIMIT as u64)
            .min(50) as u32;
        let memories = crate::db::repositories::memories::search(db, &keyword, limit)
            .map_err(|e| CoreError::Tool(format!("记忆检索失败: {e}")))?;
        let items: Vec<Value> = memories
            .into_iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "event_type": m.event_type,
                    "content": m.content,
                    "timestamp": m.timestamp,
                })
            })
            .collect();
        Ok(json!({ "ok": true, "keyword": keyword, "count": items.len(), "memories": items }))
    }

    fn send_message_impl(&self, args: &Value) -> Result<Value> {
        let Some(cb) = &self.send_message else {
            return Err(CoreError::Tool(
                "send_message 未接线（无投递通道，当前轮不可用）".into(),
            ));
        };
        let target_id = args
            .get("target_id")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Tool("send_message 缺 target_id".into()))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Tool("send_message 缺 content".into()))?;
        let delivered = cb(target_id, content)?;
        Ok(json!({
            "ok": true,
            "tool": "send_message",
            "delivered": true,
            "message_sent": true,
            "target_id": target_id,
            "detail": delivered,
        }))
    }
}

impl ToolExecutor for NativeToolExecutor {
    fn execute(&self, name: &str, args: &Value) -> Result<String> {
        // P2-2: 分发前统一参数 schema 校验（fail-closed：未知参数/类型错/enum 越界一律拒绝）
        validate::validate_args(name, args)?;
        // Phase 1 修复 B+D：needs_approval 工具（delete_file/exec_command/delegate…）分发前
        // 统一过 ApprovalGate（CallerTrust 分层：System 免确认，User/Agent 需人工确认）。
        // 原 exec_command 内部 guard 上移至此，补齐 delete_file 等执行链缺口。
        let mut args = args.clone();
        self.guard_approval(name, &mut args)?;
        // Phase 1 修复 E：全工具统一记录 execute stage（原仅 exec_command 有轨迹）
        let t0 = Instant::now();
        let result: Result<Value> = match name {
            "get_timestamp" => self.get_timestamp(&args),
            "read_file" => self.read_file(&args),
            "write_file" => self.write_file(&args),
            "list_dir" => self.list_dir(&args),
            "make_dir" => self.make_dir(&args),
            "delete_file" => self.delete_file(&args),
            "exec_command" => self.exec_command(&args),
            "search_memory" => self.search_memory(&args),
            "send_message" => self.send_message_impl(&args),
            "collect_agents" => extra::collect_agents_impl(self, &args),
            "remind" => extra::remind_impl(self, &args),
            "matter_create" => matter_tools::matter_create_impl(self, &args),
            "matter_query" => matter_tools::matter_query_impl(self, &args),
            "delegate_to_agent" => {
                let Some(db) = &self.db else {
                    return Err(CoreError::Tool(
                        "delegate_to_agent 未接线（未注入 Db，当前轮不可用）".into(),
                    ));
                };
                let raw = exec_delegate_to_agent(db, &args);
                serde_json::from_str::<Value>(&raw)
                    .map_err(|e| CoreError::Tool(format!("delegate 结果解析失败: {e}")))
            }
            "grant_agent_delegation" => {
                let Some(db) = &self.db else {
                    return Err(CoreError::Tool(
                        "grant_agent_delegation 未接线（未注入 Db，当前轮不可用）".into(),
                    ));
                };
                let raw = exec_grant_agent_delegation(db, &args);
                serde_json::from_str::<Value>(&raw)
                    .map_err(|e| CoreError::Tool(format!("delegate 结果解析失败: {e}")))
            }
            other => Err(CoreError::Tool(format!("未知工具: {other}"))),
        };
        let dur_ms = t0.elapsed().as_millis() as u64;
        match result {
            Ok(v) => {
                crate::trace::global().record(name, "execute", "ok", "", dur_ms, true);
                Ok(v.to_string())
            }
            Err(e) => {
                let msg: String = e.to_string().chars().take(200).collect();
                crate::trace::global().record(name, "execute", "err", &msg, dur_ms, false);
                Err(e)
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 工具 schema 注册表
// ─────────────────────────────────────────────────────────────

/// 全部内置工具 schema（供 LLM 工具循环注册）。
pub fn all_tool_schemas() -> Vec<ToolSchema> {
    let mut tools = vec![
        ToolSchema::new("get_timestamp", "获取当前时间（iso / unix / human 三种格式）")
            .param("format", enum_param("时间格式", &["iso", "unix", "human"])),
        ToolSchema::new("read_file", "读取文件内容（限定在沙箱根目录内；max_bytes 控制读取上限）")
            .required("path", string_param("文件路径（相对沙箱根或绝对路径）"))
            .param("max_bytes", integer_param("最大读取字节数，默认 256KB")),
        ToolSchema::new("write_file", "写入文件（限定在沙箱根目录内，自动创建父目录）")
            .required("path", string_param("文件路径"))
            .required("content", string_param("文件内容")),
        ToolSchema::new("list_dir", "列出目录条目（目录优先，按名称排序）")
            .param("path", string_param("目录路径，默认沙箱根")),
        ToolSchema::new("make_dir", "创建目录（递归创建缺失的父目录）")
            .required("path", string_param("目录路径")),
        ToolSchema::new("delete_file", "删除文件或目录（限定在沙箱根目录内）")
            .required("path", string_param("文件/目录路径")),
        ToolSchema::new("exec_command", "执行 shell 命令（超时强杀；stdout/stderr 各截断 64KB）")
            .required("command", string_param("要执行的命令"))
            .param("timeout_ms", integer_param("超时毫秒，默认 30000"))
            .param("cwd", string_param("工作目录（沙箱根内），默认沙箱根")),
        ToolSchema::new("search_memory", "按关键词检索记忆（FTS5 全文搜索，返回近期相关记忆）")
            .required("keyword", string_param("检索关键词"))
            .param("limit", integer_param("返回条数，默认 10，最大 50")),
        ToolSchema::new("send_message", "向指定对象发送消息（投递最终回复给用户）")
            .required("target_id", string_param("接收方 ID，如 ID:000001"))
            .required("content", string_param("消息正文")),
    ];
    tools.extend(extra::extra_tool_schemas());
    tools.extend(matter_tools::matter_tool_schemas());
    tools.push(delegate_to_agent_schema());
    tools.push(grant_agent_delegation_schema());
    tools
}

// ─────────────────────────────────────────────────────────────
// 路径安全
// ─────────────────────────────────────────────────────────────

/// 将路径解析到 root 内（词法归一化 `.` / `..`，Windows 统一盘符大小写）；
/// 越界返回 Err。与 sandbox crate 的 `normalize_absolute` 策略一致。
fn resolve_under_root(root: &Path, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let normalized = normalize_absolute(&joined);
    if !path_prefix_within(&normalized, root) {
        return Err(CoreError::Tool(format!(
            "路径越界（沙箱根 {}）: {}",
            root.display(),
            path.display()
        )));
    }
    Ok(normalized)
}

#[cfg(windows)]
fn normalize_absolute(path: &Path) -> PathBuf {
    let s = path.to_string_lossy().replace('/', "\\");
    let (prefix, rest) = if s.len() >= 2 && s.as_bytes().get(1) == Some(&b':') {
        (format!("{}\\", s[..2].to_uppercase()), &s[2..])
    } else {
        (String::new(), s.as_str())
    };
    let mut stack: Vec<String> = Vec::new();
    for seg in rest.split('\\') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            seg => stack.push(seg.to_string()),
        }
    }
    let mut out = prefix;
    out.push_str(&stack.join("\\"));
    PathBuf::from(out)
}

#[cfg(not(windows))]
fn normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::RootDir => out.push(std::env::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
            Component::Prefix(p) => out.push(p.as_os_str()),
        }
    }
    out
}

/// 组件级边界判定：`a` 等于 `b` 或在 `b` 的完整组件序列之内。
/// 替代旧实现的字符串 `starts_with` 前缀比较——旧实现会让 `root`
/// 放行 `root2\...`（同名前缀兄弟目录）造成 sandbox 逃逸（第 1 轮审计实锤）。
fn path_prefix_within(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        // Windows 统一小写后做组件级 strip_prefix（组件比较天然带分隔符边界）。
        // 注意：小写字符串必须先绑定到 let 变量——直接内联 `Path::new(&expr)`
        // 会让 to_string_lossy 的临时 Cow 在语句结束即被 drop（E0716 编译错误）。
        let a_lower = a.to_string_lossy().to_lowercase();
        let b_lower = b.to_string_lossy().to_lowercase();
        let a = Path::new(&a_lower);
        let b = Path::new(&b_lower);
        a == b || a.strip_prefix(b).map(|r| !r.as_os_str().is_empty()).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        a == b || a.strip_prefix(b).map(|r| !r.as_os_str().is_empty()).unwrap_or(false)
    }
}

/// UTF-8 安全截断（不切坏字符边界）。
fn truncate_utf8(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn executor(root: &Path) -> NativeToolExecutor {
        NativeToolExecutor::new(root.to_path_buf())
    }

    #[test]
    fn get_timestamp_all_formats() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        for fmt in ["iso", "unix", "human"] {
            let r = ex
                .execute(
                    "get_timestamp",
                    &json!({ "format": fmt }),
                )
                .unwrap();
            let v: Value = serde_json::from_str(&r).unwrap();
            assert_eq!(v["ok"], true, "{fmt}");
            let val = &v["time"][fmt];
            let nonempty = match fmt {
                "unix" => val.as_i64().is_some(),
                _ => !val.as_str().unwrap_or("").is_empty(),
            };
            assert!(nonempty, "format={fmt} resp={r}");
        }
    }

    #[test]
    fn file_roundtrip_and_list() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        ex.execute(
            "write_file",
            &json!({ "path": "notes/hello.txt", "content": "你好工具层" }),
        )
        .unwrap();
        let r = ex
            .execute("read_file", &json!({ "path": "notes/hello.txt" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["content"], "你好工具层");
        let l = ex.execute("list_dir", &json!({ "path": "notes" })).unwrap();
        let lv: Value = serde_json::from_str(&l).unwrap();
        assert_eq!(lv["entries"][0]["name"], "hello.txt");
        // 删除
        let d = ex
            .execute("delete_file", &json!({ "path": "notes/hello.txt" }))
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&d).unwrap()["deleted"], true);
    }

    #[test]
    fn path_traversal_rejected() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        let r = ex.execute("read_file", &json!({ "path": "../outside.txt" }));
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("越界"), "{msg}");
        // 绝对路径越界
        let r2 = ex.execute("read_file", &json!({ "path": "C:\\Windows\\win.ini" }));
        assert!(r2.is_err());
    }

    #[test]
    fn exec_command_runs() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        let r = ex
            .execute("exec_command", &json!({ "command": "echo tool-layer-ok" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap().contains("tool-layer-ok"));
    }

    #[test]
    fn exec_command_timeout_kills() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        let r = ex
            .execute(
                "exec_command",
                &json!({ "command": "ping -n 30 127.0.0.1", "timeout_ms": 300 }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["timed_out"], true, "{v}");
    }

    #[test]
    fn search_memory_requires_db() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        let r = ex.execute("search_memory", &json!({ "keyword": "咖啡" }));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("未接线"));
    }

    #[test]
    fn search_memory_with_db() {
        let dir = tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        crate::db::repositories::memories::insert_simple(&db, "fact", "用户喜欢冷萃咖啡").unwrap();
        let ex = executor(dir.path()).with_db(db);
        let r = ex
            .execute("search_memory", &json!({ "keyword": "咖啡" }))
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["count"], 1, "{v}");
        assert!(v["memories"][0]["content"].as_str().unwrap().contains("咖啡"));
    }

    #[test]
    fn send_message_requires_callback() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        let r = ex.execute(
            "send_message",
            &json!({ "target_id": "ID:000001", "content": "hi" }),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("未接线"));
    }

    #[test]
    fn send_message_with_callback() {
        let dir = tempdir().unwrap();
        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sent2 = sent.clone();
        let ex = executor(dir.path()).with_send_message(Arc::new(
            move |target: &str, content: &str| {
                sent2.lock().unwrap().push(format!("{target}:{content}"));
                Ok("delivered".into())
            },
        ));
        let r = ex
            .execute(
                "send_message",
                &json!({ "target_id": "ID:000001", "content": "你好" }),
            )
            .unwrap();
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["delivered"], true);
        assert_eq!(v["message_sent"], true);
        assert_eq!(sent.lock().unwrap()[0], "ID:000001:你好");
    }

    #[test]
    fn unknown_tool_errors() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        assert!(ex.execute("rm_rf", &json!({})).is_err());
    }

    #[test]
    fn schemas_cover_all_tools() {
        let schemas = all_tool_schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        for tool in [
            "get_timestamp",
            "read_file",
            "write_file",
            "list_dir",
            "make_dir",
            "delete_file",
            "exec_command",
            "search_memory",
            "send_message",
            "collect_agents",
            "remind",
        ] {
            assert!(names.contains(&tool), "缺少 schema: {tool}");
        }
        // 每个 schema 都能转 OpenAI tools 格式
        for s in &schemas {
            let v = s.to_openai_value();
            assert_eq!(v["type"], "function");
            assert_eq!(v["function"]["name"], s.name);
        }
    }

    #[test]
    fn readiness_gates_optional_tools() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        assert!(!ex.is_ready("search_memory"));
        assert!(!ex.is_ready("send_message"));
        assert!(!ex.is_ready("collect_agents"));
        assert!(!ex.is_ready("remind"));
        assert!(ex.is_ready("exec_command"));
        let ex2 = executor(dir.path())
            .with_db(Db::open(dir.path().join("t2.db")).unwrap())
            .with_send_message(Arc::new(|_, _| Ok("ok".into())));
        assert!(ex2.is_ready("search_memory"));
        assert!(ex2.is_ready("send_message"));
        assert!(ex2.is_ready("collect_agents"));
        assert!(ex2.is_ready("remind"));
    }

    // ── 第 1 轮审计修复的回归测试（红转绿）──

    #[test]
    fn prefix_collision_sibling_rejected() {
        // root=…/root 时，`..\root2\secret.txt` 落在 root 外同名前缀兄弟目录
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let root2 = dir.path().join("root2");
        std::fs::create_dir_all(&root2).unwrap();
        std::fs::write(root2.join("secret.txt"), "TOP-SECRET-OUTSIDE-ROOT").unwrap();

        let ex = executor(&root);
        let r = ex.execute("read_file", &json!({ "path": "..\\root2\\secret.txt" }));
        assert!(r.is_err(), "前缀碰撞穿越应被拒: {r:?}");
    }

    #[test]
    fn read_file_rejects_oversized_before_reading() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("big.bin"), vec![0u8; 1024 * 1024]).unwrap();
        let ex = executor(dir.path());
        let r = ex.execute(
            "read_file",
            &json!({ "path": "big.bin", "max_bytes": 1024 }),
        );
        assert!(r.is_err(), "超大文件应拒绝而非整读截断: {r:?}");
    }

    // ── Phase 1 修复 B：delete_file 挂 ApprovalGate（原仅 exec_command 挂门）──

    #[test]
    fn delete_file_requires_approval_when_gate_present() {
        use crate::approval::ApprovalGate;
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("victim.txt"), "x").unwrap();
        let gate = Arc::new(
            ApprovalGate::new(dir.path().to_path_buf())
                .with_timeout(Duration::from_millis(300)),
        );
        let ex = executor(dir.path()).with_approval(gate.clone());
        let handle = std::thread::spawn(move || {
            ex.execute("delete_file", &json!({ "path": "victim.txt" }))
        });
        let mut id = None;
        for _ in 0..100 {
            if let Some(first) = gate.pending_ids().first() {
                id = Some(first.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let id = id.expect("delete_file 应产生审批挂起");
        gate.submit(&id, "allow_once").unwrap();
        let r = handle.join().unwrap();
        assert!(r.is_ok(), "allow_once 后应删除成功: {r:?}");
        assert!(!dir.path().join("victim.txt").exists(), "文件应已被删除");
    }

    #[test]
    fn delete_file_denied_on_timeout_when_gate_present() {
        use crate::approval::ApprovalGate;
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("victim.txt"), "x").unwrap();
        let gate = Arc::new(
            ApprovalGate::new(dir.path().to_path_buf())
                .with_timeout(Duration::from_millis(150)),
        );
        let ex = executor(dir.path()).with_approval(gate.clone());
        let r = ex.execute("delete_file", &json!({ "path": "victim.txt" }));
        assert!(r.is_err(), "无人确认应拒绝: {r:?}");
        assert!(r.unwrap_err().to_string().contains("拒绝"));
        assert!(dir.path().join("victim.txt").exists(), "文件不应被删");
        assert_eq!(gate.pending_count(), 0, "挂起请求应已清理");
    }

    // ── Phase 1 修复 D：CallerTrust 分层接线（System 免确认）──

    #[test]
    fn system_caller_bypasses_approval_for_high_risk_tools() {
        use crate::approval::ApprovalGate;
        use crate::capability::CallerTrust;
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("victim.txt"), "x").unwrap();
        let gate = Arc::new(ApprovalGate::new(dir.path().to_path_buf()));
        let ex = executor(dir.path())
            .with_approval(gate.clone())
            .with_caller_trust(CallerTrust::System);
        let r = ex.execute("delete_file", &json!({ "path": "victim.txt" }));
        assert!(r.is_ok(), "System 来源应免确认: {r:?}");
        assert_eq!(gate.pending_count(), 0);
        assert!(!dir.path().join("victim.txt").exists());
    }

    // ── Phase 1 修复 C：delegate 工具注册进工具循环 ──

    #[test]
    fn delegate_tools_registered_and_gated() {
        let schemas = all_tool_schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"delegate_to_agent"), "schema 缺失 delegate_to_agent");
        assert!(names.contains(&"grant_agent_delegation"), "schema 缺失 grant_agent_delegation");
        // 无 db：未接线错误（不执行真实进程）
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        assert!(!ex.is_ready("delegate_to_agent"));
        let r = ex.execute(
            "delegate_to_agent",
            &json!({ "agent_id": "codex", "prompt": "hi" }),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("未接线"));
        // 能力声明：High + 需确认（与文档一致）
        assert!(builtin("delegate_to_agent").unwrap().needs_approval());
        assert!(builtin("grant_agent_delegation").unwrap().needs_approval());
        assert_eq!(crate::capability::trust_tier("delegate_to_agent"), crate::capability::TrustTier::Approval);
    }

    // ── Phase 1 修复 E：execute stage 全工具统一记录 ──

    #[test]
    fn execute_stage_traced_for_all_tools() {
        let dir = tempdir().unwrap();
        let ex = executor(dir.path());
        let _ = ex.execute("get_timestamp", &json!({ "format": "iso" }));
        let _ = ex.execute("list_dir", &json!({ "path": "." }));
        let recent = crate::trace::global().recent(20, "");
        let tools_with_execute: Vec<&str> = recent
            .iter()
            .filter(|t| t.stage == "execute" && t.decision == "ok")
            .map(|t| t.tool.as_str())
            .collect();
        assert!(tools_with_execute.contains(&"get_timestamp"), "recent={recent:?}");
        assert!(tools_with_execute.contains(&"list_dir"), "recent={recent:?}");
        // 失败也记录 err
        let _ = ex.execute("read_file", &json!({ "path": "no-such.txt" }));
        let recent = crate::trace::global().recent(5, "read_file");
        let errs: Vec<&str> = recent
            .iter()
            .filter(|t| t.stage == "execute" && t.decision == "err")
            .map(|t| t.tool.as_str())
            .collect();
        assert!(!errs.is_empty(), "read_file 失败应记录 execute/err");
    }
}
