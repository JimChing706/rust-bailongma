//! 本地 AI Agent 探测（对齐 `src/agents/detector.js`）。
//!
//! 纯探测层：对每个已知 Agent 定义运行探针（查 PATH / 端口 / WSL / 安装目录），
//! 返回 `DetectedAgent` 结果列表。**不写库**——写库由调用方（[`super::collect_agents`]）
//! 经 `db::repositories::agents::upsert_agents` 完成（对齐 Node `saveAgents`）。
//!
//! 与 Node 版行为对齐要点：
//! - 命令执行带超时（`execSync` 的 `timeout` 语义），超时即 kill 并视为探测失败；
//! - Windows 用 `where` / `netstat`，macOS/Linux 用 `which` / `lsof`；
//! - `wsl --list --quiet` 输出按 UTF-16LE 解码（Windows 上 Node 以 `utf-16le` 读取）；
//! - hermes 端口只查 1337/11434（避开 8080/8081 的泛化误报），openclaw 查 3210/3211/8765。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::db::models::NewKnownAgent;

/// 平台常量（与 Node `process.platform` 判断对齐）。
pub const IS_WIN: bool = cfg!(target_os = "windows");
pub const IS_MAC: bool = cfg!(target_os = "macos");

/// macOS/Linux 下 Electron 的 PATH 可能缺少用户路径，手动补全（对齐 EXTRA_PATH_DIRS）。
fn extra_path_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"), // Apple Silicon homebrew
        PathBuf::from("/usr/bin"),
        PathBuf::from("/opt/local/bin"), // MacPorts
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.extend([
            home.join(".local/bin"),
            home.join("bin"),
            home.join(".npm-global/bin"),
        ]);
    }
    dirs
}

/// 单个探针的探测结果（对齐 detector.js `probe()` 返回对象）。
#[derive(Debug, Clone, Default)]
pub struct ProbeResult {
    pub available: bool,
    pub version: Option<String>,
    pub invoke_type: Option<String>,
    pub invoke_cmd: Option<String>,
    pub invoke_args: Vec<String>,
    pub notes: String,
}

/// 一个 Agent 的完整探测输出（对齐 `detectAgents()` 的 results 条目）。
///
/// 序列化字段名对齐 Node `detectAgents` 返回的 camelCase 形状
/// （`invokeType`/`invokeCmd`/`invokeArgs`/`docsUrl`/`docsSearchQuery`/`detectedAt`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedAgent {
    pub id: String,
    pub name: String,
    pub description: String,
    pub available: bool,
    pub version: Option<String>,
    pub invoke_type: Option<String>,
    pub invoke_cmd: Option<String>,
    pub invoke_args: Vec<String>,
    pub notes: String,
    pub docs_url: Option<String>,
    pub docs_search_query: Option<String>,
    /// 探测时间（对齐 `new Date().toISOString()`）。
    pub detected_at: String,
}

impl DetectedAgent {
    /// 投影为 [`NewKnownAgent`]（`saveAgents` 的输入形状），供 upsert 写库。
    pub fn to_new_known_agent(&self) -> NewKnownAgent {
        NewKnownAgent {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            available: self.available,
            version: self.version.clone(),
            invoke_type: self.invoke_type.clone(),
            invoke_cmd: self.invoke_cmd.clone(),
            invoke_args: self.invoke_args.clone(),
            notes: self.notes.clone(),
            docs_url: self.docs_url.clone(),
            docs_search_query: self.docs_search_query.clone(),
            detected_at: Some(self.detected_at.clone()),
        }
    }
}

// ── 工具函数（对齐 detector.js 工具函数半） ─────────────────────────────────

/// Drop 时终止并回收子进程的守卫。
///
/// tokio 的 `Child` drop **不会**终止进程（与 std 不同）；探测在超时/取消路径上
/// 需要显式清理，否则预算截断会泄漏 wsl.exe 等子进程。用 [`KillOnDrop`] 包住
/// `Child` 后，无论正常返回还是被 `tokio::time::timeout` 取消，都会 kill + wait。
struct KillOnDrop(Option<tokio::process::Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            tokio::spawn(async move {
                let _ = child.start_kill();
                let _ = child.wait().await;
            });
        }
    }
}

