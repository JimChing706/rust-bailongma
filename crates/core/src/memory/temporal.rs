//! Temporal hint parser —— 把"今天/昨天/前天/大前天"等相对时间词解析成日期区间。
//!
//! 对齐 Node 版 `src/memory/temporal-parser.js`：
//! - 纯函数、零外部依赖，可在不连 db / llm 的环境下单测
//! - 只识别确定能算出区间的相对词，不命中比误命中好
//! - 输出 ISO 字符串带本地时区偏移，与 `conversations.timestamp` 格式一致

use chrono::{DateTime, Duration, FixedOffset, Local, NaiveDate, TimeZone};

/// 一条时间提示（对齐 parseTemporalHints 返回的 hint 对象）。
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalHint {
    /// 命中标签词（如 '昨天'）
    pub label: String,
    /// 区间起点（本地时区 ISO 字符串，含偏移）
    pub from: String,
    /// 区间终点（本地时区 ISO 字符串，含偏移；[from, to) 半开区间）
    pub to: String,
    /// 相对今天的天数（0=今天，-1=昨天）
    pub offset_days: i64,
}

/// 词表：只收"确定能算出日期"的高频词（对齐 PATTERNS）。
/// 模糊词（最近/这阵子/之前）不收；"明天/后天"也不收 —— 只回忆过去。
const PATTERNS: &[(&[&str], &str, i64)] = &[
    (
        &["今天", "今早", "今晨", "今夜", "今晚", "今儿", "今日"],
        "今天",
        0,
    ),
    (&["昨天", "昨晚", "昨夜", "昨儿", "昨日"], "昨天", -1),
    (&["前天"], "前天", -2),
    (&["大前天"], "大前天", -3),
];

/// 把本地时间格式化成带偏移的 ISO 字符串（对齐 isoLocal：YYYY-MM-DDTHH:MM:SS+08:00）。
fn iso_local(dt: DateTime<FixedOffset>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// 解析消息中的相对时间词，返回区间数组（对齐 parseTemporalHints）。
///
/// - 多个命中按 offset_days 从大到小排（最近的先）
/// - 同一个标签词只命中一次（多次出现合并）
/// - "大前天"优先匹配，避免被"前天"截断（最长匹配 + 消耗扫描）
pub fn parse_temporal_hints(text: &str, now: DateTime<Local>) -> Vec<TemporalHint> {
    if text.is_empty() {
        return Vec::new();
    }
    let today = start_of_day(now);

    // 长词先扫（对齐 sortedPatterns：按 max word len desc）
    let mut sorted: Vec<&(&[&str], &str, i64)> = PATTERNS.iter().collect();
    sorted.sort_by(|a, b| {
        let max_a = a.0.iter().map(|w| w.chars().count()).max().unwrap_or(0);
        let max_b = b.0.iter().map(|w| w.chars().count()).max().unwrap_or(0);
        max_b.cmp(&max_a)
    });

    let mut hits = Vec::new();
    let mut scratch = text.to_string();
    for (synonyms, label, offset_days) in sorted {
        if !synonyms.iter().any(|w| scratch.contains(w)) {
            continue;
        }
        // 消耗：把该模式所有同义词都从 scratch 清掉，避免短词从长词残骸再误匹配
        for w in *synonyms {
            scratch = scratch.split(w).collect::<Vec<_>>().join(" ");
        }
        let from = today
            .checked_add_signed(Duration::days(*offset_days))
            .expect("date arithmetic");
        let to = from
            .checked_add_signed(Duration::days(1))
            .expect("date arithmetic");
        hits.push(TemporalHint {
            label: (*label).to_string(),
            from: iso_local(from),
            to: iso_local(to),
            offset_days: *offset_days,
        });
    }

    // 输出按 offset_days desc 排（今天 0 > 昨天 -1 > 前天 -2 > 大前天 -3）
    hits.sort_by(|a, b| b.offset_days.cmp(&a.offset_days));
    hits
}

/// 所有时间标签词（含同义词），长词在前（对齐 ALL_TEMPORAL_WORDS）。
fn all_temporal_words() -> Vec<&'static str> {
    let mut words: Vec<&'static str> = Vec::new();
    for (synonyms, _, _) in PATTERNS {
        words.extend(synonyms.iter().copied());
    }
    words.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
    words
}

