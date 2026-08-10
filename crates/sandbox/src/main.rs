//! 工具执行沙箱子进程（R1 真实实现）。
//!
//! 替代 M5 占位：独立进程，通过 stdin/stdout 逐行 JSON-RPC 通信，
//! 主进程（bailongma-core 工具层）将受限能力委托给本进程执行。
//!
//! 协议（每行一个 JSON 对象）：
//! ```text
//! 请求:  {"id":1,"method":"exec","params":{"command":"echo hi","cwd":".","timeout_ms":10000}}
//!         {"id":2,"method":"read_file","params":{"path":"a.txt","max_bytes":65536}}
//!         {"id":3,"method":"write_file","params":{"path":"a.txt","content":"..."}}
//!         {"id":4,"method":"list_dir","params":{"path":"."}}
//! 响应:  {"id":1,"ok":true,"result":{...}}
//!         {"id":1,"ok":false,"error":"..."}
//! ```
//!
//! 能力约束（对齐 RUST-ROADMAP.md §6.1）：
//! - 路径约束：所有文件操作限定在 `root`（绝对路径，默认进程 cwd）内，防 `..` 穿越
//! - 命令执行：超时强杀（kill 后 wait），stdout/stderr 各截断 64KB
//! - 环境清理：执行子命令时继承最小环境（PATH/TMP 等），不注入父进程敏感变量
//! - 一次一请求：串行处理，天然防并发资源争抢

use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// stdout/stderr 截断上限（字节）
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// 默认命令超时（毫秒）
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// 写文件大小上限（字节）
const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;
/// read_file 默认最大读取字节
const DEFAULT_MAX_READ_BYTES: usize = 256 * 1024;

// ─────────────────────────────────────────────────────────────
// 沙箱执行器
// ─────────────────────────────────────────────────────────────

pub struct Sandbox {
    /// 路径约束根（绝对路径）；所有文件操作必须落在其内
    pub root: PathBuf,
    /// 命令前缀白名单（空 = 全部允许）；逐条 `starts_with` 前缀匹配
    pub allow_commands: Vec<String>,
}

impl Sandbox {
    pub fn new(root: PathBuf, allow_commands: Vec<String>) -> Self {
        Self {
            root: normalize_absolute(&root),
            allow_commands,
        }
    }

    /// 处理一条请求，返回响应 JSON（永不 panic，错误转 JSON）。
    pub fn handle(&self, req: &Value) -> Value {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let outcome = match method.as_str() {
            "exec" => self.exec(&params),
            "read_file" => self.read_file(&params),
            "write_file" => self.write_file(&params),
            "list_dir" => self.list_dir(&params),
            "ping" => Ok(json!({ "pong": true, "root": self.root.display().to_string() })),
            other => Err(format!("未知方法: {other}")),
        };

        match outcome {
            Ok(result) => json!({ "id": id, "ok": true, "result": result }),
            Err(e) => json!({ "id": id, "ok": false, "error": e }),
        }
    }

    // ── exec ──

    fn exec(&self, params: &Value) -> Result<Value, String> {
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if command.is_empty() {
            return Err("参数非法: command 为空".into());
        }
        if !self.allow_commands.is_empty() {
            let allowed = self
                .allow_commands
                .iter()
                .any(|prefix| command.starts_with(prefix.as_str()));
            if !allowed {
                return Err(format!(
                    "命令不在白名单内: {}",
                    command.chars().take(80).collect::<String>()
                ));
            }
        }

        let timeout_ms = params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.root.clone());
        // cwd 也约束在 root 内
        let cwd = self.resolve_in_root(&cwd)?;

