//! 文本协议标记的单一真相源（对齐 `src/runtime/markers.js`）。
//!
//! 模型输出文本里夹带 4 种运行时协议标记，投递前必须剥离：
//!   [RECALL: ...] / [SET_TASK: ...] / [CLEAR_TASK] / [UPDATE_PERSONA: ...]
//! 同时剥掉 `<think>`/`<thinking>` 块与开头的"松散内部思考前奏"行。
//!
//! 本模块只负责「剥离」，不做任何副作用（执行侧在 memory/injector 等调用方）。

use once_cell::sync::Lazy;
use regex::Regex;

/// `<think>`/`<thinking>` 块（对齐 THINK_STRIP）
static THINK_STRIP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<think(?:ing)?>[\s\S]*?</think(?:ing)?>").expect("think regex"));
/// [RECALL: ...]（单行，对齐 RECALL_STRIP）
static RECALL_STRIP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[RECALL:\s*.+?\]").expect("recall regex"));
/// [SET_TASK: ...]（跨行，对齐 SET_TASK_STRIP）
static SET_TASK_STRIP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[SET_TASK:\s*[\s\S]+?\]").expect("set_task regex"));
/// [CLEAR_TASK]
static CLEAR_TASK_STRIP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[CLEAR_TASK\]").expect("clear_task regex"));
/// [UPDATE_PERSONA: ...]（跨行）
static UPDATE_PERSONA_STRIP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[UPDATE_PERSONA:\s*[\s\S]+?\]").expect("persona regex"));

/// 高置信"内部思考行"（对齐 HIGH_CONFIDENCE_INTERNAL_LINE_RE）
static HIGH_CONFIDENCE_LINES: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"^(?:用户|user).*?(?:刚从|切到|切回|话题|意图|可能|上下文|语音输入|问)",
        r"^(?:结合上下文|考虑到|最(?:可能|自然)的(?:意图|理解)|话题(?:切回|切到|切换)|当前(?:最可能|应该是在问))",
        r"^the user\b.*\b(?:probably|likely|intent|context|topic|asked|switched)\b",
        r"^(?:given|considering)\b.*\b(?:context|conversation|history|user)\b",
    ]
    .iter()
    .map(|p| Regex::new(&format!("(?i){p}")).expect("high-confidence regex"))
    .collect()
});

/// 低置信"内部思考行"（对齐 LOW_CONFIDENCE_INTERNAL_LINE_RE）
static LOW_CONFIDENCE_LINES: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"^让我(?:想想|查|看|确认|检查)",
        r"^我(?:先|需要|来)(?:想想|查|看|确认|检查)",
        r"^I\s+(?:need to|should|will|can)\s+(?:check|look|think|figure|inspect)\b",
    ]
    .iter()
    .map(|p| Regex::new(&format!("(?i){p}")).expect("low-confidence regex"))
    .collect()
});

/// 剥掉 `<think>/<thinking>` 块和全部 4 个协议标记后返回正文。
/// 对齐 Node `stripMarkers`（含末尾 trim）。
pub fn strip_markers(text: &str) -> String {
    let s = THINK_STRIP.replace_all(text, "").into_owned();
    let s = RECALL_STRIP.replace_all(&s, "").into_owned();
    let s = SET_TASK_STRIP.replace_all(&s, "").into_owned();
    let s = CLEAR_TASK_STRIP.replace_all(&s, "").into_owned();
    let s = UPDATE_PERSONA_STRIP.replace_all(&s, "").into_owned();
    s.trim().to_string()
}

/// 行分类（对齐 classifyLooseInternalLine）：blank / high / low / none
fn classify_loose_internal_line(line: &str) -> &'static str {
    let s = line.trim();
    if s.is_empty() {
        return "blank";
    }
    if s.contains("<invoke") || s.contains("</invoke>") {
        return "none";
    }
    if s.chars().count() > 240 {
        return "none";
    }
    if HIGH_CONFIDENCE_LINES.iter().any(|re| re.is_match(s)) {
        return "high";
    }
    if LOW_CONFIDENCE_LINES.iter().any(|re| re.is_match(s)) {
        return "low";
    }
    "none"
}