/// 从原文剥离已被解析的时间词，让后续 extractKeywords 不会切出含时间词的 ngram
/// （如"昨天我"），从而污染 FTS5 召回（对齐 stripTemporalWords）。
pub fn strip_temporal_words(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = text.to_string();
    for w in all_temporal_words() {
        out = out.split(w).collect::<Vec<_>>().join(" ");
    }
    out
}

/// 取某天的 00:00:00（本地时区）。
fn start_of_day(now: DateTime<Local>) -> DateTime<FixedOffset> {
    let date: NaiveDate = now.date_naive();
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight is always valid");
    let offset = *now.offset();
    Local
        .from_local_datetime(&midnight)
        .single()
        .expect("local midnight exists")
        .with_timezone(&offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[test]
    fn parses_yesterday_and_day_before() {
        let now = Local.with_ymd_and_hms(2026, 4, 13, 10, 0, 0).unwrap();
        let hints = parse_temporal_hints("昨天和前天的事", now);
        assert_eq!(hints.len(), 2);
        // 最近的在先：昨天(-1) > 前天(-2)
        assert_eq!(hints[0].label, "昨天");
        assert_eq!(hints[1].label, "前天");
        assert_eq!(hints[0].offset_days, -1);
        assert_eq!(hints[1].offset_days, -2);
        // 昨天 from = 04-12 00:00:00
        assert!(hints[0].from.starts_with("2026-04-12T00:00:00"));
        assert!(hints[0].to.starts_with("2026-04-13T00:00:00"));
        // 前天 from = 04-11
        assert!(hints[1].from.starts_with("2026-04-11T00:00:00"));
    }

    #[test]
    fn longest_match_consumes_synonyms() {
        let now = Local.with_ymd_and_hms(2026, 4, 13, 10, 0, 0).unwrap();
        // "大前天" 优先匹配，不残留"前天"残骸
        let hints = parse_temporal_hints("大前天的事", now);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].label, "大前天");
        assert_eq!(hints[0].offset_days, -3);
        // 连续出现 "前天和大前天" → 两个独立命中
        let hints = parse_temporal_hints("前天和大前天的事", now);
        assert_eq!(hints.len(), 2);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_str()).collect();
        assert_eq!(labels, vec!["前天", "大前天"]);
    }

    #[test]
    fn duplicate_label_merges() {
        let now = Local.with_ymd_and_hms(2026, 4, 13, 10, 0, 0).unwrap();
        let hints = parse_temporal_hints("昨天买了咖啡，昨晚又买了一次", now);
        // "昨天" 与 "昨晚" 同属一个模式 → 只命中一次
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].label, "昨天");
    }

    #[test]
    fn strips_temporal_words() {
        assert_eq!(strip_temporal_words("昨天我们聊了什么"), " 我们聊了什么");
        // 长词先剥，避免"前天"截断"大前天"
        assert_eq!(strip_temporal_words("大前天聊的"), " 聊的");
        assert_eq!(strip_temporal_words("没有时间词"), "没有时间词");
    }

    #[test]
    fn empty_or_ambiguous_returns_empty() {
        let now = Local.with_ymd_and_hms(2026, 4, 13, 10, 0, 0).unwrap();
        assert!(parse_temporal_hints("", now).is_empty());
        // 模糊词不收
        assert!(parse_temporal_hints("最近怎么样", now).is_empty());
        assert!(parse_temporal_hints("明天见", now).is_empty());
    }

    #[test]
    fn iso_local_has_offset() {
        let now = Local.with_ymd_and_hms(2026, 4, 13, 10, 0, 0).unwrap();
        let hints = parse_temporal_hints("今天", now);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].label, "今天");
        // 带时区偏移（形如 +08:00 / +00:00 / -07:00）
        assert!(hints[0].from.contains('+') || hints[0].from.contains('-'));
        assert!(hints[0].from.len() >= 19 + 6);
    }
}
