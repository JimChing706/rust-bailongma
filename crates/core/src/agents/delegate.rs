//! Agent 委托工具（对齐 `src/capabilities/executor.js` 的
//! `execDelegateToAgent` / `execGrantAgentDelegation` / `agentDocsHint`）。
//!
//! - `delegate_to_agent`：把任务委托给已注册的本地 AI Agent（claude-code / codex / hermes /
//!   openclaw）。前置条件：用户已通过 `grant_agent_delegation` 授权。CLI 型 Agent 按
//!   `invoke_cmd + invoke_args`（`{prompt}` 占位替换为完整提示词）在超时窗口内执行；
//!   调用失败（非零退出码）时附加文档引导字段（`docs_url` / `docs_search_query`）。
//! - `grant_agent_delegation`：授予 / 撤销委托权限（config 表 `agent_delegation_allowed`）。
//!
//! 执行语义对齐 Node `execCommand`（后台轮询 + 超时强杀），不依赖 sandbox crate
//! （当前为未实现空壳）。DB 访问全部走 `crate::db::repositories::agents`。
//!
//! # 安全（M1.5 注入面加固）
//!
//! CLI 调用使用**参数数组直启**（[`run_command_with_args`]）：`invoke_cmd` 拆分为
//! 「程序 + 前缀参数」，`{prompt}` 作为独立 argv 元素原样传递，**不经任何 shell**。
//! 此前把 `invoke_cmd + invoke_args` 拼成单字符串交给 `cmd.exe /C`（或 `sh -c`）执行，
//! prompt 内的 `& | < > % ^` 等元字符会被 shell 解释为命令分隔符/展开（注入面）；
//! 直启后由操作系统把 argv 原样交给子进程，注入面消除。
//! 超时强杀保留进程树逻辑（Windows `taskkill /T /F`，子进程会继承 stdout/stderr 句柄，
//! 只杀父进程会导致读线程阻塞）。

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::db::models::KnownAgent;
use crate::db::repositories::agents::{
    get_agent_by_id, is_delegation_allowed, revoke_delegation, grant_delegation,
};
use crate::db::Db;
use crate::llm::tools::{boolean_param, number_param, string_param, ToolSchema};

// ─────────────────────────────────────────────────────────────
// 工具 schema（对齐 Node schemas/agents.js 的 delegate_to_agent / grant_agent_delegation）
// ─────────────────────────────────────────────────────────────

/// `delegate_to_agent` 的 OpenAI schema。
pub fn delegate_to_agent_schema() -> ToolSchema {
    ToolSchema::new(
        "delegate_to_agent",
        "把任务委托给已注册的本地 AI Agent（如 claude-code / codex / hermes / openclaw）执行。\
         需先经 grant_agent_delegation 获得用户授权；可用 Agent 列表用 list_known_agents 查看。\
         CLI 型 Agent 会在独立进程中以配置的调用方式执行，超时后强制终止。",
    )
    .required("agent_id", string_param("目标 Agent 的 id（如 \"claude-code\"、\"codex\"）"))
    .required("prompt", string_param("委托给 Agent 的任务提示词（其收到的唯一指令）"))
    .param("context", string_param("可选的附加上下文，会拼在 prompt 之前（Agent 同样可见）"))
    .param("timeout", number_param("超时秒数，默认 60，范围 5-300"))
    .param("verify_hint", string_param(
        "验证提示/断言：给出可客观核对的判据（如输出应出现的标志、文件应存在、期望的退出码等）。\
         执行完成后结果会附带该提示与输出快照，供调用方复核 Agent 声称的完成，不要只信 done",
    ))
}

/// `grant_agent_delegation` 的 OpenAI schema。
pub fn grant_agent_delegation_schema() -> ToolSchema {
    ToolSchema::new(
        "grant_agent_delegation",
        "授予或撤销 Agent 委托权限（持久化）。只有授权后 delegate_to_agent 才能调用外部 Agent；\
         撤销后所有委托立即失效。",
    )
    .required("allowed", boolean_param("true=授予委托权限，false=撤销"))
    .param("note", string_param("备注（仅记录用途）"))
}

// ─────────────────────────────────────────────────────────────
// 内部工具：toolJson 语义、超时收敛、文档引导
// ─────────────────────────────────────────────────────────────