/// 剥掉开头的"松散内部思考前奏"行（对齐 stripLooseThinkingPrelude）：
/// - 高置信行（dropped 或第一行）全剥；
/// - 低置信行仅在见过高置信行后剥；
/// - 空行在已剥过后继续剥；
/// - 一旦遇到非内部行立即停。
pub fn strip_loose_thinking_prelude(text: &str) -> String {
    let s = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let lines: Vec<&str> = s.split('\n').collect();
    let mut i = 0usize;
    let mut dropped = false;
    let mut saw_high_confidence = false;
    while i < lines.len() {
        let kind = classify_loose_internal_line(lines[i]);
        match kind {
            "blank" => {
                if dropped || i == 0 {
                    i += 1;
                    continue;
                }
                break;
            }
            "high" => {
                dropped = true;
                saw_high_confidence = true;
                i += 1;
                continue;
            }
            "low" if saw_high_confidence => {
                dropped = true;
                i += 1;
                continue;
            }
            _ => break,
        }
    }
    if dropped {
        lines[i..].join("\n").trim().to_string()
    } else {
        s.trim().to_string()
    }
}

/// 投递净化：剥标记 + 剥松散前奏（对齐 sanitizeAssistantReplyForDelivery）。
pub fn sanitize_assistant_reply_for_delivery(text: &str) -> String {
    strip_loose_thinking_prelude(&strip_markers(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_markers_removes_think_and_protocol_markers() {
        // 注意：对齐 Node —— replace 剥除后残留的空格不折叠，仅末尾 trim
        let t =
            "前置 <think>内部推理</think> 正文 [RECALL: 昨天的对话] 继续 [SET_TASK: 完成部署] 完毕";
        let got = strip_markers(t);
        assert_eq!(got, "前置  正文  继续  完毕");
    }

    #[test]
    fn strip_markers_removes_thinking_and_persona_and_clear() {
        // 无空格夹带时剥除后无缝拼接（对齐 Node：仅 replace，不做空格折叠）
        let t = "<thinking>多行\n思考</thinking>好[UPDATE_PERSONA: 叫我小马]啦[CLEAR_TASK]结束";
        let got = strip_markers(t);
        assert_eq!(got, "好啦结束");
    }

    #[test]
    fn sanitize_strips_loose_thinking_prelude() {
        // 高置信前奏行 + 低置信行（见过高置信后）→ 全部剥离
        let t = "用户刚从微信切到群聊，可能是在问部署的事\n我需要查一下日志。\n结论：可以部署。";
        let got = sanitize_assistant_reply_for_delivery(t);
        assert!(!got.contains("用户刚从微信切到群聊"), "got: {got}");
        assert!(!got.contains("我需要查一下日志"), "got: {got}");
        assert_eq!(got, "结论：可以部署。");

        // 低置信行 + 未见高置信 → 保留
        let t2 = "让我想想\n这是正文。";
        assert_eq!(
            sanitize_assistant_reply_for_delivery(t2),
            "让我想想\n这是正文。"
        );

        // 英文内部前奏
        let t3 = "the user probably switched topics here\nok done.";
        assert_eq!(sanitize_assistant_reply_for_delivery(t3), "ok done.");
    }

    #[test]
    fn sanitize_keeps_normal_reply() {
        let t = "好的，已为你部署完成，日志在 /var/log/app.log。";
        assert_eq!(sanitize_assistant_reply_for_delivery(t), t);
        assert_eq!(sanitize_assistant_reply_for_delivery(""), "");
    }

    #[test]
    fn strip_markers_handles_missing_close_think() {
        // 未闭合的 <think> 不剥（对齐正则语义，需闭合标签）
        let t = "开 <think>未闭合";
        assert_eq!(strip_markers(t), "开 <think>未闭合");
    }
}
