//! P2-1 沙箱逃逸测试套件（端到端）。
//!
//! 与 src/main.rs 内单元测试的区别：本套件以**真实子进程**方式启动
//! `bailongma-sandbox` 二进制，通过 stdin/stdout JSON-RPC 逐行发攻击载荷，
//! 覆盖协议层与进程边界上的逃逸面（单元测试直接调 `handle` 绕过了
//! 进程边界、stdin 解析与真实命令 spawn）。
//!
//! 攻击面矩阵：
//! - 路径逃逸：`..` 相对穿越 / 绝对路径越界 / 前缀碰撞兄弟目录 /
//!   混合分隔符 / 深嵌套 / 符号链接（junction）指向 root 外
//! - 命令逃逸：shell 链（&& | ;）/ 重定向元字符 / 引号混淆 / 大小写 /
//!   白名单外命令 / 环境变量泄露探针
//! - 资源防护：输出截断标记 / cwd 越界 / 写入越界与超限
//! - 协议层：畸形 JSON / 未知方法 / 空 id / 超大参数

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// 启动一个沙箱子进程；`--allow` 为空则不过滤命令（仅路径约束生效）。
fn spawn_sandbox(root: &Path, allow: &[&str]) -> std::process::Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bailongma-sandbox"));
    cmd.arg("--root").arg(root);
    if !allow.is_empty() {
        cmd.arg("--allow").arg(allow.join(","));
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动沙箱子进程失败")
}

/// 发一条请求并读回响应（一次一请求，串行协议）。
fn rpc(child: &mut std::process::Child, req: &Value) -> Value {
    let stdin = child.stdin.as_mut().expect("stdin 已关闭");
    writeln!(stdin, "{req}").expect("写请求失败");
    stdin.flush().expect("flush 失败");
    let stdout = child.stdout.as_mut().expect("stdout 已关闭");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("读响应失败");
    serde_json::from_str(&line).expect("响应不是合法 JSON")
}

fn kill(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 在 root 内创建指向 outside 的链接；失败返回 false（调用方跳过用例）。
#[cfg(windows)]
fn make_link(root: &Path, outside: &Path) -> bool {
    Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(root.join("leak").to_string_lossy().as_ref())
        .arg(outside.to_string_lossy().as_ref())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn make_link(root: &Path, outside: &Path) -> bool {
    std::os::unix::fs::symlink(outside, root.join("leak")).is_ok()
}

// ─────────────────────────────────────────────────────────────
// 1. 路径逃逸
// ─────────────────────────────────────────────────────────────

#[test]
fn escape_relative_dotdot_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(dir.path().join("secret.txt"), "OUTSIDE").unwrap();

    let mut child = spawn_sandbox(&root, &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "read_file", "params": { "path": "../secret.txt" } }),
    );
    kill(&mut child);

    assert_eq!(r["ok"], false, "`..` 穿越应被拒: {r}");
    assert!(r["error"].as_str().unwrap_or("").contains("越界"), "{r}");
}

#[test]
fn escape_absolute_path_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_sandbox(dir.path(), &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "read_file", "params": { "path": "C:\\Windows\\win.ini" } }),
    );
    kill(&mut child);
    assert_eq!(r["ok"], false, "绝对路径越界应被拒: {r}");
}

#[test]
fn escape_prefix_collision_sibling_rejected() {
    // root=…/root，`..\root2\…` 命中同名前缀兄弟目录（第 1 轮审计实锤的向量）
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let root2 = dir.path().join("root2");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&root2).unwrap();
    std::fs::write(root2.join("secret.txt"), "SIBLING-SECRET").unwrap();

    let mut child = spawn_sandbox(&root, &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "read_file", "params": { "path": "..\\root2\\secret.txt" } }),
    );
    kill(&mut child);
    assert_eq!(r["ok"], false, "前缀碰撞兄弟目录应被拒: {r}");
}

#[test]
fn escape_mixed_separators_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(dir.path().join("secret.txt"), "OUTSIDE").unwrap();

    let mut child = spawn_sandbox(&root, &[]);
    // 正反斜杠混用 + 多级 .. 归一化后仍越界
    for payload in [
        "../..\\..\\secret.txt",
        "a\\..\\..\\secret.txt",
        "....//secret.txt",
    ] {
        let r = rpc(
            &mut child,
            &json!({ "id": 1, "method": "read_file", "params": { "path": payload } }),
        );
        assert_eq!(r["ok"], false, "payload `{payload}` 应被拒: {r}");
    }
    kill(&mut child);
}

#[test]
fn escape_deep_dotdot_normalized() {
    // 深层合法路径内嵌 .. 但最终落在 root 内 → 应放行（正常化而非一律拒绝）
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    std::fs::write(root.join("target.txt"), "INSIDE").unwrap();

    let mut child = spawn_sandbox(&root, &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "read_file", "params": { "path": "a/b/c/../../../target.txt" } }),
    );
    kill(&mut child);
    assert_eq!(r["ok"], true, "归一化后仍在 root 内应放行: {r}");
    assert_eq!(r["result"]["content"], "INSIDE", "{r}");
}