        // Windows: cmd /C；Unix: sh -c
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_string(), command.clone()])
        } else {
            ("sh", vec!["-c".to_string(), command.clone()])
        };

        let start = Instant::now();
        let mut child = Command::new(program)
            .args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 最小环境：只带 PATH / TEMP / TMP，剔除敏感变量
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TEMP", std::env::temp_dir().display().to_string())
            .env("TMP", std::env::temp_dir().display().to_string())
            .env_remove("BAILONGMA_API_TOKEN")
            .env_remove("OPENAI_API_KEY")
            .env_remove("DEEPSEEK_API_KEY")
            .spawn()
            .map_err(|e| format!("命令启动失败: {e}"))?;

        let deadline = start + Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        // 强杀：Windows 用 taskkill /T 杀进程树，Unix 直接 kill
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
                Err(e) => return Err(format!("命令执行失败: {e}")),
            }
        }

        let stdout = child
            .stdout
            .take()
            .map(|mut o| {
                let mut buf = Vec::new();
                let _ = o.read_to_end(&mut buf);
                buf
            })
            .unwrap_or_default();
        let stderr = child
            .stderr
            .take()
            .map(|mut o| {
                let mut buf = Vec::new();
                let _ = o.read_to_end(&mut buf);
                buf
            })
            .unwrap_or_default();

        let stdout_str = String::from_utf8_lossy(&stdout);
        let stderr_str = String::from_utf8_lossy(&stderr);
        let (stdout_cut, truncated_stdout) = truncate_bytes(&stdout_str, MAX_OUTPUT_BYTES);
        let (stderr_cut, truncated_stderr) = truncate_bytes(&stderr_str, MAX_OUTPUT_BYTES);
        let exit_code = child
            .try_wait()
            .ok()
            .flatten()
            .map(|s| s.code().unwrap_or(-1))
            .unwrap_or(-1);

        Ok(json!({
            "stdout": stdout_cut,
            "stderr": stderr_cut,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "truncated": truncated_stdout || truncated_stderr,
            "duration_ms": start.elapsed().as_millis() as u64,
        }))
    }

    // ── read_file ──

    fn read_file(&self, params: &Value) -> Result<Value, String> {
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "参数非法: path 缺失".to_string())?;
        let full = self.resolve_in_root(Path::new(path))?;
        if !full.is_file() {
            return Err(format!("文件不存在: {path}"));
        }
        let max_bytes = params
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_READ_BYTES as u64) as usize;
        let bytes = std::fs::read(&full).map_err(|e| format!("读取失败: {e}"))?;
        let truncated = bytes.len() > max_bytes;
        let content = if truncated {
            String::from_utf8_lossy(&bytes[..max_bytes]).into_owned()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        Ok(json!({
            "content": content,
            "bytes": bytes.len(),
            "truncated": truncated,
        }))
    }

    // ── write_file ──

    fn write_file(&self, params: &Value) -> Result<Value, String> {
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "参数非法: path 缺失".to_string())?;
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("");
        if content.len() > MAX_WRITE_BYTES {
            return Err(format!("写入超限（>{MAX_WRITE_BYTES} 字节）"));
        }
        let full = self.resolve_in_root(Path::new(path))?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
        }
        std::fs::write(&full, content.as_bytes()).map_err(|e| format!("写入失败: {e}"))?;
        Ok(json!({ "path": path, "bytes": content.len() }))
    }

    // ── list_dir ──

    fn list_dir(&self, params: &Value) -> Result<Value, String> {
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(".");
        let full = self.resolve_in_root(Path::new(path))?;
        if !full.is_dir() {
            return Err(format!("目录不存在: {path}"));
        }
        let mut entries: Vec<Value> = Vec::new();
        for entry in std::fs::read_dir(&full).map_err(|e| format!("读取目录失败: {e}"))? {
            let entry = entry.map_err(|e| format!("读取目录项失败: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = if entry.path().is_dir() { "dir" } else { "file" };
            entries.push(json!({ "name": name, "kind": kind }));
        }
        entries.sort_by(|a, b| {
            let ka = (a["kind"].as_str().unwrap_or("") == "dir") as u8;
            let kb = (b["kind"].as_str().unwrap_or("") == "dir") as u8;
            kb.cmp(&ka)
                .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
        });
        Ok(json!({ "path": full.display().to_string(), "entries": entries }))
    }

    // ── 路径约束 ──

    /// 将相对/绝对路径解析为 root 内绝对路径；越界返回 Err。
    fn resolve_in_root(&self, path: &Path) -> Result<PathBuf, String> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let normalized = normalize_absolute(&joined);
        // Windows 下比较统一小写（盘符/大小写不敏感），Unix 直接比较
        if !same_path_prefix(&normalized, &self.root) {
            return Err(format!(
                "路径越界（沙箱根 {}）: {}",
                self.root.display(),
                path.display()
            ));
        }
        Ok(normalized)
    }
}

/// 规范化绝对路径（解析 `.` / `..`；Windows 统一盘符大小写与分隔符）。
/// 注意：prefix 必须取 `s[..2]`（含冒号，如 `C:`），只取 `s[..1]` 会产出
/// 缺冒号的 `C\Users\...` —— Windows 将其解析为当前盘相对路径，酿成 os error 267。
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
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
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

