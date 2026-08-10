//! 编程纪律内化块（对齐 `src/prompt-blocks/coding-discipline.js`）。
//!
//! 「内化」= runtime 检测到编程/排障场景时把方法段主动注入 system prompt，
//! 不是 agent 自己想起来去读 skill 文件。触发三信号源（任一命中即注入）：
//! 1. 本轮用户消息文本命中场景词；
//! 2. 当前 task 文本命中（TICK 自主干活轮也能触发，哪怕用户一字未发）；
//! 3. 最近动作模式：recentActions 出现 write_file + exec_command/node/npm 组合。

use std::sync::OnceLock;

use regex::Regex;

/// 写代码纪律块（对齐 `CODING_BLOCK`）。
pub const CODING_BLOCK: &str = r#"## Coding Discipline
You are writing or modifying code. Work in vertical slices, not horizontal ones:
1. **Skeleton first — run it immediately.** Write the smallest thing that can run (one entry file with stub content), start it, and verify it actually loads (exec_command to run it, web_read to see the page). Only then add features. Never write the whole project across several files and run it for the first time at the end — by then every bug is buried under four files at once.
2. **One slice = one verification.** After each meaningful addition, run/fetch again. One tool call buys you certainty about exactly which change broke what.
3. **Make state visible.** Demos and prototypes should render their internal state on screen (current phase, key values, sim time) so problems show themselves instead of hiding in silence.
4. **One command to run.** A single entry (node server.js or one HTML file). No build steps unless the user asked for them.
5. **web_read is your eyes — the stateful browser is the user's.** Before opening anything for the user or reporting done: read the page yourself, confirm the entry resources load, read the server's stderr. An unverified deliverable is a guess, not a result. Runtime probes URLs you open and writes the real HTTP status into the tool result — read it and act on it.
6. **Edit files with read_file + write_file — never with shell text replacement.** PowerShell Get-Content/-replace/Set-Content reads UTF-8 as GBK and silently destroys every multibyte character (Chinese, symbols) in the file; sed/python -c one-liners hit quote-escaping traps. For any edit, however small: read_file → modify in your head → write_file the whole file. If you need scripted processing, write the script to a file with write_file and run it with node."#;

/// 排障纪律块（对齐 `DIAGNOSE_BLOCK`）。
pub const DIAGNOSE_BLOCK: &str = r#"## Debugging Discipline
Something is broken. Before touching any code:
1. **Build a feedback loop first.** Construct a repeatable pass/fail check that reproduces the symptom — web_read asserting on the response, exec_command running the entry and reading its output, re-running the exact failing command. A reliable loop is 90% of the fix: every later step just consumes its signal.
2. **Reproduce before you hypothesize.** Run the loop and watch it fail the way the user described. If you cannot reproduce it, say so and ask for the missing artifact (exact error text, what the screen shows) — do not guess-fix.
3. **List 3 ranked, falsifiable hypotheses.** Each must make a prediction: "if X is the cause, changing Y makes the symptom disappear". A hypothesis without a prediction is a vibe — sharpen it or drop it. Never grab the first plausible idea and start editing.
4. **Change one variable at a time**, testing against the loop, starting from the top hypothesis.
5. **The fix is proven only when the loop flips to pass** — the original symptom, not a nearby one. Then tell the user the cause in one line."#;

fn coding_text_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // 对齐 CODING_TEXT_RE：中文动宾组合（≤16 字修饰语、不跨句）+ 强独立词 +
        // 点名英文目录/文件 + 英文 build/写码模式。
        Regex::new(r"(?i)(写|做|搞|弄|建|搭|搭建|重建|改|实现|开发|重构|优化)\s*(个|一个|一下|下)?[^，。,.!?；;：:\n]{0,16}(代码|脚本|程序|网页|页面|网站|项目|工具|插件|游戏|demo|原型|app|应用|动画|可视化|模拟|仿真|服务|接口|爬虫|机器人)|编程|代码|前端|后端|html|css|javascript|typescript|python脚本|three\.js|api接口|webgl|3d\s*(可视化|动画|模型|场景)|[a-z][\w.-]{2,}\s*(目录|文件夹)|\.(js|html|css|py|ts|json|mjs)\b|build (a|an|the|some)?[\w -]{0,20}(app|page|site|tool|script|demo|prototype|server|game)|(write|implement|refactor|code up) ")
            .expect("static regex")
    })
}

fn diagnose_text_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)报错|出错|错误|坏了|崩了|崩溃|打不开|不工作|不能用|用不了|没反应|不显示|白屏|黑屏|加载不出|404|500|修一下|修复|修好|排查|诊断|debug|broken|not working|error|bug\b|fix (this|it|the)")
            .expect("static regex")
    })
}

/// 最近动作模式：正在写代码（write_file + 执行/起服务 同窗出现）。
fn recent_actions_look_like_coding(recent_actions_text: &str) -> bool {
    let t = recent_actions_text;
    if t.is_empty() {
        return false;
    }
    static EXEC_RE: OnceLock<Regex> = OnceLock::new();
    let exec_re =
        EXEC_RE.get_or_init(|| Regex::new(r"(?i)exec_command\(|node |npm ").expect("static regex"));
    t.contains("write_file(") && exec_re.is_match(t)
}

/// 编程纪律注入判定（对齐 `shouldInjectCoding`）。
pub fn should_inject_coding(
    user_message: &str,
    task_text: &str,
    recent_actions_text: &str,
) -> bool {
    if coding_text_re().is_match(user_message) {
        return true;
    }
    if coding_text_re().is_match(task_text) {
        return true;
    }
    recent_actions_look_like_coding(recent_actions_text)
}

/// 排障纪律注入判定（对齐 `shouldInjectDiagnose`）。
pub fn should_inject_diagnose(user_message: &str, task_text: &str) -> bool {
    if diagnose_text_re().is_match(user_message) {
        return true;
    }
    // task 文本命中症状词（例如「修复 XX 页面打不开」的任务在 TICK 轮继续时）
    diagnose_text_re().is_match(task_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coding_detects_chinese_verb_phrases() {
        assert!(should_inject_coding("帮我写一个爬虫", "", ""));
        assert!(should_inject_coding("搭一个3D可视化", "", ""));
        assert!(should_inject_coding("把 free-return 目录清掉重做", "", ""));
        assert!(should_inject_coding("报错", "做一个网页", "")); // task 文本命中
        assert!(should_inject_coding("", "重构前端代码", "")); // task 文本命中
        assert!(!should_inject_coding("帮我做个计划表", "", ""));
        assert!(!should_inject_coding("", "", ""));
    }

    #[test]
    fn coding_detects_english_and_recent_actions() {
        assert!(should_inject_coding("build a landing page", "", ""));
        assert!(should_inject_coding(
            "",
            "",
            "write_file(/a.js) + exec_command(node)"
        ));
        assert!(!should_inject_coding("", "", "read_file(x) + web_read(y)"));
        assert!(!should_inject_coding("", "", ""));
    }

    #[test]
    fn diagnose_detects_symptom_words() {
        assert!(should_inject_diagnose("页面打不开", ""));
        assert!(should_inject_diagnose("", "修复 XX 崩溃的问题"));
        assert!(!should_inject_diagnose("今天天气不错", ""));
    }
}