#[test]
fn escape_symlink_escape_rejected_or_documented() {
    // junction 指向 root 外：当前实现是词法路径判定、不解析链接，
    // 若实现未升级 canonicalize 双保险，此测试失败并暴露真实逃逸缺口。
    // Windows 上 mklink /J 普通权限可用；创建失败则跳过（标注环境限制）。
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "LINK-ESCAPE-SECRET").unwrap();

    if !make_link(&root, &outside) {
        eprintln!("SKIP: 无法创建链接（权限/平台限制），本用例不判定");
        return;
    }

    let mut child = spawn_sandbox(&root, &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "read_file", "params": { "path": "leak/secret.txt" } }),
    );
    kill(&mut child);

    assert_eq!(
        r["ok"], false,
        "链接指向 root 外必须被拒（canonicalize 双保险）: {r}"
    );
}

#[test]
fn escape_symlink_write_path_new_file_rejected() {
    // 审计 B1 实锤的缺口：目标【尚不存在】时旧实现的 canonicalize 失败 → 跳过
    // 二次校验 → 经 junction 父目录逃逸（write_file 落盘到 root 外）。
    // 修复后：以父目录为锚点校验，新文件写入必须被拒，且 outside 无落盘。
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    if !make_link(&root, &outside) {
        eprintln!("SKIP: 无法创建链接（权限/平台限制），本用例不判定");
        return;
    }

    let mut child = spawn_sandbox(&root, &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "write_file",
                 "params": { "path": "leak/new.txt", "content": "ESCAPED" } }),
    );
    // 链接逃逸拒绝后沙箱仍应存活（后续请求可正常处理）
    let r2 = rpc(
        &mut child,
        &json!({ "id": 2, "method": "write_file",
                 "params": { "path": "ok.txt", "content": "FINE" } }),
    );
    kill(&mut child);

    assert_eq!(
        r["ok"], false,
        "写路径经 junction 父目录必须被拒（目标不存在也要校验）: {r}"
    );
    assert_eq!(r2["ok"], true, "拒绝逃逸后正常写路径应放行: {r2}");
    assert!(
        std::fs::read_to_string(outside.join("new.txt")).is_err(),
        "outside 目录必须无落盘文件（逃逸写入未发生）"
    );
}

// ─────────────────────────────────────────────────────────────
// 2. 命令逃逸
// ─────────────────────────────────────────────────────────────

#[test]
fn cmd_shell_chaining_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_sandbox(dir.path(), &["echo"]);
    for payload in [
        "echo hi && echo BYPASS-OK",
        "echo hi | more",
        "echo hi; echo BYPASS-OK",
        "echo hi > pwn.txt",
        "echo hi < secret.txt",
        "echo %PATH%",
        "echo `whoami`",
        "echo $HOME",
    ] {
        let r = rpc(
            &mut child,
            &json!({ "id": 1, "method": "exec", "params": { "command": payload } }),
        );
        assert_eq!(r["ok"], false, "payload `{payload}` 应被白名单拒绝: {r}");
    }
    kill(&mut child);
}

#[test]
fn cmd_allowlist_case_and_quote() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_sandbox(dir.path(), &["echo"]);
    // 白名单内大小写不敏感放行
    let ok = rpc(
        &mut child,
        &json!({ "id": 1, "method": "exec", "params": { "command": "ECHO hi" } }),
    );
    assert_eq!(ok["ok"], true, "大小写不敏感应放行: {ok}");
    // 引号内 argv[0] 匹配（"echo" 是白名单命令）
    let quoted = rpc(
        &mut child,
        &json!({ "id": 2, "method": "exec", "params": { "command": "\"echo\" hi" } }),
    );
    assert_eq!(quoted["ok"], true, "引号命令应放行: {quoted}");
    // 白名单外命令拒绝
    let denied = rpc(
        &mut child,
        &json!({ "id": 3, "method": "exec", "params": { "command": "type secret.txt" } }),
    );
    assert_eq!(denied["ok"], false, "白名单外命令应拒绝: {denied}");
    kill(&mut child);
}

#[test]
fn cmd_env_does_not_leak_token() {
    // 最小环境 + 敏感变量剔除：子进程内看不到 BAILONGMA_API_TOKEN / OPENAI_API_KEY
    // `set`（cmd 内部命令）列出全部环境变量；`%VAR%` 载荷会被 % 元字符拦截，故不用
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_sandbox(dir.path(), &["set"]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "exec", "params": { "command": "set" } }),
    );
    kill(&mut child);
    assert_eq!(r["ok"], true, "set 应在白名单内: {r}");
    let out = r["result"]["stdout"].as_str().unwrap_or("");
    assert!(
        !out.contains("API_TOKEN") && !out.contains("OPENAI"),
        "敏感变量不得泄露（当前环境变量: {}）: {r}",
        out.chars().take(300).collect::<String>()
    );
}