/// 路径前缀比较（Windows 大小写不敏感）。
fn same_path_prefix(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let a = a.to_string_lossy().to_lowercase();
        let b = b.to_string_lossy().to_lowercase();
        a.starts_with(&b)
    }
    #[cfg(not(windows))]
    {
        a.starts_with(b)
    }
}

/// 截断到 max 字节（UTF-8 安全：不切坏字符边界），返回 (文本, 是否截断)。
fn truncate_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

// ─────────────────────────────────────────────────────────────
// stdin/stdout 主循环
// ─────────────────────────────────────────────────────────────

fn run_io_loop(sandbox: &Sandbox) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({ "id": null, "ok": false, "error": format!("读取请求失败: {e}") })
                );
                let _ = stdout.flush();
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({ "id": null, "ok": false, "error": format!("请求不是合法 JSON: {e}") })
                );
                let _ = stdout.flush();
                continue;
            }
        };
        let resp = sandbox.handle(&req);
        let _ = writeln!(stdout, "{resp}");
        let _ = stdout.flush();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--self-test") {
        self_test();
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "bailongma-sandbox — 工具执行沙箱子进程 (JSON-RPC over stdin/stdout)\n\
             用法: bailongma-sandbox [--root <dir>] [--allow <prefix>[,<prefix>...]]\n\
             \x20    bailongma-sandbox --self-test   # 内置冒烟测试\n\
             协议: 每行一个 JSON 请求 {{id, method, params}} → 每行一个 JSON 响应"
        );
        return;
    }

    let mut root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut allow: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                if let Some(v) = args.get(i + 1) {
                    root = PathBuf::from(v);
                    i += 2;
                    continue;
                }
            }
            "--allow" => {
                if let Some(v) = args.get(i + 1) {
                    allow = v
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    i += 2;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    // root 必须绝对路径（resolve 时相对父进程 cwd）
    if root.is_relative() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        root = cwd.join(root);
    }
    root = normalize_absolute(&root);

    let sandbox = Sandbox::new(root, allow);
    run_io_loop(&sandbox);
}

// ─────────────────────────────────────────────────────────────
// 自测
// ─────────────────────────────────────────────────────────────

