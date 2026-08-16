//! API 安全原语 —— origin / token / 局域网来源校验。
//!
//! 对齐 Node 版 `src/api.js` + `src/api/websocket-security.js`：
//! - loopback / 私有 LAN 地址判定（含 IPv4-mapped `::ffff:` 剥离）
//! - 常量时间 token 比较（Bearer header / `?token=` / WS `sec-websocket-protocol` base64url）
//! - HTTP origin 与 WebSocket upgrade 授权
//! - `/message` 来源限流（固定窗口，防刷接口烧额度 / 撑爆 DB）

/// WS 公共子协议名（对齐 WS_PUBLIC_PROTOCOL）
pub const WS_PUBLIC_PROTOCOL: &str = "bailongma.v1";
/// WS token 子协议前缀（对齐 WS_TOKEN_PROTOCOL_PREFIX）
pub const WS_TOKEN_PROTOCOL_PREFIX: &str = "bailongma.auth.";

/// 规范化远端地址：去方括号、去 IPv4-mapped 前缀、小写（对齐 normalizeRemoteAddress）。
pub fn normalize_remote_address(address: &str) -> String {
    let mut value = address.trim().to_ascii_lowercase();
    if value.starts_with('[') && value.ends_with(']') {
        value = value[1..value.len() - 1].to_string();
    }
    if let Some(rest) = value.strip_prefix("::ffff:") {
        rest.to_string()
    } else {
        value
    }
}

/// 是否回环地址（对齐 isLoopbackAddress）。
pub fn is_loopback_address(address: &str) -> bool {
    let v = normalize_remote_address(address);
    v == "127.0.0.1" || v == "::1" || v == "localhost"
}

fn is_private_ipv4(octets: &[u16]) -> bool {
    if octets.len() != 4 || !octets.iter().all(|&o| o <= 255) {
        return false;
    }
    let (a, b) = (octets[0] as u8, octets[1] as u8);
    // RFC1918: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16; link-local 169.254.0.0/16
    a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 169 && b == 254)
}

/// 是否私有局域网地址（对齐 isPrivateLanAddress：IPv4 RFC1918/link-local + IPv6 ULA/link-local）。
pub fn is_private_lan_address(address: &str) -> bool {
    let value = normalize_remote_address(address);
    if value.is_empty() {
        return false;
    }
    // IPv4
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() == 4 {
        let octets: Vec<u16> = parts.iter().filter_map(|p| p.parse::<u16>().ok()).collect();
        return is_private_ipv4(&octets);
    }
    // IPv6：ULA fc00::/7（fc/fd）或 link-local fe80::
    value.starts_with("fc") || value.starts_with("fd") || value.starts_with("fe80:")
}