/// 执行命令并捕获 stdout **原始字节**（超时/非零退出/无输出 → None）。
///
/// 不做任何编码转换：`wsl --list --quiet` 在 Windows 上输出 UTF-16LE，
/// 若先经 `String::from_utf8_lossy` 再取字节，原始 UTF-16LE 字节已被破坏
/// （每个 `\0` 变成 U+FFFD），后续 UTF-16LE 解码必然失败——必须先拿原始字节。
async fn run_cmd_bytes(cmd: &str, args: &[&str], timeout_ms: u64) -> Option<Vec<u8>> {
    let child = tokio::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut guard = KillOnDrop(Some(child));
    let mut buf = Vec::new();
    let mut stdout = guard.0.as_mut()?.stdout.take();
    let read_fut = async {
        if let Some(s) = &mut stdout {
            use tokio::io::AsyncReadExt;
            let _ = s.read_to_end(&mut buf).await;
        }
        if let Some(c) = &mut guard.0 {
            c.wait().await
        } else {
            Ok(std::process::ExitStatus::default())
        }
    };
    let status = match tokio::time::timeout(Duration::from_millis(timeout_ms), read_fut).await {
        Ok(Ok(st)) => st,
        _ => return None, // 超时/读失败：guard drop → kill 子进程
    };
    if !status.success() || buf.is_empty() {
        return None;
    }
    Some(buf)
}

