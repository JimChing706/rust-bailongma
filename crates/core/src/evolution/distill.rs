//! 语料蒸馏（P4 · DL/NN 蒸馏的地面层）。
//!
//! 评审裁决（E4 立场 + R9 缓解）：**只蒸馏确定性子问题**——意图分类 / 敏感检测 /
//! 嵌入文本块；以**规则层标注为 ground truth 冷启动**，不做端到端模型训练。
//!
//! 数据源：`conversations` 表（M1 起自然增长 + 预留 `label` 列，评审 §5.2 修订 9）。
//! 蒸馏输出：
//! - 内存 `Vec<DistillEntry>`（可直接喂后续分类器/嵌入管线）
//! - JSONL 文件（`export_jsonl`，逐行 JSON，可版本化入库）
//! - 标注写回 `conversations.label`（`write_labels_back`，零成本积累闭环：
//!   语料随对话自然增长，标注即落库，蒸馏工程 P4 启动时已有标注历史）
//!
//! 回归门槛（验收）：规则标注器对构造样本 100% 复现（ground truth 单测锁定），
//! 任何规则改动必须同步改测试，防止标注漂移。

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::Result;

/// 蒸馏来源标识：规则层 v1。
pub const DISTILL_SOURCE_RULE_V1: &str = "rule_v1";

/// 一条蒸馏语料。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DistillEntry {
    /// conversations.id。
    pub id: i64,
    /// 消息角色（user/assistant/…）。
    pub role: String,
    /// 消息原文。
    pub content: String,
    /// 确定性意图标签（rule_intent 输出）。
    pub intent: String,
    /// 是否命中敏感形态（密钥/token/手机号/身份证/JWT 等）。
    pub sensitive: bool,
    /// 是否可作为嵌入文本块（长度达标且非敏感）。
    pub embeddable: bool,
    /// 标注来源（ground truth 冷启动：rule_v1）。
    pub source: String,
}

/// 确定性意图标签（只保留可被规则 100% 判定的子问题，R9「不蒸馏模糊语义」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentLabel {
    /// 祈使命令（跑/执行/打开/继续/停止/删除/更新/推送/提交/写/建/改/修…）
    Command,
    /// 疑问（问号结尾或疑问词）
    Query,
    /// 提醒类（提醒/记得/别忘/remind…）
    Reminder,
    /// 规划类（计划/规划/下一步/里程碑/方案…）
    Plan,
    /// 其余（闲聊/陈述/上下文补充）
    Chat,
}

impl IntentLabel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Query => "query",
            Self::Reminder => "reminder",
            Self::Plan => "plan",
            Self::Chat => "chat",
        }
    }
}

/// 规则层意图标注（判定顺序：Reminder → Plan → Query → Command → Chat）。
/// 每个分支的样本都锁在 `tests::rule_intent_ground_truth` 里（回归门槛）。
pub fn rule_intent(content: &str) -> IntentLabel {
    let c = content.trim();
    // 提醒类：最强信号，优先
    for kw in ["提醒", "记得", "别忘", "别忘了", "remind", "别忘了提醒"] {
        if c.contains(kw) {
            return IntentLabel::Reminder;
        }
    }
    // 规划类
    for kw in [
        "计划",
        "规划",
        "下一步",
        "路线图",
        "里程碑",
        "方案",
        "roadmap",
        "milestone",
    ] {
        if c.contains(kw) {
            return IntentLabel::Plan;
        }
    }
    // 疑问类：问号结尾或疑问词
    if c.ends_with('?') || c.ends_with('？') {
        return IntentLabel::Query;
    }
    for w in [
        "什么",
        "怎么",
        "为什么",
        "如何",
        "哪个",
        "多少",
        "是不是",
        "能否",
        "吗",
        "呢",
    ] {
        if c.contains(w) {
            return IntentLabel::Query;
        }
    }
    // 命令类：祈使开头
    for kw in [
        "跑", "执行", "做", "打开", "继续", "停止", "删除", "更新", "推送", "提交", "查", "写",
        "建", "改", "修", "看", "run", "exec", "open", "stop",
    ] {
        if c.starts_with(kw) || c.starts_with(&format!("{kw} ")) {
            return IntentLabel::Command;
        }
    }
    IntentLabel::Chat
}

