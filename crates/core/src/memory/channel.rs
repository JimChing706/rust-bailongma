//! 渠道规范化与系统信号判定（对齐 `src/runtime/channel.js`）。

/// 渠道规范化（对齐 `normalizeChannel`）：
/// - 空 → `TUI`；
/// - 已知别名 → 映射表值（`WECHAT_CLAWBOT`/`WECHAT_OFFICIAL` → `WECHAT` 等）；
/// - 其余 → 原样大写。
pub fn normalize_channel(channel: &str) -> String {
    let c = channel.trim();
    if c.is_empty() {
        return "TUI".to_string();
    }
    match c {
        "WECHAT_CLAWBOT" | "WECHAT_OFFICIAL" | "WECHAT" => "WECHAT".into(),
        "WECOM" => "WECOM".into(),
        "DISCORD" => "DISCORD".into(),
        "FEISHU" => "FEISHU".into(),
        "TUI" | "API" | "voice" | "VOICE" | "语音识别" | "语音对话" | "FocusBanner" => {
            "TUI".into()
        }
        "REMINDER" | "SYSTEM" | "APP_SIGNAL" => "SYSTEM".into(),
        other => other.to_uppercase(),
    }
}

/// 语音渠道判定（对齐 `isVoiceChannel`）。
pub fn is_voice_channel(channel: &str) -> bool {
    matches!(
        channel.trim(),
        "voice" | "VOICE" | "语音识别" | "语音对话" | "FocusBanner"
    )
}

/// 系统信号判定（对齐 `isSystemSignalRow`）：from_id 为 SYSTEM、
/// 规范化渠道为 SYSTEM、或原始渠道为 APP_SIGNAL/REMINDER。
/// `fallback_channel`：row.channel 为空时的回退（对齐 currentMsg.channel 回退语义）。
pub fn is_system_signal_row(from_id: &str, channel: &str, fallback_channel: &str) -> bool {
    let ch = if channel.trim().is_empty() {
        fallback_channel
    } else {
        channel
    };
    let norm = normalize_channel(ch);
    from_id == "SYSTEM" || norm == "SYSTEM" || ch == "APP_SIGNAL" || ch == "REMINDER"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_channel_maps_aliases_and_uppercases() {
        assert_eq!(normalize_channel(""), "TUI");
        assert_eq!(normalize_channel("WECHAT_CLAWBOT"), "WECHAT");
        assert_eq!(normalize_channel("WECHAT_OFFICIAL"), "WECHAT");
        assert_eq!(normalize_channel("wechat"), "WECHAT");
        assert_eq!(normalize_channel("voice"), "TUI");
        assert_eq!(normalize_channel("FocusBanner"), "TUI");
        assert_eq!(normalize_channel("REMINDER"), "SYSTEM");
        assert_eq!(normalize_channel("APP_SIGNAL"), "SYSTEM");
        assert_eq!(normalize_channel("slack"), "SLACK");
    }

    #[test]
    fn system_signal_row_detection() {
        assert!(is_system_signal_row("SYSTEM", "", ""));
        assert!(is_system_signal_row("ID:1", "REMINDER", ""));
        assert!(is_system_signal_row("ID:1", "APP_SIGNAL", ""));
        assert!(!is_system_signal_row("ID:1", "", "WECHAT"));
        assert!(!is_system_signal_row("ID:1", "TUI", ""));
        // fallback 语义：row.channel 为空回退到 currentMsg.channel
        assert!(is_system_signal_row("ID:1", "", "REMINDER"));
        assert!(!is_system_signal_row("ID:1", "", "TUI"));
    }
}