#[test]
fn cmd_output_truncation_marked() {
    let dir = tempfile::tempdir().unwrap();
    // 造 3000 个文件：`dir /s /b` 输出完整路径 ≈ 200KB（>64KB 截断阈值）。
    // 不用 echo & 拼接（超 cmd 命令行 32K 上限）、不用 %（元字符被拦）。
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    for i in 0..3000 {
        std::fs::write(out_dir.join(format!("file{i:04}.txt")), "").unwrap();
    }
    let mut child = spawn_sandbox(dir.path(), &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "exec", "params": { "command": "dir /s /b out", "timeout_ms": 15000 } }),
    );
    kill(&mut child);
    assert_eq!(r["ok"], true, "exec 应成功返回: {r}");
    assert_eq!(
        r["result"]["timed_out"], false,
        "大输出不应触发超时死锁: {r}"
    );
    assert_eq!(r["result"]["truncated"], true, "大输出应标记截断: {r}");
    assert!(
        r["result"]["stdout"].as_str().unwrap_or("").len() <= 64 * 1024,
        "stdout 不得超过 64KB: {r}"
    );
}

#[test]
fn cmd_cwd_outside_root_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let mut child = spawn_sandbox(&root, &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "exec", "params": { "command": "echo hi", "cwd": outside.to_string_lossy().as_ref() } }),
    );
    kill(&mut child);
    assert_eq!(r["ok"], false, "cwd 越界应拒绝: {r}");
}

// ─────────────────────────────────────────────────────────────
// 3. 写入与读取防护
// ─────────────────────────────────────────────────────────────

#[test]
fn write_escape_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let mut child = spawn_sandbox(&root, &[]);
    for payload in [
        "../evil.txt",
        "C:\\Windows\\evil.txt",
        "..\\root2\\evil.txt",
    ] {
        let r = rpc(
            &mut child,
            &json!({ "id": 1, "method": "write_file", "params": { "path": payload, "content": "PWNED" } }),
        );
        assert_eq!(r["ok"], false, "写入 payload `{payload}` 应被拒: {r}");
    }
    kill(&mut child);
    // 确认磁盘上没有越界写入产物
    assert!(
        !dir.path().join("evil.txt").exists(),
        "root 外不得出现写入产物"
    );
    assert!(!dir.path().join("root2").exists(), "root 外不得创建目录");
}

#[test]
fn write_oversized_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_sandbox(dir.path(), &[]);
    let big = "A".repeat(4 * 1024 * 1024 + 1);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "write_file", "params": { "path": "big.txt", "content": big } }),
    );
    kill(&mut child);
    assert_eq!(r["ok"], false, ">4MB 写入应拒绝: {r}");
}

#[test]
fn read_oversized_rejected_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.bin"), vec![0u8; 2 * 1024 * 1024]).unwrap();
    let mut child = spawn_sandbox(dir.path(), &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "read_file", "params": { "path": "big.bin", "max_bytes": 1024 } }),
    );
    kill(&mut child);
    assert_eq!(r["ok"], false, "超大文件应拒绝而非整读截断: {r}");
}

// ─────────────────────────────────────────────────────────────
// 4. 协议层
// ─────────────────────────────────────────────────────────────

#[test]
fn proto_malformed_json_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_sandbox(dir.path(), &[]);
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{{{{{{ not json").unwrap();
        stdin.flush().unwrap();
    }
    let stdout = child.stdout.as_mut().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let r: Value = serde_json::from_str(&line).unwrap();
    kill(&mut child);
    assert_eq!(r["ok"], false, "畸形 JSON 应返回错误: {r}");
    assert!(r["error"].as_str().unwrap_or("").contains("JSON"), "{r}");
}

#[test]
fn proto_unknown_method_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_sandbox(dir.path(), &[]);
    let r = rpc(
        &mut child,
        &json!({ "id": 1, "method": "rm_rf_everything", "params": {} }),
    );
    kill(&mut child);
    assert_eq!(r["ok"], false, "未知方法应拒绝: {r}");
}

#[test]
fn proto_null_id_still_processed() {
    // id 缺失/为 null 时仍应返回结果（不 panic、不崩溃），响应 id 为 null
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_sandbox(dir.path(), &[]);
    let r = rpc(&mut child, &json!({ "method": "ping" }));
    assert_eq!(r["ok"], true, "ping 应成功: {r}");
    assert_eq!(r["id"], Value::Null, "id 缺失时响应 id 为 null: {r}");
    kill(&mut child);
}
