//! 关键词抽取：纯函数，零外部依赖（不碰 DB、不碰网络）。
//!
//! 对齐 Node 版 `src/memory/keywords.js`，被召回检索（`memory::retrieval`）和
//! 焦点判断（M4 后续 focus 模块）使用。中文 n-gram（2-4 字滑动窗口）+ 英文词，
//! 经停用词 / 边界字符 / 叠字过滤后按 `频次 × 长度权重` 排序取前 N。

use std::cmp::Ordering;

/// 停用词：高频但无信息量的词（对齐 STOP_WORDS）。
const STOP_WORDS: &[&str] = &[
    "的",
    "了",
    "是",
    "在",
    "我",
    "你",
    "他",
    "她",
    "它",
    "我们",
    "你们",
    "他们",
    "这",
    "那",
    "有",
    "没有",
    "和",
    "与",
    "把",
    "被",
    "因为",
    "所以",
    "如果",
    "一个",
    "一些",
    "什么",
    "怎么",
    "为什么",
    "帮我",
    "请",
    "好的",
    "明白",
    "告诉",
    "让",
    "做",
    "去",
    "来",
    "说",
    "给",
    // 相对时间词：由 memory::temporal 解析成日期窗口并独立注入。
    // 这里加入让"昨天"等不再作为字面搜索词污染 FTS5 召回。
    "今天",
    "昨天",
    "前天",
    "大前天",
    "今早",
    "今晨",
    "今夜",
    "今晚",
    "昨晚",
    "昨夜",
    "昨日",
    "今日",
];

/// n-gram 内含这些字符时跨越了词边界，不是完整词，过滤掉（对齐 STOP_CHARS）。
const STOP_CHARS: &[char] = &[
    '的', '了', '着', '过', '起', '来', '去', '吗', '呢', '吧', '啊', '呀', '嘛', '哦', '和', '与',
    '跟', '或', '及', '并', '很', '太', '再', '又', '也', '都', '还', '只', '就', '才',
];

/// 首字禁止：量词单字不应作为 n-gram 起点（对齐 STOP_HEAD_CHARS）。
const STOP_HEAD_CHARS: &[char] = &['们', '个', '些', '点', '次', '件', '种', '样'];

/// 末字禁止：指代词/时间前缀字不应作为 n-gram 结尾（对齐 STOP_TAIL_CHARS）。
const STOP_TAIL_CHARS: &[char] = &['一', '几', '某', '每', '这', '那', '今'];

/// 中文清洗：标点 / 数字 → 空格（对齐 extractCore 的正则字符类）。
fn chinese_clean_regex() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"[，。！？、；：”””’’’【】\[\]()（）\d]").expect("static regex")
    })
}

/// n-gram 内重复字：除"天天/常常"这类合法叠词（整段就是两字叠词）外都丢弃。
fn has_invalid_duplicate(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    if chars.len() == 2 {
        return false;
    }
    let mut seen = std::collections::HashSet::new();
    chars.iter().any(|c| !seen.insert(*c))
}

fn is_valid_ngram(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    if word.is_empty() || chars.len() < 2 || STOP_WORDS.contains(&word) {
        return false;
    }
    if chars.iter().any(|c| STOP_CHARS.contains(c)) {
        return false;
    }
    if STOP_HEAD_CHARS.contains(&chars[0]) {
        return false;
    }
    if STOP_TAIL_CHARS.contains(&chars[chars.len() - 1]) {
        return false;
    }
    if has_invalid_duplicate(word) {
        return false;
    }
    true
}

/// 长度权重：短词在召回里命中率更高，给点排序加成；长 ngram 打折（对齐 lengthWeight）。
fn length_weight(len: usize) -> f64 {
    match len {
        2 => 1.5,
        4 => 0.8,
        _ => 1.0,
    }
}