/// 敏感形态正则（保守集：只含可确定判定的模式，宁缺毋滥）。
const SENSITIVE_PATTERNS: &[&str] = &[
    r"sk-[A-Za-z0-9]{16,}",        // OpenAI 风格密钥
    r"AKIA[0-9A-Z]{16}",           // AWS access key
    r"gh[pousr]_[A-Za-z0-9]{20,}", // GitHub token
    r"-----BEGIN [A-Z ]*-----",    // PEM 私钥
    r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}", // JWT
    r"1[3-9]\d{9}",                // 大陆手机号
    r"\d{17}[\dXx]",               // 18 位身份证
];

fn sensitive_regexes() -> &'static Vec<Regex> {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        SENSITIVE_PATTERNS
            .iter()
            .map(|p| Regex::new(p).expect("敏感正则必须合法"))
            .collect()
    })
}

/// 敏感检测：命中任一保守模式即标记（蒸馏时不嵌入敏感内容）。
pub fn detect_sensitive(content: &str) -> bool {
    sensitive_regexes().iter().any(|re| re.is_match(content))
}

/// 从 conversations 蒸馏确定性子问题。
///
/// - 过滤：内容 trim 后长度 < `min_len` 的丢弃；
/// - 嵌入判定：非敏感 且 字符数 ∈ [20, 2000]；
/// - 只读不改写（标注写回由 `write_labels_back` 显式调用）。
pub fn distill_from_conversations(
    db: &Db,
    limit: usize,
    min_len: usize,
) -> Result<Vec<DistillEntry>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, role, content, COALESCE(label, '') FROM conversations ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id, role, content, _existing_label) = row?;
        if content.trim().chars().count() < min_len {
            continue;
        }
        let intent = rule_intent(&content);
        let sensitive = detect_sensitive(&content);
        let len = content.chars().count();
        let embeddable = !sensitive && (20..=2000).contains(&len);
        out.push(DistillEntry {
            id,
            role,
            content,
            intent: intent.as_str().to_string(),
            sensitive,
            embeddable,
            source: DISTILL_SOURCE_RULE_V1.to_string(),
        });
    }
    Ok(out)
}