/// 常量时间 token 比较（对齐 timingSafeTokenEqual：长度不等或空 → false）。
pub fn timing_safe_token_equal(provided: &str, expected: &str) -> bool {
    let p = provided.as_bytes();
    let e = expected.as_bytes();
    if p.len() != e.len() || e.is_empty() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in p.iter().zip(e.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 从 Authorization header 提取 Bearer token。
pub fn extract_bearer(authorization: &str) -> Option<String> {
    let h = authorization.trim();
    let rest = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))?;
    let token = rest.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// 从 sec-websocket-protocol 头解码 token（base64url，前缀 `bailongma.auth.`）。
pub fn decode_protocol_token(protocol_header: &str) -> Option<String> {
    for item in protocol_header.split(',') {
        let protocol = item.trim();
        let Some(encoded) = protocol.strip_prefix(WS_TOKEN_PROTOCOL_PREFIX) else {
            continue;
        };
        if let Ok(bytes) = base64url_decode(encoded) {
            if let Ok(s) = String::from_utf8(bytes) {
                return Some(s);
            }
        }
    }
    None
}

/// WebSocket 凭据：优先 Bearer，其次协议头（对齐 getWebSocketCredential）。
pub fn get_web_socket_credential(
    authorization: &str,
    sec_websocket_protocol: &str,
) -> Option<String> {
    extract_bearer(authorization).or_else(|| decode_protocol_token(sec_websocket_protocol))
}

/// HTTP origin 是否回环来源（对齐 isLoopbackOrigin）。
///
/// 安全修复（审计 H1）：`Origin: null` 是浏览器的 opaque origin（沙箱 iframe /
/// data: URL 页面），**不是**受信回环客户端——旧实现将其当回环放行，任意网页即可
/// 无凭据读取本机 API。空 origin（非浏览器客户端，如 curl）仍按回环处理。
pub fn is_loopback_origin(origin: &str) -> bool {
    if origin.is_empty() {
        return true;
    }
    if origin == "null" {
        return false;
    }
    let Ok(parsed) = url_hostname(origin) else {
        return false;
    };
    matches!(parsed.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// HTTP origin 是否被允许（对齐 isAllowedOrigin：回环或 LAN 私有）。
pub fn is_allowed_origin(origin: &str, lan_enabled: bool) -> bool {
    if is_loopback_origin(origin) {
        return true;
    }
    if !lan_enabled {
        return false;
    }
    match url_hostname(origin) {
        Ok(h) => is_private_lan_address(&h),
        Err(_) => false,
    }
}

/// WebSocket 来源是否被允许（对齐 isAllowedWebSocketOrigin）。
///
/// 安全修复（审计 H1）：opaque origin（`null`）的 WS 升级一律拒绝——`/scene` 流
/// 经 hello 快照广播审批卡 id，放行 null origin 会让任意网页窃取代批 exec_command。
/// 桌面/brain-ui 的 WS 走回环 HTTP origin（`http://127.0.0.1:*`），不受影响。
pub fn is_allowed_ws_origin(origin: &str, _remote_is_loopback: bool, lan_enabled: bool) -> bool {
    if origin.is_empty() {
        return true;
    }
    if origin == "null" {
        return false;
    }
    match url_hostname(origin) {
        Ok(h) => {
            if is_loopback_address(&h) {
                return true;
            }
            lan_enabled && is_private_lan_address(&h)
        }
        Err(_) => false,
    }
}

/// WebSocket upgrade 授权结果（对齐 authorizeWebSocketUpgrade 的 { ok, status, reason }）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsAuth {
    pub ok: bool,
    pub status: u16,
    pub reason: &'static str,
}

/// 带凭据的 WebSocket 授权（调用方已解析 header；对齐 authorizeWebSocketUpgrade）。
pub fn authorize_ws_upgrade_with_credential(
    pathname: &str,
    origin: &str,
    remote_address: &str,
    lan_enabled: bool,
    expected_token: &str,
    credential: Option<&str>,
    known_paths: &[&str],
) -> WsAuth {
    if !known_paths.contains(&pathname) {
        return WsAuth {
            ok: false,
            status: 404,
            reason: "unknown_path",
        };
    }
    let remote_is_loopback = is_loopback_address(remote_address);
    if !is_allowed_ws_origin(origin, remote_is_loopback, lan_enabled) {
        return WsAuth {
            ok: false,
            status: 403,
            reason: "forbidden_origin",
        };
    }
    if remote_is_loopback {
        return WsAuth {
            ok: true,
            status: 200,
            reason: "",
        };
    }
    if !lan_enabled || !is_private_lan_address(remote_address) {
        return WsAuth {
            ok: false,
            status: 403,
            reason: "forbidden",
        };
    }
    // LAN 来源必须携带有效 token
    if expected_token.is_empty() {
        return WsAuth {
            ok: false,
            status: 403,
            reason: "forbidden",
        };
    }
    let cred = credential.unwrap_or("");
    if !timing_safe_token_equal(cred, expected_token) {
        return WsAuth {
            ok: false,
            status: 403,
            reason: "forbidden",
        };
    }
    WsAuth {
        ok: true,
        status: 200,
        reason: "",
    }
}

/// 固定窗口限流器（按来源键独立计数；滑动窗口清理过期）。
///
/// 第 1 轮审计修复：`POST /message` 无速率限制，本机任意进程/网内设备
/// 可刷接口烧 API 额度、撑爆 DB。接入 guard 后按来源地址限流。
#[derive(Debug)]
pub struct RateLimiter {
    inner: std::sync::Mutex<std::collections::HashMap<String, Vec<std::time::Instant>>>,
    window: std::time::Duration,
    max: usize,
}

impl RateLimiter {
    pub fn new(window: std::time::Duration, max: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(std::collections::HashMap::new()),
            window,
            max,
        }
    }

    /// 尝试放行；窗口内已达上限返回 false。
    pub fn allow(&self, key: &str) -> bool {
        let now = std::time::Instant::now();
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let v = m.entry(key.to_string()).or_default();
        v.retain(|t| now.duration_since(*t) < self.window);
        if v.len() >= self.max {
            return false;
        }
        v.push(now);
        true
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(std::time::Duration::from_secs(10), 30)
    }
}

/// 第 3 轮审计检查项：LAN 暴露 fail-closed 启动检查。
///
/// 背景：运行中的桌面实例（Node 版 Bailongma.exe）曾以 `0.0.0.0:3721` 监听
/// 且未配置 token——网内任意设备可直接访问其 `/message`（旧版无 token 强制
/// 校验，不受 Rust 修复保护）。
///
/// Rust 版 serve 启动时强制执行：`network.allowLanAccess=true` 必须同时配置
/// `BAILONGMA_API_TOKEN`，否则拒绝启动（fail-closed）。不允许「开 LAN 但
/// 不设 token」的暴露态存在；只监听回环（allowLanAccess=false）时无 token 可放行。
///
/// 返回 `Ok(())` = 检查通过；`Err(message)` = 检查不通过（含修复指引）。
pub fn lan_exposure_check(lan_enabled: bool, token_configured: bool) -> Result<(), String> {
    if lan_enabled && !token_configured {
        return Err(
            "LAN 暴露检查未通过：network.allowLanAccess=true 但未配置 BAILONGMA_API_TOKEN。\
             开启局域网访问必须配置 token，否则拒绝启动（fail-closed）。\
             修复：设置环境变量 BAILONGMA_API_TOKEN=<强随机值> 后重启，\
             或关闭 allowLanAccess（仅回环 127.0.0.1 监听，网内不可达）。"
                .into(),
        );
    }
    Ok(())
}

/// 取 URL 的 hostname（简单解析，不引入 URL crate）。
fn url_hostname(origin: &str) -> Result<String, ()> {
    let s = origin.trim();
    if s.is_empty() {
        return Err(());
    }
    // 去掉 scheme://
    let rest = match s.find("://") {
        Some(i) => &s[i + 3..],
        None => s,
    };
    // 去掉 path/query/fragment
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // 去掉端口（IPv6 带括号时保持括号内内容）
    let host = if rest.starts_with('[') {
        let end = rest.find(']').ok_or(())?;
        &rest[..end + 1]
    } else {
        rest.split(':').next().unwrap_or(rest)
    };
    Ok(host.to_string())
}

/// base64url 解码（无填充，对齐 Node Buffer.from(..., 'base64url')）。
fn base64url_decode(input: &str) -> Result<Vec<u8>, ()> {
    let mut s = input.to_string();
    // 去掉空白
    s.retain(|c| !c.is_whitespace());
    match s.len() % 4 {
        0 => {}
        2 => s.push_str("=="),
        3 => s.push('='),
        _ => return Err(()),
    }
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE
        .decode(s.as_bytes())
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_address_normalization() {
        assert_eq!(
            normalize_remote_address("::ffff:192.168.1.5"),
            "192.168.1.5"
        );
        assert_eq!(normalize_remote_address("[::1]"), "::1");
        assert_eq!(normalize_remote_address(" 127.0.0.1 "), "127.0.0.1");
    }

    #[test]
    fn loopback_and_lan_classification() {
        assert!(is_loopback_address("127.0.0.1"));
        assert!(is_loopback_address("::1"));
        assert!(is_loopback_address("::ffff:127.0.0.1"));
        assert!(!is_loopback_address("192.168.1.1"));

        assert!(is_private_lan_address("10.1.2.3"));
        assert!(is_private_lan_address("172.16.0.1"));
        assert!(is_private_lan_address("172.31.255.255"));
        assert!(is_private_lan_address("192.168.0.1"));
        assert!(is_private_lan_address("169.254.10.10"));
        assert!(is_private_lan_address("fd00::1"));
        assert!(is_private_lan_address("fe80::1"));
        assert!(!is_private_lan_address("8.8.8.8"));
        assert!(!is_private_lan_address("172.32.0.1"));
        assert!(!is_private_lan_address("11.0.0.1"));
    }

    #[test]
    fn token_comparison_is_constant_time() {
        assert!(timing_safe_token_equal("abc", "abc"));
        assert!(!timing_safe_token_equal("abc", "abd"));
        assert!(!timing_safe_token_equal("", ""));
        assert!(!timing_safe_token_equal("abc", "abcd"));
    }

    #[test]
    fn bearer_and_protocol_token_extraction() {
        assert_eq!(extract_bearer("Bearer sk-123"), Some("sk-123".into()));
        assert_eq!(extract_bearer("bearer abc"), Some("abc".into()));
        assert_eq!(extract_bearer("Basic xxx"), None);
        // base64url: "hello" → aGVsbG8
        assert_eq!(
            decode_protocol_token("bailongma.auth.aGVsbG8"),
            Some("hello".into())
        );
        assert_eq!(decode_protocol_token("bailongma.v1"), None);
        assert_eq!(
            decode_protocol_token("chat, bailongma.auth.d29ybGQ="),
            Some("world".into())
        );
    }

    #[test]
    fn origin_checks() {
        assert!(is_loopback_origin(""));
        // 安全修复（审计 H1）：opaque origin 不再视为回环
        assert!(!is_loopback_origin("null"));
        assert!(is_loopback_origin("http://127.0.0.1:8080"));
        assert!(is_loopback_origin("http://localhost:3721"));
        assert!(!is_loopback_origin("http://evil.com"));
        assert!(!is_loopback_origin("not a url"));

        assert!(is_allowed_origin("http://127.0.0.1:3000", false));
        assert!(!is_allowed_origin("http://192.168.1.5", false));
        assert!(is_allowed_origin("http://192.168.1.5", true));
        assert!(!is_allowed_origin("http://8.8.8.8", true));
        // opaque origin 不被任何 LAN/回环判定放行
        assert!(!is_allowed_origin("null", false));
        assert!(!is_allowed_origin("null", true));
    }

    #[test]
    fn ws_origin_and_auth() {
        // 回环来源任意情况放行
        assert!(is_allowed_ws_origin("http://localhost:5173", true, false));
        // 非回环远端 + LAN 关闭 → 拒绝
        assert!(!is_allowed_ws_origin("http://192.168.1.5", false, false));
        // LAN 开启 + 私有来源 → 允许
        assert!(is_allowed_ws_origin("http://192.168.1.5", false, true));
        // 无 origin（原生客户端）→ 允许
        assert!(is_allowed_ws_origin("", false, false));
        // 安全修复（审计 H1）：opaque origin 的 WS 升级一律拒绝（无论远端是否回环）
        assert!(!is_allowed_ws_origin("null", true, false));
        assert!(!is_allowed_ws_origin("null", false, true));

        // 未知路径
        let a = authorize_ws_upgrade_with_credential(
            "/nope",
            "",
            "127.0.0.1",
            false,
            "",
            None,
            &["/scene"],
        );
        assert_eq!(a.status, 404);

        // 回环远端 + 无 origin（原生客户端）→ 放行
        let a = authorize_ws_upgrade_with_credential(
            "/scene",
            "",
            "127.0.0.1",
            false,
            "",
            None,
            &["/scene"],
        );
        assert!(a.ok);
        // 回环远端 + 恶意 origin → 仍拒绝（对齐 Node：origin 检查在回环放行之前）
        let a = authorize_ws_upgrade_with_credential(
            "/scene",
            "http://evil.com",
            "127.0.0.1",
            false,
            "",
            None,
            &["/scene"],
        );
        assert!(!a.ok);

        // LAN 无 token → 拒绝
        let a = authorize_ws_upgrade_with_credential(
            "/scene",
            "",
            "192.168.1.5",
            true,
            "secret",
            None,
            &["/scene"],
        );
        assert!(!a.ok);

        // LAN + 正确 token → 放行
        let a = authorize_ws_upgrade_with_credential(
            "/scene",
            "",
            "192.168.1.5",
            true,
            "secret",
            Some("secret"),
            &["/scene"],
        );
        assert!(a.ok);

        // LAN + 错误 token → 拒绝
        let a = authorize_ws_upgrade_with_credential(
            "/scene",
            "",
            "192.168.1.5",
            true,
            "secret",
            Some("wrong"),
            &["/scene"],
        );
        assert!(!a.ok);
    }

    #[test]
    fn hostname_parsing() {
        assert_eq!(
            url_hostname("http://localhost:3721/path?x=1").unwrap(),
            "localhost"
        );
        assert_eq!(url_hostname("https://[::1]:8080").unwrap(), "[::1]");
        assert_eq!(url_hostname("http://192.168.1.5").unwrap(), "192.168.1.5");
        assert!(url_hostname("").is_err());
    }

    #[test]
    fn rate_limiter_blocks_burst_per_key() {
        let rl = RateLimiter::new(std::time::Duration::from_secs(60), 3);
        assert!(rl.allow("a"));
        assert!(rl.allow("a"));
        assert!(rl.allow("a"));
        assert!(!rl.allow("a"), "同一来源窗口内第 4 次应被拒");
        assert!(rl.allow("b"), "不同来源独立计数");
    }

    // ── 第 3 轮审计检查项：LAN 暴露 fail-closed ──

    #[test]
    fn lan_exposure_check_fail_closed() {
        // 开 LAN + 无 token → 拒绝启动（fail-closed，即「LAN 暴露」必须被拦截）
        assert!(lan_exposure_check(true, false).is_err());
        // 开 LAN + 有 token → 放行
        assert!(lan_exposure_check(true, true).is_ok());
        // 关 LAN（仅回环 127.0.0.1）→ 无 token 也放行
        assert!(lan_exposure_check(false, false).is_ok());
        assert!(lan_exposure_check(false, true).is_ok());
    }

    #[test]
    fn lan_exposure_check_error_is_actionable() {
        let err = lan_exposure_check(true, false).unwrap_err();
        // 错误信息必须给出可执行修复路径（token 或改绑回环）
        assert!(err.contains("BAILONGMA_API_TOKEN"));
        assert!(err.contains("127.0.0.1"));
        assert!(err.contains("allowLanAccess"));
    }
}