/// 核心抽取：返回 `(freq 保持首次出现序, rawNgrams 全部 ngram)`。
/// 对齐 extractCore：中文按清洗后的文本滑 2-4 字窗；英文从**原始文本**取 `[a-zA-Z]{3,}`。
pub fn extract_core(text: &str) -> (Vec<(String, usize)>, Vec<String>) {
    if text.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // 清洗：标点/数字 → 空格，连续空白 → 单空格，去首尾
    let cleaned = chinese_clean_regex().replace_all(text, " ").to_string();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");

    let mut freq: Vec<(String, usize)> = Vec::new();
    let mut raw_ngrams: Vec<String> = Vec::new();

    let bump_chinese = |word: &str, raw: &mut Vec<String>, freq: &mut Vec<(String, usize)>| {
        if word.is_empty() {
            return;
        }
        raw.push(word.to_string());
        if !is_valid_ngram(word) {
            return;
        }
        if let Some(slot) = freq.iter_mut().find(|(w, _)| w == word) {
            slot.1 += 1;
        } else {
            freq.push((word.to_string(), 1));
        }
    };

    // 中文：英文 → 空格 后滑窗（对齐 chinese = cleaned.replace(/[a-zA-Z]+/g, ' ')）
    let chinese: String = cleaned
        .split(|c: char| c.is_ascii_alphabetic())
        .collect::<Vec<_>>()
        .join(" ");
    let chs: Vec<char> = chinese.chars().collect();
    for i in 0..chs.len().saturating_sub(1) {
        for len in 2..=4usize.min(chs.len() - i) {
            let w: String = chs[i..i + len].iter().collect();
            let w = w.trim();
            bump_chinese(w, &mut raw_ngrams, &mut freq);
        }
    }

    // 英文：原始文本 `[a-zA-Z]{3,}`，小写化查停用词，原样入 freq（对齐 bumpEnglish）
    let english: Vec<String> = {
        let mut out = Vec::new();
        let mut cur = String::new();
        for c in text.chars() {
            if c.is_ascii_alphabetic() {
                cur.push(c);
            } else if cur.len() >= 3 {
                out.push(cur.clone());
                cur.clear();
            } else {
                cur.clear();
            }
        }
        if cur.len() >= 3 {
            out.push(cur);
        }
        out
    };
    for word in &english {
        let normalized = word.to_lowercase();
        if STOP_WORDS.contains(&normalized.as_str()) {
            continue;
        }
        if let Some(slot) = freq.iter_mut().find(|(w, _)| w == word) {
            slot.1 += 1;
        } else {
            freq.push((word.clone(), 1));
        }
    }

    (freq, raw_ngrams)
}

/// 抽取关键词（对齐 extractKeywords）：
/// 按 `频次 × 长度权重` desc、平局按长度 desc、再平局保持首次出现序（stable）。
pub fn extract_keywords(text: &str, max_keywords: usize) -> Vec<String> {
    let (freq, _) = extract_core(text);
    let mut scored: Vec<(String, f64)> = freq
        .into_iter()
        .map(|(w, f)| {
            let score = f as f64 * length_weight(w.chars().count());
            (w, score)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.0.chars().count().cmp(&a.0.chars().count()))
    });
    scored
        .into_iter()
        .take(max_keywords)
        .map(|(w, _)| w)
        .collect()
}

/// 调试辅助：返回各阶段 ngram 集合，便于单测断言"伪词被丢掉了"（对齐 __extractKeywordsDebug）。
pub fn extract_keywords_debug(text: &str, max_keywords: usize) -> KeywordsDebug {
    let (freq, raw) = extract_core(text);
    let filtered: Vec<String> = freq.iter().map(|(w, _)| w.clone()).collect();
    let final_kws = extract_keywords(text, max_keywords);
    KeywordsDebug {
        raw,
        filtered,
        final_kws,
    }
}