fn tool_json(obj: serde_json::Map<String, Value>) -> String {
    Value::Object(obj).to_string()
}

fn error_result(message: impl Into<String>) -> String {
    tool_json(serde_json::Map::from_iter([(
        "ok".into(),
        Value::Bool(false),
    ), (
        "error".into(),
        Value::String(message.into()),
    )]))
}

/// 超时收敛（对齐 `Math.min(Math.max(Number(timeout) || 60, 5), 300)`）：
/// 0 / NaN / 缺省 → 60；越界收敛到 [5, 300]。
fn clamp_timeout(raw: Option<f64>) -> u64 {
    let n = match raw {
        Some(n) if n.is_finite() && n != 0.0 => n,
        _ => 60.0,
    };
    n.clamp(5.0, 300.0) as u64
}

/// 把 Agent 的文档信息格式化成错误响应里的引导字段（对齐 `agentDocsHint`）。
fn agent_docs_hint(agent: &KnownAgent) -> Option<(String, String)> {
    if let Some(url) = &agent.docs_url {
        return Some((
            "docs_url".into(),
            format!(
                "调用失败。建议先用 web_read(\"{url}\") 查阅 {} 当前版本（{}）的使用文档，确认正确的参数格式后重试。",
                agent.name,
                agent.version.as_deref().unwrap_or("unknown")
            ),
        ));
    }
    if let Some(query) = &agent.docs_search_query {
        return Some((
            "docs_search_query".into(),
            format!(
                "调用失败。建议先用 web_search(\"{query}\") 查找 {} 当前版本（{}）的使用文档，确认正确的调用方式后重试。",
                agent.name,
                agent.version.as_deref().unwrap_or("unknown")
            ),
        ));
    }
    None
}

/// 向错误结果注入文档引导：`docs_url` / `docs_search_query` 存原始值（调用方可直接用），
/// `docs_hint` 存引导文本（含 web_read / web_search 工具建议）。
fn attach_docs_hint(map: &mut serde_json::Map<String, Value>, agent: &KnownAgent) {
    if let Some((key, text)) = agent_docs_hint(agent) {
        let raw = if key == "docs_url" {
            agent.docs_url.clone()
        } else {
            agent.docs_search_query.clone()
        };
        map.insert(key, Value::String(raw.unwrap_or_default()));
        map.insert("docs_hint".into(), Value::String(text));
    }
}

// ─────────────────────────────────────────────────────────────
// CLI 执行（对齐 Node execCommand 的阻塞执行语义）
// ─────────────────────────────────────────────────────────────

/// 以超时窗口执行一个子进程（`program + args` 参数数组直启，不经 shell），
/// 返回 execCommand 形状的 JSON：
/// `{ ok, exit_code, stdout, stderr, [timed_out, error] }`。
///
/// 实现要点：
/// - stdout/stderr 边跑边由读取线程排空，防止 64KB 管道缓冲把子进程堵死；
/// - 主线程 50ms 粒度轮询 `try_wait`，超时先 kill 再 wait 回收；
/// - 启动失败 / 等待失败返回 `{ok:false, exit_code:-1, error}`。
///
/// 超时强杀：Windows 用 taskkill /T /F 杀整棵进程树（子进程会继承 stdout/stderr
/// 句柄，只杀父进程会导致读线程阻塞到子进程自然结束）；其他平台直接 kill（进程组由外壳转发）。
fn spawn_and_wait(program: &str, args: &[String], timeout_sec: u64) -> String {
    let mut builder = Command::new(program);
    builder.args(args);
    builder.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match builder.spawn() {
        Ok(c) => c,
        Err(e) => return error_result(format!("命令启动失败: {e}")),
    };

    let stdout_reader = child
        .stdout
        .take()
        .map(|mut s| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf);
                buf
            })
        });
    let stderr_reader = child
        .stderr
        .take()
        .map(|mut s| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf);
                buf
            })
        });

    let deadline = Instant::now() + Duration::from_secs(timeout_sec.max(1));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_process_tree(&mut child);
                    let out = join_reader(stdout_reader);
                    let err = join_reader(stderr_reader);
                    return json!({
                        "ok": false,
                        "exit_code": -1,
                        "timed_out": true,
                        "stdout": String::from_utf8_lossy(&out),
                        "stderr": String::from_utf8_lossy(&err),
                        "error": format!("命令超时（{timeout_sec}s）"),
                    })
                    .to_string();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return error_result(format!("等待命令失败: {e}"));
            }
        }
    };

    let out = join_reader(stdout_reader);
    let err = join_reader(stderr_reader);
    let exit_code = status.code().unwrap_or(-1);
    json!({
        "ok": exit_code == 0,
        "exit_code": exit_code,
        "stdout": String::from_utf8_lossy(&out),
        "stderr": String::from_utf8_lossy(&err),
    })
    .to_string()
}

fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let pid = child.id();
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .status();
        let _ = child.wait();
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 以超时窗口执行一条 shell 命令（Windows 用 `cmd.exe /C`，其余用 `sh -c`）。
///
/// **保留用于兼容**（内部仍走 [`spawn_and_wait`]）；新代码请用 [`run_command_with_args`]：
/// 参数数组直启不经 shell，避免 prompt 等外部输入中的元字符被 shell 解释（命令注入）。
pub fn run_command_with_timeout(cmd: &str, timeout_sec: u64) -> String {
    let (program, args) = if cfg!(windows) {
        ("cmd.exe", vec!["/C".to_string(), cmd.to_string()])
    } else {
        ("sh", vec!["-c".to_string(), cmd.to_string()])
    };
    spawn_and_wait(program, &args, timeout_sec)
}

/// 以超时窗口直启一个可执行文件（参数数组传递，不经 shell，防命令注入）。
/// 超时强杀与 [`run_command_with_timeout`] 一致（Windows `taskkill /T /F` 进程树）。
pub fn run_command_with_args(program: &str, args: &[String], timeout_sec: u64) -> String {
    spawn_and_wait(program, args, timeout_sec)
}

fn join_reader(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

// ─────────────────────────────────────────────────────────────
// 工具执行器（对齐 execDelegateToAgent / execGrantAgentDelegation）
// ─────────────────────────────────────────────────────────────

/// `delegate_to_agent` 执行体（对齐 `execDelegateToAgent`）：
/// 鉴权 → 查 Agent → 可用性检查 → 组装完整提示词 → CLI/HTTP 调用 → 失败附加文档引导。
pub fn exec_delegate_to_agent(db: &Db, args: &Value) -> String {
    let agent_id = args
        .get("agent_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let prompt = args.get("prompt").and_then(Value::as_str).unwrap_or("");
    let context = args.get("context").and_then(Value::as_str).unwrap_or("");
    let timeout = args.get("timeout").and_then(Value::as_f64);
    let verify_hint = args
        .get("verify_hint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // 1) 鉴权：未授权直接拒绝（对齐 Node 1164-1166）
    match is_delegation_allowed(db) {
        Ok(true) => {}
        Ok(false) => {
            return error_result("尚未获得 Agent 委托权限，请先询问用户并通过 grant_agent_delegation 获取授权。");
        }
        Err(e) => return error_result(format!("读取委托权限失败: {e}")),
    }

    // 2) 查 Agent（对齐 1168-1171）
    let agent = match get_agent_by_id(db, &agent_id) {
        Ok(Some(a)) => a,
        Ok(None) => {
            return error_result(format!(
                "未找到 Agent：{agent_id}。请先用 list_known_agents 查看可用列表。"
            ));
        }
        Err(e) => return error_result(format!("读取 Agent 失败: {e}")),
    };

    // 3) 可用性（对齐 1172-1178）：不可用附文档引导
    if !agent.available {
        let mut map = serde_json::Map::new();
        map.insert("ok".into(), Value::Bool(false));
        map.insert(
            "error".into(),
            Value::String(format!(
                "Agent {} 当前不可用（上次检测：{}）。",
                agent.name, agent.detected_at
            )),
        );
        attach_docs_hint(&mut map, &agent);
        return tool_json(map);
    }

    // 4) 组装完整提示词（对齐 1180-1182）
    let prompt = prompt.trim();
    let full_prompt = if context.trim().is_empty() {
        prompt.to_string()
    } else {
        format!("{}\n\n{}", context.trim(), prompt)
    };

    // 5) 超时收敛（对齐 1184）
    let timeout_sec = clamp_timeout(timeout);

    // 6) CLI 型调用（对齐 1186-1203）：M1.5 注入面加固——参数数组直启，不经 shell。
    if agent.invoke_type.as_deref() == Some("cli") {
        let invoke_cmd = agent.invoke_cmd.clone().unwrap_or_default();
        if invoke_cmd.trim().is_empty() {
            return error_result(format!(
                "Agent {} 缺少 invoke_cmd，无法以 CLI 方式调用。",
                agent.name
            ));
        }
        // invoke_cmd 拆为「程序 + 前缀参数」；{prompt} 占位替换为完整提示词（原样保留换行，
        // 不经 shell 转义）。此前拼成 `cmd.exe /C <字符串>` 执行，prompt 内的
        // `& | < > % ^` 等 cmd 元字符可被解释为命令分隔符/展开（注入面）；直启后
        // prompt 是独立 argv 元素，由操作系统原样交给子进程。
        let mut parts = invoke_cmd.split_whitespace();
        let program = match parts.next() {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => {
                return error_result(format!(
                    "Agent {} 的 invoke_cmd 无效：{invoke_cmd}",
                    agent.name
                ));
            }
        };
        let mut args: Vec<String> = parts.map(|s| s.to_string()).collect();
        for a in &agent.invoke_args {
            if a == "{prompt}" {
                args.push(full_prompt.clone());
            } else {
                args.push(a.clone());
            }
        }

        let result = run_command_with_args(&program, &args, timeout_sec);

        // CLI 调用失败（ok=false 且 exit_code != 0）时注入文档引导（对齐 1191-1203）
        if let Ok(parsed) = serde_json::from_str::<Value>(&result) {
            let failed = parsed.get("ok").and_then(Value::as_bool) == Some(false)
                && parsed.get("exit_code").and_then(Value::as_i64) != Some(0);
            if failed {
                if let Some(obj) = parsed.as_object() {
                    let mut merged = obj.clone();
                    attach_docs_hint(&mut merged, &agent);
                    if !verify_hint.is_empty() {
                        merged.insert("verify_hint".into(), Value::String(verify_hint.clone()));
                        merged.insert("verify_status".into(), Value::String("failed_exit".into()));
                        merged.insert(
                            "verify_note".into(),
                            Value::String("Agent 调用失败（非零退出码）。复核提示仍附上，供调用方判断失败是否符合预期。".into()),
                        );
                    }
                    return tool_json(merged);
                }
            }
            // 命题5 跨 agent 委托维度：语言承诺 != 世界事实。
            // verify_hint 让调用方拿到可核对的判据 + 输出快照，复核后才算完成，
            // 不再只信 Agent 的 done（对齐 matter.rs 的 evidence 非空约束）。
            if !verify_hint.is_empty() {
                if let Some(obj) = parsed.as_object() {
                    let mut merged = obj.clone();
                    let snapshot = parsed
                        .get("stdout")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .take(500)
                        .collect::<String>();
                    merged.insert("verify_hint".into(), Value::String(verify_hint.clone()));
                    merged.insert("verify_status".into(), Value::String("pending_manual_check".into()));
                    merged.insert(
                        "verify_note".into(),
                        Value::String("Agent 声称已完成；请按 verify_hint 对照输出快照复核，不要只信 done".into()),
                    );
                    if !snapshot.is_empty() {
                        merged.insert("output_snapshot".into(), Value::String(snapshot));
                    }
                    return tool_json(merged);
                }
            }
        }
        return result;
    }

    // 7) 其他调用类型暂不支持（对齐 Node 的兜底分支）
    error_result(format!(
        "不支持的 Agent 调用类型：{}（当前仅支持 cli）",
        agent.invoke_type.as_deref().unwrap_or("unknown")
    ))
}

/// `grant_agent_delegation` 执行体（对齐 `execGrantAgentDelegation`）：
/// `allowed=true` 授权 / `false` 撤销，并返回状态文本。
pub fn exec_grant_agent_delegation(db: &Db, args: &Value) -> String {
    let allowed = args.get("allowed").and_then(Value::as_bool).unwrap_or(false);
    let result = if allowed {
        grant_delegation(db)
    } else {
        revoke_delegation(db)
    };
    if let Err(e) = result {
        return error_result(format!("更新委托权限失败: {e}"));
    }

    let granted_now = is_delegation_allowed(db).unwrap_or(false);
    let lines = [
        format!(
            "已记录：用户{}了 Agent 委托授权。",
            if allowed { "同意" } else { "拒绝" }
        ),
        format!(
            "已授权状态：{}。",
            if granted_now { "已授权" } else { "未授权" }
        ),
        "如需修改，请再次调用 grant_agent_delegation。".to_string(),
    ];
    tool_json(serde_json::Map::from_iter([
        ("ok".into(), Value::Bool(true)),
        ("message".into(), Value::String(lines.join("\n"))),
    ]))
}

// ─────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewKnownAgent;
    use crate::db::open_database;
    use crate::db::repositories::agents::{
        grant_delegation, is_delegation_allowed, upsert_agents,
    };

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        // 临时目录保持存活到进程结束：SQLite 连接持有文件句柄，drop 目录会删除
        // 底层文件（Windows 上静默失败）；mem::forget 由 OS 在测试进程退出后清理。
        std::mem::forget(dir);
        open_database(path).unwrap()
    }

    fn agent(id: &str, invoke_type: &str, invoke_cmd: &str, invoke_args: Vec<&str>) -> NewKnownAgent {
        NewKnownAgent {
            id: id.into(),
            name: format!("agent-{id}"),
            description: String::new(),
            available: true,
            version: Some("1.0.0".into()),
            invoke_type: Some(invoke_type.into()),
            invoke_cmd: Some(invoke_cmd.into()),
            invoke_args: invoke_args.iter().map(|s| s.to_string()).collect(),
            notes: String::new(),
            docs_url: Some("https://example.com/agent-docs".into()),
            docs_search_query: None,
            detected_at: None,
        }
    }

    /// docs hint 纯函数用的 KnownAgent（对齐 `agent_docs_hint` 签名）。
    fn known(id: &str, docs_url: Option<String>, docs_search_query: Option<String>) -> KnownAgent {
        KnownAgent {
            id: id.into(),
            name: format!("agent-{id}"),
            description: String::new(),
            available: true,
            version: Some("1.0.0".into()),
            invoke_type: Some("cli".into()),
            invoke_cmd: Some("cmd".into()),
            invoke_args: Vec::new(),
            notes: String::new(),
            docs_url,
            docs_search_query,
            detected_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// 回显替身：Windows 用 node（`-e` 脚本后的参数原样进 `process.argv`，不经 shell，
    /// `& | %` 等元字符全部字面保留）；非 Windows 用 `echo`（/bin/echo 真实可执行文件）。
    fn echo_invoke() -> (&'static str, Vec<&'static str>) {
        if cfg!(windows) {
            (
                "node",
                vec![
                    "-e",
                    "console.log(process.argv.slice(1).join('|'))",
                    "{prompt}",
                ],
            )
        } else {
            ("echo", vec!["{prompt}"])
        }
    }

    // ── 鉴权与查库守卫 ──

    #[test]
    fn delegation_denied_without_grant() {
        let db = test_db();
        let r = exec_delegate_to_agent(&db, &json!({"agent_id": "codex", "prompt": "hi"}));
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("grant_agent_delegation"));
    }

    #[test]
    fn unknown_agent_rejected() {
        let db = test_db();
        grant_delegation(&db).unwrap();
        let r = exec_delegate_to_agent(&db, &json!({"agent_id": "nope", "prompt": "hi"}));
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("未找到 Agent"));
    }

    #[test]
    fn unavailable_agent_returns_docs_hint() {
        let db = test_db();
        grant_delegation(&db).unwrap();
        upsert_agents(
            &db,
            &[NewKnownAgent {
                available: false,
                ..agent("codex", "cli", "codex", vec!["{prompt}"])
            }],
        )
        .unwrap();
        let r = exec_delegate_to_agent(&db, &json!({"agent_id": "codex", "prompt": "hi"}));
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("不可用"));
        assert_eq!(v["docs_url"], "https://example.com/agent-docs");
        assert!(v["docs_hint"].as_str().unwrap().contains("web_read"));
    }

    // ── CLI 调用（真实进程，参数数组直启语义） ──

    #[test]
    fn cli_success_runs_agent_command() {
        let db = test_db();
        grant_delegation(&db).unwrap();
        let (cmd, args) = echo_invoke();
        upsert_agents(&db, &[agent("echo-agent", "cli", cmd, args)]).unwrap();
        let r = exec_delegate_to_agent(
            &db,
            &json!({"agent_id": "echo-agent", "prompt": "hello", "context": "ctx"}),
        );
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "result: {r}");
        assert_eq!(v["exit_code"], 0);
        let stdout = v["stdout"].as_str().unwrap().to_lowercase();
        // context + prompt 都进了完整提示词
        assert!(stdout.contains("hello"), "stdout: {stdout}");
        assert!(stdout.contains("ctx"), "stdout: {stdout}");
    }

    #[test]
    fn cli_success_attaches_verify_hint() {
        let db = test_db();
        grant_delegation(&db).unwrap();
        let (cmd, args) = echo_invoke();
        upsert_agents(&db, &[agent("vh-agent", "cli", cmd, args)]).unwrap();
        let r = exec_delegate_to_agent(
            &db,
            &json!({"agent_id": "vh-agent", "prompt": "hello", "verify_hint": "stdout 应包含 hello"}),
        );
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "result: {r}");
        assert_eq!(v["verify_hint"], "stdout 应包含 hello");
        assert_eq!(v["verify_status"], "pending_manual_check");
        assert!(v["output_snapshot"].as_str().unwrap_or("").contains("hello"), "快照应含输出: {r}");
    }

    #[test]
    fn cli_without_verify_hint_unchanged() {
        let db = test_db();
        grant_delegation(&db).unwrap();
        let (cmd, args) = echo_invoke();
        upsert_agents(&db, &[agent("nvh-agent", "cli", cmd, args)]).unwrap();
        let r = exec_delegate_to_agent(
            &db,
            &json!({"agent_id": "nvh-agent", "prompt": "hello"}),
        );
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "result: {r}");
        assert!(v.get("verify_hint").is_none(), "未提供 verify_hint 时不注入: {r}");
    }

    #[test]
    fn cli_failure_injects_docs_hint() {
        let db = test_db();
        grant_delegation(&db).unwrap();
        // Windows: node 退出码 1；非 Windows: /bin/false（退出码 1）
        let (cmd, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("node", vec!["-e", "process.exit(1)"])
        } else {
            ("false", vec![])
        };
        upsert_agents(&db, &[agent("fail-agent", "cli", cmd, args)]).unwrap();
        let r = exec_delegate_to_agent(
            &db,
            &json!({"agent_id": "fail-agent", "prompt": "boom"}),
        );
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["exit_code"], 1);
        assert_eq!(v["docs_url"], "https://example.com/agent-docs");
        assert!(v["docs_hint"].as_str().unwrap().contains("web_read"));
    }

    /// M1.5 回归 #2：含 shell 元字符的 prompt 必须原样回显、不得被解释执行。
    /// 载荷覆盖 cmd.exe 的命令分隔符（`&`）、管道（`|`）、重定向（`>`）、
    /// 变量展开（`%PATH%`）与引号——直启路径下全部是普通 argv 文本。
    #[test]
    fn cli_metachar_prompt_not_injected() {
        let db = test_db();
        grant_delegation(&db).unwrap();
        let (cmd, args) = echo_invoke();
        upsert_agents(&db, &[agent("meta-agent", "cli", cmd, args)]).unwrap();
        let payload = "hi & echo PWNED_7f3a | more \"quoted\" %PATH%";
        let r = exec_delegate_to_agent(
            &db,
            &json!({"agent_id": "meta-agent", "prompt": payload}),
        );
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true, "result: {r}");
        assert_eq!(v["exit_code"], 0, "result: {r}");
        let stdout = v["stdout"].as_str().unwrap();
        assert!(
            stdout.contains(payload),
            "prompt 必须原样回显（未经 shell 注入执行），stdout: {stdout:?}"
        );
    }

    #[test]
    fn run_command_timeout_kills_process() {
        // 真实超时：1s 内强制终止一个长时间命令（shell 兼容路径）
        let cmd = if cfg!(windows) {
            "ping -n 60 127.0.0.1"
        } else {
            "sleep 60"
        };
        let r = run_command_with_timeout(cmd, 1);
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["timed_out"], true);
        assert!(v["error"].as_str().unwrap().contains("超时"));
    }

    #[test]
    fn run_command_args_timeout_kills_process() {
        // M1.5 回归 #3：直启路径同样必须超时强杀进程树（Windows taskkill /T /F）
        let (program, args): (&str, Vec<String>) = if cfg!(windows) {
            ("ping", vec!["-n".into(), "60".into(), "127.0.0.1".into()])
        } else {
            ("sleep", vec!["60".into()])
        };
        let r = run_command_with_args(program, &args, 1);
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["timed_out"], true);
        assert!(v["error"].as_str().unwrap().contains("超时"));
    }

    #[test]
    fn run_command_captures_output() {
        let cmd = "echo hello-from-bailongma";
        let r = run_command_with_timeout(cmd, 5);
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["stdout"]
            .as_str()
            .unwrap()
            .contains("hello-from-bailongma"));
    }

    // ── 超时收敛 ──

    #[test]
    fn timeout_clamping() {
        assert_eq!(clamp_timeout(None), 60);
        assert_eq!(clamp_timeout(Some(0.0)), 60); // 0 → 默认 60
        assert_eq!(clamp_timeout(Some(f64::NAN)), 60);
        assert_eq!(clamp_timeout(Some(2.0)), 5); // 下限
        assert_eq!(clamp_timeout(Some(999.0)), 300); // 上限
        assert_eq!(clamp_timeout(Some(30.0)), 30);
    }

    // ── grant / revoke 往返 ──

    #[test]
    fn grant_then_revoke_roundtrip() {
        let db = test_db();
        let r = exec_grant_agent_delegation(&db, &json!({"allowed": true}));
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["message"].as_str().unwrap().contains("已授权"));
        assert!(is_delegation_allowed(&db).unwrap());

        let r2 = exec_grant_agent_delegation(&db, &json!({"allowed": false}));
        let v2: Value = serde_json::from_str(&r2).unwrap();
        assert_eq!(v2["ok"], true);
        assert!(v2["message"].as_str().unwrap().contains("未授权"));
        assert!(!is_delegation_allowed(&db).unwrap());
    }

    // ── docs hint 纯函数 ──

    #[test]
    fn docs_hint_prefers_url_then_search_query() {
        let a = known("x", Some("https://example.com/agent-docs".into()), None);
        let (k, text) = agent_docs_hint(&a).unwrap();
        assert_eq!(k, "docs_url");
        assert!(text.contains("web_read") && text.contains("1.0.0"));

        let a2 = known("x", None, Some("claude code docs".into()));
        let (k2, text2) = agent_docs_hint(&a2).unwrap();
        assert_eq!(k2, "docs_search_query");
        assert!(text2.contains("web_search") && !text2.contains("unknown"));

        let a3 = known("x", None, None);
        assert!(agent_docs_hint(&a3).is_none());
    }

    // ── schema 形状 ──

    #[test]
    fn schemas_are_well_formed() {
        let d = delegate_to_agent_schema().to_openai_value();
        assert_eq!(d["function"]["name"], "delegate_to_agent");
        let props = &d["function"]["parameters"]["properties"];
        assert!(props.get("agent_id").is_some());
        assert!(props.get("prompt").is_some());
        assert!(props.get("verify_hint").is_some(), "schema 必须声明 verify_hint");
        let g = grant_agent_delegation_schema().to_openai_value();
        assert_eq!(g["function"]["name"], "grant_agent_delegation");
        assert!(g["function"]["parameters"]["properties"]
            .get("allowed")
            .is_some());
    }

    // ── 自由函数可跨线程（tool_loop 要求 executor Send+Sync 的间接验证） ──

    #[test]
    fn module_fns_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // exec 系列是自由函数（fn 指针无条件 Send+Sync）
        assert_send_sync::<fn(&Db, &Value) -> String>();
    }
}