fn self_test() {
    let dir =
        std::env::temp_dir().join(format!("bailongma-sandbox-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let sb = Sandbox::new(dir.clone(), vec![]);

    // 1. ping
    let ping = sb.handle(&json!({ "id": 1, "method": "ping" }));
    assert_eq!(ping["ok"], true, "ping: {ping}");

    // 2. write + read 往返
    let w = sb.handle(&json!({ "id": 2, "method": "write_file", "params": { "path": "a.txt", "content": "你好 sandbox" } }));
    assert_eq!(w["ok"], true, "write: {w}");
    let r = sb.handle(&json!({ "id": 3, "method": "read_file", "params": { "path": "a.txt" } }));
    assert_eq!(r["ok"], true, "read: {r}");
    assert_eq!(r["result"]["content"], "你好 sandbox", "内容往返: {r}");

    // 3. list_dir 含刚写的文件
    let l = sb.handle(&json!({ "id": 4, "method": "list_dir", "params": { "path": "." } }));
    assert_eq!(l["ok"], true, "list: {l}");
    let names: Vec<&str> = l["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(names.contains(&"a.txt"), "entries 含 a.txt: {names:?}");

    // 4. exec 真实命令
    let e = sb.handle(&json!({ "id": 5, "method": "exec", "params": { "command": "echo hi" } }));
    assert_eq!(e["ok"], true, "exec: {e}");
    assert_eq!(e["result"]["exit_code"], 0, "exit 0: {e}");
    assert!(
        e["result"]["stdout"].as_str().unwrap_or("").contains("hi"),
        "stdout: {e}"
    );

    // 5. 路径穿越被拒
    let escape = sb.handle(&json!({ "id": 6, "method": "read_file", "params": { "path": "../secret.txt" } }));
    assert_eq!(escape["ok"], false, "穿越应被拒: {escape}");
    assert!(
        escape["error"].as_str().unwrap_or("").contains("越界"),
        "错误信息: {escape}"
    );

    // 6. 绝对路径越界被拒
    let abs = sb.handle(&json!({ "id": 7, "method": "read_file", "params": { "path": "C:\\Windows\\win.ini" } }));
    assert_eq!(abs["ok"], false, "绝对路径越界应被拒: {abs}");

    // 7. 未知方法
    let unknown = sb.handle(&json!({ "id": 8, "method": "rm_rf" }));
    assert_eq!(unknown["ok"], false, "未知方法: {unknown}");

    // 8. 命令白名单生效
    let strict = Sandbox::new(dir.clone(), vec!["echo".to_string()]);
    let denied = strict.handle(&json!({ "id": 9, "method": "exec", "params": { "command": "format c:" } }));
    assert_eq!(denied["ok"], false, "白名单拒绝: {denied}");
    let allowed = strict.handle(&json!({ "id": 10, "method": "exec", "params": { "command": "echo ok" } }));
    assert_eq!(allowed["ok"], true, "白名单放行: {allowed}");

    let _ = std::fs::remove_dir_all(&dir);
    println!("bailongma-sandbox self-test: ALL PASS");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注意：必须返回 TempDir（而非路径字符串）——TempDir drop 会删除目录，
    /// 若只返回 PathBuf，目录在函数返回时即被删除，sb.root 指向不存在的路径，
    /// exec 的 current_dir 会以 os error 267 失败（write_file 靠 create_dir_all
    /// 重建父目录而侥幸通过，掩盖了这个坑）。
    fn test_sandbox() -> (Sandbox, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(dir.path().to_path_buf(), vec![]);
        (sb, dir)
    }

    #[test]
    fn write_read_roundtrip_utf8() {
        let (sb, _d) = test_sandbox();
        let w = sb.handle(&json!({ "id": 1, "method": "write_file", "params": { "path": "t/中文.txt", "content": "你好，世界" } }));
        assert_eq!(w["ok"], true);
        let r = sb.handle(&json!({ "id": 2, "method": "read_file", "params": { "path": "t/中文.txt" } }));
        assert_eq!(r["result"]["content"], "你好，世界");
    }

    #[test]
    fn path_traversal_rejected() {
        let (sb, _d) = test_sandbox();
        let r = sb.handle(&json!({ "id": 1, "method": "read_file", "params": { "path": "../../etc/passwd" } }));
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("越界"));
    }

    #[test]
    fn exec_runs_and_captures() {
        let (sb, _d) = test_sandbox();
        let r = sb.handle(&json!({ "id": 1, "method": "exec", "params": { "command": "echo sandbox-ok" } }));
        assert_eq!(r["ok"], true, "exec 应成功: {r}");
        assert_eq!(r["result"]["exit_code"], 0, "exit 0: {r}");
        assert!(
            r["result"]["stdout"].as_str().unwrap_or("").contains("sandbox-ok"),
            "stdout 捕获: {r}"
        );
    }

    #[test]
    fn exec_timeout_kills() {
        let (sb, _d) = test_sandbox();
        let r = sb.handle(&json!({ "id": 1, "method": "exec", "params": { "command": "ping -n 30 127.0.0.1", "timeout_ms": 300 } }));
        assert_eq!(r["ok"], true, "exec 应返回 ok（超时也算结果）: {r}");
        assert_eq!(r["result"]["timed_out"], true, "应标记超时: {r}");
    }

    #[test]
    fn allowlist_gates_commands() {
        let dir = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(dir.path().to_path_buf(), vec!["echo".into()]);
        assert_eq!(
            sb.handle(&json!({ "id": 1, "method": "exec", "params": { "command": "del *.*" } }))["ok"],
            false
        );
        assert_eq!(
            sb.handle(&json!({ "id": 2, "method": "exec", "params": { "command": "echo allowed" } }))["ok"],
            true
        );
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "你好世界";
        let (t, cut) = truncate_bytes(s, 5); // 5 字节切在「你」后 1 字节处 → 回退到 3
        assert!(cut);
        assert_eq!(t, "你");
        let (t2, _) = truncate_bytes(s, 100);
        assert_eq!(t2, s);
    }

    #[test]
    fn list_dir_sorts_dirs_first() {
        let (sb, d) = test_sandbox();
        std::fs::create_dir_all(d.path().join("b_dir")).unwrap();
        std::fs::write(d.path().join("a_file.txt"), "x").unwrap();
        let r = sb.handle(&json!({ "id": 1, "method": "list_dir", "params": { "path": "." } }));
        let entries = r["result"]["entries"].as_array().unwrap();
        assert_eq!(entries[0]["name"], "b_dir");
        assert_eq!(entries[0]["kind"], "dir");
        assert_eq!(entries[1]["name"], "a_file.txt");
    }

    #[cfg(windows)]
    #[test]
    fn normalize_keeps_drive_prefix() {
        let p = normalize_absolute(Path::new("c:/Users/ADMIN/../ADMIN/app/./x"));
        assert_eq!(p.to_string_lossy(), "C:\\Users\\ADMIN\\app\\x");
    }
}