/// 执行命令并捕获 stdout（trim 后非空返回 Some）。超时/非零退出/无输出 → None。
///
/// 对齐 `execSync(cmd, { timeout, encoding: 'utf-8' })`：超时即 kill 子进程，
/// 不经过 shell（Rust 直接 CreateProcess/exec，避免 shell 注入面）。
async fn run_cmd(cmd: &str, args: &[&str], timeout_ms: u64) -> Option<String> {
    let bytes = run_cmd_bytes(cmd, args, timeout_ms).await?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// 从 `where` 输出中选首个**可执行**路径（纯函数，便于单测）。
///
/// `where claude` 常同时返回无扩展名 bash shim 与 `claude.cmd`；CreateProcess
/// 只认带扩展名的项（Rust 1.84+ 可自动经 cmd.exe 运行 `.cmd`/`.bat`），故优先
/// 选扩展名非空的路径，兜底取首行。
fn pick_windows_executable(where_out: &str) -> Option<String> {
    let has_ext = |p: &str| !p.trim().is_empty() && PathBuf::from(p.trim()).extension().is_some();
    if let Some(p) = where_out.lines().find(|p| has_ext(p)) {
        return Some(p.trim().to_string());
    }
    where_out
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 在 PATH 中查找可执行文件，返回首个**可执行**路径（对齐 `findInPath`）。
///
/// Windows 用 `where` + [`pick_windows_executable`]（Node 的 execSync 经
/// cmd.exe + PATHEXT 解析到 `.cmd`，这里同样保证返回可直接 spawn 的路径）；
/// Unix 用 `which`，失败时逐目录检查 [`extra_path_dirs`] 兜底。
async fn find_in_path(name: &str) -> Option<String> {
    if IS_WIN {
        let out = run_cmd("where", &[name], 3000).await?;
        return pick_windows_executable(&out);
    }
    // macOS / Linux：`which` 失败后逐目录检查（Electron PATH 可能被裁剪）
    if let Some(out) = run_cmd("which", &[name], 3000).await {
        if let Some(first) = out.lines().next() {
            let first = first.trim();
            if !first.is_empty() {
                return Some(first.to_string());
            }
        }
    }
    for dir in extra_path_dirs() {
        let full = dir.join(name);
        if full.exists() {
            return Some(full.display().to_string());
        }
    }
    None
}

/// 执行命令并取整行 trim（对齐 `tryExec`）。
async fn try_exec(cmd: &str, args: &[&str]) -> Option<String> {
    run_cmd(cmd, args, 3000).await
}

/// 解析版本号（对齐 `parseVersion`：取首个 `数字.数字` 序列；否则取首行前 40 字符）。
fn parse_version(str: &str) -> Option<String> {
    if str.is_empty() {
        return None;
    }
    let re = regex::Regex::new(r"\d+\.\d+[\.\d]*").expect("static regex");
    if let Some(m) = re.find(str) {
        return Some(m.as_str().to_string());
    }
    let first_line = str.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        None
    } else {
        Some(first_line.chars().take(40).collect())
    }
}

/// 端口是否在监听（对齐 `isPortListening`）。
///
/// Windows: `netstat -ano`，逐行匹配 `0.0.0.0:port ` / `127.0.0.1:port ` / `[::]:port `，
/// 带尾随空格（对齐 Node `findstr ":<port> "`）——`:<port>` 不带空格会误命中
/// `:32105` 之类的同前缀端口；macOS/Linux: `lsof -iTCP:port -sTCP:LISTEN`（无需 root）。
async fn is_port_listening(port: u16) -> bool {
    if IS_WIN {
        let out = match run_cmd("netstat", &["-ano"], 2000).await {
            Some(o) => o,
            None => return false,
        };
        let f = |s: &str| format!("{s}:{port} ");
        return out.lines().any(|l| {
            l.contains(&f("0.0.0.0")) || l.contains(&f("127.0.0.1")) || l.contains(&f("[::]"))
        });
    }
    // macOS / Linux: lsof
    run_cmd(
        "lsof",
        &["-iTCP", &format!(":{port}"), "-sTCP:LISTEN", "-n", "-P"],
        2000,
    )
    .await
    .is_some()
}

// ── WSL 工具（仅 Windows；对齐 detector.js WSL 半） ─────────────────────────

/// UTF-16LE 解码（`wsl --list --quiet` 在 Windows 上的输出编码；Node 用 utf-16le 读）。
fn decode_utf16le(bytes: &[u8]) -> String {
    let b = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else {
        bytes
    };
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// 列出可用 WSL 发行版（对齐 `getWSLDistros`）。UTF-16LE 解码失败回退 UTF-8。
async fn get_wsl_distros() -> Vec<String> {
    if !IS_WIN {
        return Vec::new();
    }
    // 必须走原始字节路径：`wsl --list --quiet` 输出 UTF-16LE，经 run_cmd 的
    // lossy 转换后字节已被破坏（见 run_cmd_bytes 注释）。
    let Some(bytes) = run_cmd_bytes("wsl", &["--list", "--quiet"], 4000).await else {
        return Vec::new();
    };
    let decoded = decode_utf16le(&bytes);
    // 若 UTF-16LE 解码明显异常（大量替换符/空串），回退 UTF-8
    let text = if decoded.contains('\u{FFFD}') || decoded.trim().is_empty() {
        String::from_utf8_lossy(&bytes).to_string()
    } else {
        decoded
    };
    text.split_whitespace()
        .map(|s| s.trim_matches('\0').to_string())
        // docker-desktop 是 Docker Desktop 的内部 distro，不可能是 Agent 宿主；
        // 跳过可避免对每个 WSL distro 做一次慢速冷启动探测（不影响探测语义）。
        .filter(|s| !s.is_empty() && s != "(Default)" && !s.eq_ignore_ascii_case("docker-desktop"))
        .collect()
}

/// 在指定 WSL 发行版里执行 bash 命令（对齐 `wslExec`；失败返回 None）。
async fn wsl_exec(distro: &str, shell_cmd: &str) -> Option<String> {
    // 对齐 Node：命令拼接 ` 2>/dev/null` 过滤 WSL2 NAT 警告
    let full = format!("{shell_cmd} 2>/dev/null");
    run_cmd("wsl", &["-d", distro, "bash", "-c", &full], 5000).await
}

/// 在 WSL 里查找二进制（对齐 `findInWSL`）。
async fn find_in_wsl(distro: &str, name: &str) -> Option<String> {
    let result = wsl_exec(distro, &format!("which {name}")).await?;
    if result.starts_with("wsl:") {
        None
    } else {
        Some(result)
    }
}

/// WSL 内某端口是否在监听（对齐 `isPortListeningInWSL`）。
async fn is_port_listening_in_wsl(distro: &str, port: u16) -> bool {
    let cmd = format!(
        "{{ ss -lnt 2>/dev/null | grep -q ':{port}' || netstat -lnt 2>/dev/null | grep -q ':{port}'; }} && echo yes"
    );
    wsl_exec(distro, &cmd).await.as_deref() == Some("yes")
}

/// 获取 WSL 发行版的内网 IP（NAT 模式下 localhost 不通；对齐 `getWSLIP`）。
async fn get_wsl_ip(distro: &str) -> Option<String> {
    let ip = wsl_exec(
        distro,
        "ip -4 addr show eth0 2>/dev/null | grep -oP '(?<=inet\\s)\\d+(\\.\\d+){3}'",
    )
    .await;
    // hostname -I 兜底
    let ip = match ip {
        Some(ip) => Some(ip),
        None => wsl_exec(distro, "hostname -I 2>/dev/null | awk '{print $1}'").await,
    };
    let ip = ip?;
    let re = regex::Regex::new(r"^\d+\.\d+\.\d+\.\d+$").expect("static regex");
    if re.is_match(ip.trim()) {
        Some(ip.trim().to_string())
    } else {
        None
    }
}

// ── 各 Agent 探针（对齐 detector.js probe* 函数） ───────────────────────────

/// claude-code：PATH 中的 claude CLI → 桌面应用安装目录 → ~/.claude 配置目录。
async fn probe_claude_code() -> ProbeResult {
    if let Some(cli_path) = find_in_path("claude").await {
        // 用找到的可执行路径（Windows 上可能是 claude.cmd）取版本，而非裸名——
        // CreateProcess 不解析 .cmd/.bat，裸名会 spawn 失败
        let version = try_exec(&cli_path, &["--version"]).await;
        return ProbeResult {
            available: true,
            version: version
                .and_then(|v| parse_version(&v))
                .or_else(|| Some("unknown".into())),
            invoke_type: Some("cli".into()),
            invoke_cmd: Some("claude".into()),
            invoke_args: vec!["-p".into(), "{prompt}".into()],
            notes: format!("CLI: {cli_path}"),
        };
    }

    if IS_WIN {
        // Electron 桌面应用安装目录（Windows）
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Programs").join("Claude"));
        }
        if let Some(home) = dirs::home_dir() {
            dirs.push(
                home.join("AppData")
                    .join("Local")
                    .join("Programs")
                    .join("Claude"),
            );
        }
        for dir in dirs {
            if dir.exists() {
                return ProbeResult {
                    available: true,
                    version: Some("desktop".into()),
                    invoke_type: Some("cli".into()),
                    invoke_cmd: Some("claude".into()),
                    invoke_args: vec!["-p".into(), "{prompt}".into()],
                    notes: format!("Desktop app: {}", dir.display()),
                };
            }
        }
    } else if IS_MAC {
        for dir in [
            "/Applications/Claude.app",
            &format!("{}/Applications/Claude.app", home_display()),
        ] {
            if PathBuf::from(dir).exists() {
                return ProbeResult {
                    available: true,
                    version: Some("desktop".into()),
                    invoke_type: Some("cli".into()),
                    invoke_cmd: Some("claude".into()),
                    invoke_args: vec!["-p".into(), "{prompt}".into()],
                    notes: format!("Desktop app: {dir}"),
                };
            }
        }
    } else if let Some(home) = dirs::home_dir() {
        for dir in [
            home.join(".local/share/claude"),
            PathBuf::from("/opt/claude"),
        ] {
            if dir.exists() {
                return ProbeResult {
                    available: true,
                    version: Some("desktop".into()),
                    invoke_type: Some("cli".into()),
                    invoke_cmd: Some("claude".into()),
                    invoke_args: vec!["-p".into(), "{prompt}".into()],
                    notes: format!("Desktop app: {}", dir.display()),
                };
            }
        }
    }

    // ~/.claude 配置目录（说明安装过）
    if let Some(home) = dirs::home_dir() {
        let claude_config = home.join(".claude");
        if claude_config.exists() {
            return ProbeResult {
                available: true,
                version: Some("config-only".into()),
                invoke_type: Some("cli".into()),
                invoke_cmd: Some("claude".into()),
                invoke_args: vec!["-p".into(), "{prompt}".into()],
                notes: format!("Config dir: {}", claude_config.display()),
            };
        }
    }

    ProbeResult::default()
}

/// 供非 Windows 平台展示 home 路径的辅助（Mac 桌面目录探测用）。
fn home_display() -> String {
    dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_else(|| String::from("~"))
}

/// codex：PATH 中的 codex CLI → npm 全局安装 @openai/codex。
async fn probe_codex() -> ProbeResult {
    if let Some(cli_path) = find_in_path("codex").await {
        let version = try_exec(&cli_path, &["--version"]).await;
        return ProbeResult {
            available: true,
            version: version
                .and_then(|v| parse_version(&v))
                .or_else(|| Some("unknown".into())),
            invoke_type: Some("cli".into()),
            invoke_cmd: Some("codex".into()),
            invoke_args: vec!["{prompt}".into()],
            notes: format!("CLI: {cli_path}"),
        };
    }

    // npm 全局安装检测
    if let Some(npm_global) = try_exec("npm", &["root", "-g"]).await {
        let codex_pkg = PathBuf::from(npm_global.trim())
            .join("@openai")
            .join("codex");
        if codex_pkg.exists() {
            return ProbeResult {
                available: true,
                version: Some("npm-global".into()),
                invoke_type: Some("cli".into()),
                invoke_cmd: Some("codex".into()),
                invoke_args: vec!["{prompt}".into()],
                notes: format!("npm global: {}", codex_pkg.display()),
            };
        }
    }

    ProbeResult::default()
}

/// hermes：专属端口 1337/11434 → PATH hermes CLI → Ollama（Windows 原生）→ WSL 检测。
async fn probe_hermes() -> ProbeResult {
    // 只检测 Hermes / Ollama 专属端口（避开 8080/8081 的泛化误报）
    for port in [1337u16, 11434] {
        if is_port_listening(port).await {
            return ProbeResult {
                available: true,
                version: Some(format!("port:{port}")),
                invoke_type: Some("http".into()),
                invoke_cmd: Some(format!("http://localhost:{port}")),
                invoke_args: Vec::new(),
                notes: format!("HTTP on port {port}"),
            };
        }
    }

    // 检测 CLI
    if let Some(cli_path) = find_in_path("hermes").await {
        let version = try_exec(&cli_path, &["--version"]).await;
        return ProbeResult {
            available: true,
            version: version.or_else(|| Some("unknown".into())),
            invoke_type: Some("cli".into()),
            invoke_cmd: Some("hermes".into()),
            invoke_args: vec!["chat".into(), "--message".into(), "{prompt}".into()],
            notes: format!("CLI: {cli_path}"),
        };
    }

    // 检测 Ollama（Hermes 常跑在 Ollama 上，Windows 原生）
    if let Some(ollama_path) = find_in_path("ollama").await {
        if let Some(models) = try_exec(&ollama_path, &["list"]).await {
            if let Some(model_name) = extract_hermes_model(&models) {
                return ProbeResult {
                    available: true,
                    version: Some(format!("ollama:{model_name}")),
                    invoke_type: Some("cli".into()),
                    invoke_cmd: Some("ollama".into()),
                    invoke_args: vec!["run".into(), model_name.clone(), "{prompt}".into()],
                    notes: format!("Ollama (Windows native) model={model_name}"),
                };
            }
        }
    }

    // ── WSL 检测（仅 Windows） ──
    if IS_WIN {
        for distro in get_wsl_distros().await {
            // 1. WSL 内直接安装了 hermes CLI
            if let Some(hermes_path) = find_in_wsl(&distro, "hermes").await {
                let version = wsl_exec(&distro, "hermes --version").await;
                return ProbeResult {
                    available: true,
                    version: version
                        .and_then(|v| parse_version(&v))
                        .or_else(|| Some("unknown".into())),
                    invoke_type: Some("cli".into()),
                    invoke_cmd: Some("wsl".into()),
                    invoke_args: vec![
                        "-d".into(),
                        distro.clone(),
                        "hermes".into(),
                        "chat".into(),
                        "--message".into(),
                        "{prompt}".into(),
                    ],
                    notes: format!("WSL:{distro} {hermes_path}"),
                };
            }

            // 2. WSL 内跑了 Ollama + Hermes 模型
            if find_in_wsl(&distro, "ollama").await.is_some() {
                if let Some(models) = wsl_exec(&distro, "ollama list").await {
                    if let Some(model_name) = extract_hermes_model(&models) {
                        return ProbeResult {
                            available: true,
                            version: Some(format!("ollama-wsl:{model_name}")),
                            invoke_type: Some("cli".into()),
                            invoke_cmd: Some("wsl".into()),
                            invoke_args: vec![
                                "-d".into(),
                                distro.clone(),
                                "ollama".into(),
                                "run".into(),
                                model_name.clone(),
                                "{prompt}".into(),
                            ],
                            notes: format!("WSL:{distro} Ollama model={model_name}"),
                        };
                    }
                }
            }

            // 3. WSL 内跑了 HTTP 服务（Hermes server / Ollama API）
            for port in [11434u16, 1337] {
                if is_port_listening_in_wsl(&distro, port).await {
                    let wsl_ip = get_wsl_ip(&distro).await;
                    let base_url = match &wsl_ip {
                        Some(ip) => format!("http://{ip}:{port}"),
                        None => format!("http://localhost:{port}"),
                    };
                    return ProbeResult {
                        available: true,
                        version: Some(format!("wsl-http:{port}")),
                        invoke_type: Some("http".into()),
                        invoke_cmd: Some(base_url.clone()),
                        invoke_args: Vec::new(),
                        notes: format!(
                            "WSL:{distro} HTTP port {port}{}",
                            wsl_ip
                                .as_ref()
                                .map(|ip| format!(" ({ip})"))
                                .unwrap_or_default()
                        ),
                    };
                }
            }
        }
    }

    ProbeResult::default()
}

/// 从 `ollama list` 输出中提取 hermes 模型名（对齐 `models.match(/(hermes[\w.:/-]*)/i)`）。
fn extract_hermes_model(models: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?i)(hermes[\w.:/-]*)").expect("static regex");
    let m = re.find(models)?;
    let name = m
        .as_str()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// openclaw：只检测服务是否在线（端口监听；装了但没跑不算可用）。
async fn probe_openclaw() -> ProbeResult {
    for port in [3210u16, 3211, 8765] {
        if is_port_listening(port).await {
            return ProbeResult {
                available: true,
                version: Some(format!("port:{port}")),
                invoke_type: Some("http".into()),
                invoke_cmd: Some(format!("http://localhost:{port}")),
                invoke_args: Vec::new(),
                notes: format!("HTTP on port {port}"),
            };
        }
    }
    ProbeResult::default()
}

// ── 探针定义表（对齐 AGENT_PROBES；probe 逻辑按 id 分发） ───────────────────

struct AgentDef {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    docs_url: Option<&'static str>,
    docs_search_query: &'static str,
}

const AGENT_PROBES: [AgentDef; 4] = [
    AgentDef {
        id: "claude-code",
        name: "Claude Code",
        description: "擅长代码编写、重构、调试，支持多文件上下文",
        docs_url: Some("https://docs.anthropic.com/en/docs/claude-code/cli-usage"),
        docs_search_query: "Claude Code CLI usage documentation site:docs.anthropic.com",
    },
    AgentDef {
        id: "codex",
        name: "OpenAI Codex CLI",
        description: "代码生成与终端自动化，OpenAI 官方 CLI",
        docs_url: Some("https://github.com/openai/codex"),
        docs_search_query: "OpenAI Codex CLI usage documentation github",
    },
    AgentDef {
        id: "hermes",
        name: "Hermes",
        description: "本地 AI 助手，支持多模型对话与本地知识库",
        docs_url: Some("https://ollama.com/library/hermes3"),
        docs_search_query: "Hermes LLM ollama CLI usage how to run",
    },
    AgentDef {
        id: "openclaw",
        name: "小龙虾 OpenClaw",
        description: "自动化 Agent，支持工作流编排与多步任务",
        docs_url: None,
        docs_search_query: "OpenClaw AI agent CLI usage documentation",
    },
];

/// 按 id 分发到对应探针（异步 fn 无法直接放入 const 表，故用 match）。
async fn run_probe(id: &str) -> ProbeResult {
    match id {
        "claude-code" => probe_claude_code().await,
        "codex" => probe_codex().await,
        "hermes" => probe_hermes().await,
        "openclaw" => probe_openclaw().await,
        other => {
            tracing::warn!("[agents] 未知探针 id: {other}");
            ProbeResult::default()
        }
    }
}

// ── 主函数：扫描所有 Agent（对齐 detectAgents） ─────────────────────────────

/// 探测本机全部 Agent，返回结果列表（**不写库**，无扫描预算）。
///
/// 每个探针失败都会降级为 `available: false` 的结果（对齐 Node try/catch 包裹），
/// 不会中断整轮扫描。启动场景请用 [`super::collect_agents`]（内部带预算，
/// 对齐 Node 的 `withStartupTimeout(collectAgents(), 15000)` 语义）。
pub async fn detect_agents() -> Vec<DetectedAgent> {
    detect_agents_with_budget(Duration::ZERO).await
}

/// 带总预算的探测：单个探针超出剩余预算时降级为不可用并继续（不中断整轮）。
///
/// `budget = Duration::ZERO` 表示无预算限制（完整扫描）。预算耗尽后，剩余探针
/// 立即降级为不可用（tokio timeout(0) 立即返回），保证最坏情况在预算内返回。
pub(crate) async fn detect_agents_with_budget(budget: Duration) -> Vec<DetectedAgent> {
    let started = Instant::now();
    let now = crate::db::models::now_iso();
    let mut results = Vec::with_capacity(AGENT_PROBES.len());
    for def in &AGENT_PROBES {
        let probe = if budget.is_zero() {
            run_probe(def.id).await
        } else {
            let remaining = budget.saturating_sub(started.elapsed());
            match tokio::time::timeout(remaining, run_probe(def.id)).await {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(
                        "[agents] 探针 {} 超出扫描预算（剩余 {remaining:?}），记为不可用",
                        def.id
                    );
                    ProbeResult::default()
                }
            }
        };
        let notes = probe.notes.clone();
        results.push(DetectedAgent {
            id: def.id.to_string(),
            name: def.name.to_string(),
            description: def.description.to_string(),
            available: probe.available,
            version: probe.version,
            invoke_type: probe.invoke_type,
            invoke_cmd: probe.invoke_cmd,
            invoke_args: probe.invoke_args,
            notes: probe.notes,
            docs_url: def.docs_url.map(str::to_string),
            docs_search_query: Some(def.docs_search_query.to_string()),
            detected_at: now.clone(),
        });
        if probe.available {
            tracing::info!("[agents] 发现 {} ({})", def.name, notes);
        } else {
            tracing::debug!("[agents] 未发现 {}", def.name);
        }
    }
    results
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_windows_executable_prefers_extension() {
        // `where claude` 真实输出形状：无扩展名 shim 在前，.cmd 在后
        let out = "C:\\Users\\ADMIN\\AppData\\Roaming\\npm\\claude\r\nC:\\Users\\ADMIN\\AppData\\Roaming\\npm\\claude.cmd";
        assert_eq!(
            pick_windows_executable(out),
            Some("C:\\Users\\ADMIN\\AppData\\Roaming\\npm\\claude.cmd".to_string())
        );
        // 全是 .exe → 取首个
        let out2 = "C:\\Windows\\System32\\foo.exe\r\nC:\\tools\\foo.exe";
        assert_eq!(
            pick_windows_executable(out2),
            Some("C:\\Windows\\System32\\foo.exe".to_string())
        );
        // 空输出 → None
        assert_eq!(pick_windows_executable(""), None);
        assert_eq!(pick_windows_executable("\r\n  \r\n"), None);
    }

    #[tokio::test]
    async fn run_cmd_executes_cmd_shim_when_present() {
        // 真实数据回归（Windows + npm 全局装过 claude 时）：CreateProcess 应能经
        // Rust 的 .cmd 支持执行 claude.cmd 并返回版本。环境无此文件则跳过。
        if !IS_WIN {
            return;
        }
        let Some(appdata) = std::env::var_os("APPDATA") else {
            return;
        };
        let shim = PathBuf::from(appdata).join("npm").join("claude.cmd");
        if !shim.exists() {
            return;
        }
        let out = run_cmd(shim.to_str().unwrap(), &["--version"], 8000).await;
        assert!(
            out.is_some(),
            "claude.cmd --version 应可执行（npm 全局 shim）"
        );
        if let Some(v) = out {
            assert!(parse_version(&v).is_some(), "版本输出应能解析: {v}");
        }
    }

    #[tokio::test]
    async fn run_cmd_finds_where_executables() {
        // 平台无关的「必有」命令验证管道可用性
        if IS_WIN {
            let out = run_cmd("where", &["cmd"], 3000).await;
            assert!(out.is_some(), "where cmd 应成功");
        } else {
            let out = run_cmd("which", &["sh"], 3000).await;
            assert!(out.is_some(), "which sh 应成功");
        }
    }

    #[tokio::test]
    async fn run_cmd_missing_binary_returns_none() {
        assert!(run_cmd("this-binary-definitely-missing-xyz", &[], 500)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn run_cmd_nonzero_exit_returns_none() {
        if IS_WIN {
            assert!(run_cmd("cmd", &["/c", "exit 1"], 2000).await.is_none());
        } else {
            assert!(run_cmd("sh", &["-c", "exit 1"], 2000).await.is_none());
        }
    }

    #[test]
    fn parse_version_extracts_first_semver() {
        assert_eq!(
            parse_version("2.1.201 (abcdef)"),
            Some("2.1.201".to_string())
        );
        assert_eq!(
            parse_version("claude 1.2.3\nnext line"),
            Some("1.2.3".to_string())
        );
        assert_eq!(parse_version(""), None);
        // 无版本号 → 取首行前 40 字符
        assert_eq!(
            parse_version("not a version"),
            Some("not a version".to_string())
        );
        let long = "x".repeat(60);
        assert_eq!(parse_version(&long).unwrap().len(), 40);
    }

    #[test]
    fn extract_hermes_model_matches_node_regex() {
        assert_eq!(
            extract_hermes_model("hermes3:latest      3.8GB"),
            Some("hermes3:latest".to_string())
        );
        assert_eq!(
            extract_hermes_model("nomic-embed-text:latest  0.3GB\nhermes-3-llama-3.2:3b  2.1GB"),
            Some("hermes-3-llama-3.2:3b".to_string())
        );
        assert_eq!(extract_hermes_model("llama3.2:1b  1.3GB"), None);
        assert_eq!(extract_hermes_model(""), None);
    }

    #[test]
    fn decode_utf16le_handles_bom_and_plain() {
        // "Ubuntu\r\nDebian" 的 UTF-16LE + BOM
        let s = "Ubuntu\r\nDebian";
        let mut bytes = vec![0xFF, 0xFE];
        for u in s.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_utf16le(&bytes), s);
        // 无 BOM
        let bytes2: Vec<u8> = "X".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(decode_utf16le(&bytes2), "X");
    }

    #[test]
    fn detected_agent_maps_to_new_known_agent() {
        let agent = DetectedAgent {
            id: "codex".into(),
            name: "OpenAI Codex CLI".into(),
            description: "代码生成与终端自动化".into(),
            available: true,
            version: Some("0.1.0".into()),
            invoke_type: Some("cli".into()),
            invoke_cmd: Some("codex".into()),
            invoke_args: vec!["{prompt}".into()],
            notes: "CLI: /usr/bin/codex".into(),
            docs_url: Some("https://github.com/openai/codex".into()),
            docs_search_query: Some("OpenAI Codex CLI usage documentation github".into()),
            detected_at: "2026-08-09T00:00:00.000Z".into(),
        };
        let n = agent.to_new_known_agent();
        assert_eq!(n.id, "codex");
        assert_eq!(n.name, "OpenAI Codex CLI");
        assert!(n.available);
        assert_eq!(n.version.as_deref(), Some("0.1.0"));
        assert_eq!(n.invoke_type.as_deref(), Some("cli"));
        assert_eq!(n.invoke_args, vec!["{prompt}"]);
        assert_eq!(n.detected_at.as_deref(), Some("2026-08-09T00:00:00.000Z"));
    }

    #[test]
    fn agent_probes_metadata_complete() {
        assert_eq!(AGENT_PROBES.len(), 4);
        let ids: Vec<&str> = AGENT_PROBES.iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["claude-code", "codex", "hermes", "openclaw"]);
        for d in &AGENT_PROBES {
            assert!(!d.name.is_empty());
            assert!(!d.description.is_empty());
            assert!(!d.docs_search_query.is_empty());
        }
    }
}
