//! 安装软件意图判定（对齐 `src/software-install-intent.js`）。
//!
//! 独立成模块，供能力注册表（[`super::capability_registry`]）的 software-install 能力
//! 与 tool-router 共用，避免工具注入与每轮方向提示两套关键词漂移。

use std::sync::OnceLock;

use regex::Regex;

/// 安装软件触发词（对齐 `SOFTWARE_INSTALL_TRIGGERS`；字面包含匹配，已小写）。
pub const SOFTWARE_INSTALL_TRIGGERS: &[&str] = &[
    "安装软件",
    "安装应用",
    "安装程序",
    "安装客户端",
    "装软件",
    "装应用",
    "装程序",
    "装客户端",
    "下载安装包",
    "下载软件",
    "软件下载",
    "软件安装包",
    "安装包",
    "官方安装包",
    "安装微信",
    "装微信",
    "下载微信",
    "微信安装包",
    "安装qq",
    "装qq",
    "下载qq",
    "qq安装包",
    "安装剪映",
    "装剪映",
    "下载剪映",
    "剪映安装包",
    "capcut",
    "安装浏览器",
    "装浏览器",
    "下载浏览器",
    "install app",
    "install software",
    "install program",
    "install client",
    "download installer",
    "download setup",
    "software installer",
    "setup.exe",
    ".msi",
    ".exe",
];

/// 安装动词（对齐 `INSTALL_VERB_RE`）。
fn install_verb_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"安装|装一下|装个|装一个|装上|下载并安装|帮我装|给我装|\binstall\b|\bsetup\b")
            .expect("static regex")
    })
}

/// 软件名词（对齐 `SOFTWARE_NOUN_RE`）。
fn software_noun_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"软件|应用|程序|客户端|安装包|installer|setup\.exe|\.msi|\.exe|\bapp\b|\bapplication\b|\bprogram\b|\bclient\b")
            .expect("static regex")
    })
}

/// 常见桌面软件名（对齐 `COMMON_DESKTOP_APP_RE`）。
fn common_desktop_app_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:qq|tim|wechat|weixin|chrome|edge|firefox|vscode|code|git|node(?:\.js)?|python|docker|steam|discord|slack|zoom|notion|obs|vlc|potplayer|wps|office|7-?zip|winrar)\b|微信|腾讯qq|qq音乐|剪映|飞书|钉钉|企业微信|浏览器|输入法")
            .expect("static regex")
    })
}

/// winget 包 ID 形态（对齐 `WINGET_PACKAGE_ID_RE`，如 `Tencent.QQ` / `Microsoft.VSCode`；
/// Node 端带 `/i` → Rust 用 `(?i)`）。
fn winget_package_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b[a-z0-9][a-z0-9_.-]+\.[a-z0-9][a-z0-9_.-]+\b").expect("static regex")
    })
}

/// 是否为安装软件请求（对齐 `isSoftwareInstallRequest`）：
/// 触发词直接命中 → true；否则需同时有安装动词 +（软件名词 | 常见软件名 | winget 包 ID）。
pub fn is_software_install_request(text: &str) -> bool {
    let raw = text;
    let lower = raw.to_lowercase();

    if SOFTWARE_INSTALL_TRIGGERS
        .iter()
        .any(|t| lower.contains(&t.to_lowercase()))
    {
        return true;
    }
    if !install_verb_re().is_match(raw) {
        return false;
    }

    software_noun_re().is_match(raw)
        || common_desktop_app_re().is_match(raw)
        || winget_package_id_re().is_match(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_word_directly_hits() {
        assert!(is_software_install_request("帮我安装微信"));
        assert!(is_software_install_request("下载qq安装包"));
        assert!(is_software_install_request("install app"));
        assert!(is_software_install_request("setup.exe"));
    }

    #[test]
    fn verb_plus_noun_hits() {
        assert!(is_software_install_request("装一个浏览器"));
        assert!(is_software_install_request("帮我装个 Chrome 浏览器"));
        // 动词 + winget 包 ID
        assert!(is_software_install_request("安装 Tencent.QQ"));
    }

    #[test]
    fn non_install_talk_misses() {
        assert!(!is_software_install_request("今天天气怎么样"));
        assert!(!is_software_install_request("这个软件怎么用"));
        assert!(!is_software_install_request("帮我搜一下安装教程"));
    }
}