/// 导出 JSONL（每行一个 JSON 对象，UTF-8）。返回写入条数。
pub fn export_jsonl(entries: &[DistillEntry], path: &Path) -> Result<usize> {
    let mut out = String::new();
    for e in entries {
        out.push_str(&serde_json::to_string(e)?);
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(entries.len())
}

/// 标注写回 conversations.label（零成本积累闭环）。
///
/// 格式：`intent:<标签>|sensitive:<bool>|embed:<bool>|src:rule_v1`。
/// 只写空 label 的行（已有标注不覆盖，保持标注可追溯）。
pub fn write_labels_back(db: &Db, entries: &[DistillEntry]) -> Result<usize> {
    let conn = db.conn();
    let mut written = 0usize;
    for e in entries {
        let label = format!(
            "intent:{}|sensitive:{}|embed:{}|src:{}",
            e.intent, e.sensitive, e.embeddable, e.source
        );
        let n = conn.execute(
            "UPDATE conversations SET label = ?1 WHERE id = ?2 AND label = ''",
            rusqlite::params![label, e.id],
        )?;
        written += n as usize;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::conversations::insert;

    fn test_db() -> Db {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("blm_distill_test_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        Db::open(dir.join("test.db")).unwrap()
    }

    /// 回归门槛：规则层 ground truth 100% 复现（改动规则必须同步改这里）。
    #[test]
    fn rule_intent_ground_truth() {
        // Reminder
        assert_eq!(rule_intent("明天记得提醒我开会"), IntentLabel::Reminder);
        assert_eq!(rule_intent("别忘了吃药"), IntentLabel::Reminder);
        assert_eq!(rule_intent("remind me to commit"), IntentLabel::Reminder);
        // Plan
        assert_eq!(rule_intent("下一步计划是推进 P4"), IntentLabel::Plan);
        assert_eq!(rule_intent("这个里程碑的方案你怎么看"), IntentLabel::Plan);
        // Query
        assert_eq!(rule_intent("今天大盘怎么样？"), IntentLabel::Query);
        assert_eq!(rule_intent("怎么判断收敛"), IntentLabel::Query);
        assert_eq!(rule_intent("what is the status?"), IntentLabel::Query);
        // Command
        assert_eq!(rule_intent("跑一下测试"), IntentLabel::Command);
        assert_eq!(rule_intent("推送代码"), IntentLabel::Command);
        assert_eq!(rule_intent("打开文档"), IntentLabel::Command);
        // Chat
        assert_eq!(rule_intent("今天天气不错"), IntentLabel::Chat);
        assert_eq!(rule_intent("好的"), IntentLabel::Chat);
    }

    #[test]
    fn sensitive_detection_ground_truth() {
        assert!(detect_sensitive("key is sk-abc1234567890abcdefghijklmnop"));
        assert!(detect_sensitive("AKIAIOSFODNN7EXAMPLE 是我的密钥"));
        assert!(detect_sensitive(
            "token: ghp_abcdefghijklmnopqrstuvwxyz123456"
        ));
        assert!(detect_sensitive("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(detect_sensitive("JWT: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"));
        assert!(detect_sensitive("电话 13812345678 联系"));
        assert!(detect_sensitive("身份证 110101199003071234"));
        assert!(!detect_sensitive("普通聊天内容，没有任何敏感形态"));
    }

    #[test]
    fn distill_filters_and_labels() {
        let db = test_db();
        insert(&db, "user", "ID:000001", "跑一下测试").unwrap(); // command, 短(4字) → min_len=3 通过
        insert(
            &db,
            "user",
            "ID:000001",
            "今天天气怎么样？明天会下雨吗，我需要带伞吗？",
        )
        .unwrap(); // query (>20 字符)
        insert(
            &db,
            "user",
            "ID:000001",
            "密钥是 sk-abcdefghijklmnopqrstuvwx1234567890",
        )
        .unwrap(); // sensitive
        insert(&db, "assistant", "jarvis", "hi").unwrap(); // 太短 → 过滤

        let entries = distill_from_conversations(&db, 100, 3).unwrap();
        let by_id: std::collections::HashMap<i64, &DistillEntry> =
            entries.iter().map(|e| (e.id, e)).collect();

        // 短消息 "hi" 被过滤（4 条插入，3 条输出）
        assert_eq!(entries.len(), 3);
        for e in entries.iter() {
            assert_eq!(e.source, DISTILL_SOURCE_RULE_V1);
        }
        // 按内容找
        let cmd = by_id
            .values()
            .find(|e| e.content.contains("跑一下"))
            .unwrap();
        assert_eq!(cmd.intent, "command");
        assert!(!cmd.sensitive);
        let q = by_id.values().find(|e| e.content.contains("天气")).unwrap();
        assert_eq!(q.intent, "query");
        assert!(q.embeddable);
        let s = by_id.values().find(|e| e.content.contains("sk-")).unwrap();
        assert!(s.sensitive);
        assert!(!s.embeddable, "敏感内容不可嵌入");
    }

    #[test]
    fn export_jsonl_roundtrip() {
        let db = test_db();
        insert(&db, "user", "ID:000001", "明天记得提醒我买咖啡").unwrap();
        let entries = distill_from_conversations(&db, 10, 2).unwrap();
        let path =
            std::env::temp_dir().join(format!("blm_distill_out_{}.jsonl", std::process::id()));
        let n = export_jsonl(&entries, &path).unwrap();
        assert_eq!(n, entries.len());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), entries.len());
        // 每行都能反序列化
        for line in text.lines() {
            let e: DistillEntry = serde_json::from_str(line).unwrap();
            assert_eq!(e.source, DISTILL_SOURCE_RULE_V1);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn write_labels_back_only_fills_empty() {
        let db = test_db();
        let id1 = insert(&db, "user", "ID:000001", "明天记得提醒我开会").unwrap();
        let id2 = insert(&db, "user", "ID:000001", "推送代码").unwrap();
        // 预置一条已有 label 的消息
        db.conn()
            .execute(
                "UPDATE conversations SET label='intent:reminder|src:manual' WHERE id=?1",
                [id1],
            )
            .unwrap();

        let entries = distill_from_conversations(&db, 10, 2).unwrap();
        let written = write_labels_back(&db, &entries).unwrap();

        // id1 已有 label → 不覆盖；id2 空 → 写入
        assert_eq!(written, 1);
        let l1: String = db
            .conn()
            .query_row("SELECT label FROM conversations WHERE id=?1", [id1], |r| {
                r.get(0)
            })
            .unwrap();
        let l2: String = db
            .conn()
            .query_row("SELECT label FROM conversations WHERE id=?1", [id2], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(l1, "intent:reminder|src:manual");
        assert!(l2.starts_with("intent:command|"));
    }
}