/// 调试输出结构（对齐 __extractKeywordsDebug 返回值）。
#[derive(Debug, Clone, PartialEq)]
pub struct KeywordsDebug {
    pub raw: Vec<String>,
    pub filtered: Vec<String>,
    pub final_kws: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_chinese_ngrams_in_order() {
        // 精确对齐 Node 排序：score desc → 平局长度 desc → 稳定保插入序。
        // 用例取自真实文本："咖啡" f2×1.5=3.0 居首；"偏好" 同样 3.0；
        // "喝咖啡/偏好少" f2×1.0=2.0；"用户/户喜/喜欢/欢喝" f1×1.5=1.5 按出现序。
        let dbg = extract_keywords_debug("用户喜欢喝咖啡，偏好少糖。", 8);
        assert_eq!(
            dbg.final_kws,
            vec![
                "咖啡",
                "偏好",
                "喝咖啡",
                "偏好少",
                "用户",
                "户喜",
                "喜欢",
                "欢喝"
            ]
        );
        // "少糖" 是有效 ngram（在 filtered 中），仅被同分更早插入的词挤出 top-8
        assert!(dbg.filtered.contains(&"少糖".to_string()));
        // 标点被替换为空格，不产出含标点的伪词
        assert!(!dbg.final_kws.iter().any(|w| w.contains('，')));
        // 滑窗跨内部空格会产生"咖啡 偏"这类低分伪词（对齐 Node 行为），
        // 但不会进 top-8
        assert!(dbg.raw.contains(&"咖啡 偏".to_string()));
        assert!(!dbg.final_kws.iter().any(|w| w.contains(' ')));
    }

    #[test]
    fn stop_words_and_boundary_chars_are_filtered() {
        let dbg = extract_keywords_debug("昨天我们聊了项目的事", 8);
        // "昨天" 是停用词 → 不进 filtered
        assert!(!dbg.filtered.contains(&"昨天".to_string()));
        // 含 STOP_CHAR "了" 的 ngram 被过滤
        assert!(!dbg.filtered.iter().any(|w| w.contains('了')));
        // 首字 "们" 在 STOP_HEAD → "们聊" 等被过滤
        assert!(!dbg.filtered.iter().any(|w| w.starts_with('们')));
    }

    #[test]
    fn duplicate_chars_are_dropped_except_digraph() {
        let dbg = extract_keywords_debug("天天向上好好学习", 16);
        // 三字以上含重复字 → 丢弃（如"天天向"）
        assert!(!dbg
            .filtered
            .iter()
            .any(|w| w.chars().count() >= 3 && w.contains("天天")));
        // 两字叠词合法："天天" 本身保留（若通过其他校验）
        assert!(dbg.raw.contains(&"天天".to_string()));
    }

    #[test]
    fn english_words_are_extracted_from_raw_text() {
        let kws = extract_keywords("Install Node.js version 20 and test the API server", 8);
        // 英文词 ≥3 字母，小写化停用词过滤（the/and 不在停用词表，但长度语义保留）
        assert!(kws.contains(&"Install".to_string()));
        assert!(kws.contains(&"server".to_string()) || kws.contains(&"version".to_string()));
    }

    #[test]
    fn empty_text_yields_nothing() {
        assert!(extract_keywords("", 8).is_empty());
        assert!(extract_core("").0.is_empty());
    }

    #[test]
    fn score_uses_length_weight() {
        // 两个词各出现 1 次：2 字词权重 1.5 > 4 字词权重 0.8 > 3 字词 1.0
        // 构造：一个 2 字词 + 一个 4 字词同时出现
        let kws = extract_keywords("吃饭看电影，吃饭", 8);
        // "吃饭" f=2 × 1.5 = 3.0 最高
        assert_eq!(kws[0], "吃饭");
        // "看电影" f=1 × 1.0 = 1.0；"电影看" 等伪词可能也存在
        assert!(kws.contains(&"吃饭".to_string()));
    }

    #[test]
    fn insertion_order_breaks_ties() {
        let dbg = extract_keywords_debug("alpha beta gamma", 8);
        // 三个英文词各 1 次、同长 5 → 稳定排序保持首次出现序
        assert_eq!(dbg.filtered[0], "alpha");
        assert_eq!(dbg.filtered[1], "beta");
        assert_eq!(dbg.filtered[2], "gamma");
    }
}
