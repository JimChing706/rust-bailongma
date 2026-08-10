//! 按需能力展示意图（对齐 `src/capability-demo-intent.js`）：
//! 用户消息疑似 "你会做什么" 时注入候选提示，由模型按意图决定是否调 capability_demo。

use std::sync::OnceLock;

use regex::Regex;

/// 能力展示候选块（对齐 `CAPABILITY_DEMO_PROMPT_BLOCK`）。
pub const CAPABILITY_DEMO_PROMPT_BLOCK: &str = r#"## On-demand Capability Demo
The capability_demo tool is available in this turn because a lightweight gate saw a possible "what can you do" request. The gate is only a candidate filter; you decide by intent.
- If the user's intent is asking what you/BaiLongma can do, or explicitly requests a capability/function demo, showcase, or self-introduction through abilities, you MUST call capability_demo instead of answering with plain text.
- Do NOT call it for ordinary feasibility or implementation questions such as "这个能做吗", "这个功能能实现吗", "能不能做 X", or discussion about how to build this feature.
- Do NOT merely say the demo is happening. Saying "看屏幕" or "我把能力投出来了" without first calling capability_demo is a failure because no visual sequence will actually start.
- Call capability_demo first. Do not produce any assistant text before or after the tool call. The tool itself sends and speaks this intro while the visual sequence starts: "我能查查天气、操作读写你电脑上的文件、运行电脑里面的命令，还能给你网罗每日的热点信息" After the tool call, stop the round."#;

fn candidate_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:你|白龙马|bailongma|agent|ai|小白龙).{0,10}(?:能|会|可以|能够).{0,10}(?:做|干|帮|完成).{0,10}(?:什么|啥|哪些事|哪些事情)|(?:你|白龙马|bailongma|agent|ai|小白龙).{0,10}(?:有什么|有哪些).{0,10}(?:能力|功能|本事)|(?:能力|功能).{0,8}(?:展示|演示|秀一下|介绍|showcase|demo)|(?:展示|演示|秀一下).{0,8}(?:能力|功能)|what can you do|show(?: me)?(?: your)? capabilit")
            .expect("static regex")
    })
}

/// 能力展示意图判定（对齐 `shouldInjectCapabilityDemo`）。
pub fn should_inject_capability_demo(text: &str) -> bool {
    let raw = text.trim();
    if raw.is_empty() {
        return false;
    }
    candidate_re().is_match(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_demo_intent_detection() {
        assert!(should_inject_capability_demo("你能做什么？"));
        assert!(should_inject_capability_demo("你有什么能力"));
        assert!(should_inject_capability_demo("展示一下你的功能"));
        assert!(should_inject_capability_demo("what can you do"));
        assert!(!should_inject_capability_demo("这个功能能实现吗"));
        assert!(!should_inject_capability_demo(""));
        assert!(!should_inject_capability_demo("帮我写个脚本"));
    }
}
