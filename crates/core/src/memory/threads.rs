//! Thread Model —— 动态上下文记忆池第 8 章：专注栈（focus.js）的继任者。
//!
//! 对齐 Node 版 `src/memory/threads.js`。三条修正：
//! - 认识论：焦点由行动者写入（open_commitment / touch），不靠旁观者猜。
//! - 本体论：多条并发线索 + 一个前台指针，没有栈、没有 pop——前台切走线索只是去后台。
//! - 决策论：遗忘是读时纯函数（thread_temperature），不做写时状态突变；线索数据只增不删。
//!
//! 不在本模块的职责：LLM 调用（摘要在 thread-summarize，归属仲裁在 thread-classifier，
//! 本模块只产出事件）、prompt 渲染（按 thread_temperature 选粒度）。
//!
//! 持久化接缝：内存状态变更由上层调 `db::repositories::threads::save_state` 落库；
//! 本模块提供 `init_thread_state`（对齐 index.js `initThreadState`）负责启动恢复与
//! focus_stack 一次性迁移（见下方 `persist` 小节）。
//!
//! 与 focus.js 同款约束：直接从 [`keywords`] 拿关键词；除 `init_thread_state` 外保持纯逻辑。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;

use crate::db::models::Thread;
use crate::db::repositories::threads::{
    load_focus_stack, load_thread_state, save_state, Commitment, FocusFrame, ThreadState,
};
use crate::db::Db;
use crate::error::Result;

use super::keywords::extract_keywords;

// ── 温度窗口（墙钟时间，不是 tick——tick 间隔在任务/空闲模式下差 40 倍，不可作时间单位） ──
/// 6h 内活跃 → warm
pub const WARM_WINDOW_MS: i64 = 6 * 60 * 60 * 1000;
/// 48h 内 → cool；更久 → cold
pub const COOL_WINDOW_MS: i64 = 48 * 60 * 60 * 1000;

/// 注入端配额：warm 线索一行摘要最多注入几条（少即是强约束的是注入结果，不是数据存亡）
pub const MAX_WARM_INJECTED: usize = 3;

/// 内存中保留的线索上限。超限时把最冷的「已关闭且无开放承诺」线索移出内存（db 里仍在）。
pub const MAX_THREADS_IN_MEMORY: usize = 12;

/// 单线索 conclusions 滚动上限（与专注栈时代一致）
pub const THREAD_CONCLUSIONS_LIMIT: usize = 5;

/// topic 关键词数量上限 / 抽取预算（沿用专注栈的标定）
const TOPIC_KEYWORDS_LIMIT: usize = 3;
const KEYWORD_EXTRACT_BUDGET: usize = 12;
const MIN_KEYWORDS_FOR_THREAD: usize = 3;
const MIN_MESSAGE_LENGTH: usize = 4;

/// 线索"签名"：用于重叠匹配的关键词集合，比展示 topic 宽（提高字面匹配召回）。
const SIGNATURE_LIMIT: usize = 8;

/// 切换门槛不对称：前台续命是廉价操作 → 重叠 ≥1 即 continued；
/// 后台切换是昂贵操作 → 重叠 ≥2 才 resumed（专注栈时代单关键词误 returned 的教训）。
const FOREGROUND_OVERLAP_MIN: usize = 1;
const BACKGROUND_RESUME_OVERLAP_MIN: usize = 2;

// ── 正则（对齐 threads.js 词表） ────────────────────────────────────────────

static NOISE_TOKEN_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static ONE_OFF_LEAF_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static SUSTAINED_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static HELLO_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static INDEXICAL_PROGRESS_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static GENERIC_OBJECT_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static OPERATION_VERB_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static DEMONSTRATIVE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
static ENVELOPE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

fn noise_token_re() -> &'static Regex {
    NOISE_TOKEN_RE.get_or_init(|| {
        Regex::new(
            r"^(这个|那个|什么|怎么|为什|可以|我们|你们|他们|帮我|给我|一下|一个|继续|部分|现在|今天|明天|昨天|晚上|早上|然后|还是|就是|但是|因为|所以|如果|这样|那样|的话|时候|问题|事情|东西|网页|网站|网址|页面|链接|地址|文件|文档|内容)",
        ).expect("static regex")
    })
}

fn one_off_leaf_re() -> &'static Regex {
    ONE_OFF_LEAF_RE.get_or_init(|| {
        Regex::new(r"(?i)天气|气温|温度|下雨|下雪|空气质量|AQI|几点|几号|星期几|汇率|股价|热搜|新闻|在吗|早上好|晚上好|谢谢|收到")
            .expect("static regex")
    })
}

fn sustained_re() -> &'static Regex {
    SUSTAINED_RE.get_or_init(|| {
        Regex::new(r"(?i)分析|优化|修复|实现|修改|设计|写|做|排查|调试|构建|部署|项目|代码|文件|机制|方案|测试|review|debug|fix|implement|build")
            .expect("static regex")
    })
}

fn hello_re() -> &'static Regex {
    HELLO_RE.get_or_init(|| {
        Regex::new(r"(?i)^(hello|hi|hey|在吗|早上好|晚上好|谢谢|收到)$").expect("static regex")
    })
}

fn indexical_progress_re() -> &'static Regex {
    INDEXICAL_PROGRESS_RE.get_or_init(|| {
        Regex::new(r"(怎么样|咋样|如何了|进度|进展|搞定|好了吗|好了么|好了没|完成了吗|完成了没|弄完|做完|干完|干得|干的|还在弄|还在做|顺利|卡住|到哪|哪一步)")
            .expect("static regex")
    })
}

fn generic_object_re() -> &'static Regex {
    GENERIC_OBJECT_RE.get_or_init(|| {
        Regex::new(r"(网页|网站|网址|页面|那页|这页|链接|地址|玩意儿?|东西|文件|文档|内容)")
            .expect("static regex")
    })
}

fn operation_verb_re() -> &'static Regex {
    OPERATION_VERB_RE.get_or_init(|| {
        Regex::new(r"(打开|关闭|关掉|启动|运行|播放|暂停|下载|搜索|显示|发送|点开|访问|跳转|打开一?下|放一?下|查一?下|搜一?下|看一?下|念一?下|读一?下|发一?下)")
            .expect("static regex")
    })
}

fn demonstrative_re() -> &'static Regex {
    DEMONSTRATIVE_RE.get_or_init(|| {
        Regex::new(
            r"(这|那)(个|种|些|位|款|家|件|批|类|段|张|篇|份|首|部|台|项|套|条|者|回|次)|它|他|她|刚才|刚刚|刚说|刚提|上面|前面|之前(说|讲|提|的|那)|你?刚(说|讲|发|放|提)",
        )
        .expect("static regex")
    })
}

fn envelope_re() -> &'static Regex {
    ENVELOPE_RE.get_or_init(|| {
        // 对齐 Node stripMessageEnvelope（focus.js）：`[\d\-T:+]+` 兼容
        // `2026-04-13-10:00:00`（queue.js）与 `2026-04-11T15:32:00+08:00`（time.js nowTimestamp）
        Regex::new(r"(?s)^\[[^\]]+\]\s*[\d\-T:+]+\s*\[[^\]]*\]\s*(.*)$").expect("static regex")
    })
}

// ── 时间工具（对齐 Node Date.now() / Date.parse / toISOString） ─────────────

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 解析 ISO 时间戳为 epoch 毫秒（兼容 UTC 的 `Z` 与带偏移格式；失败 → None）。
fn parse_ts_ms(ts: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(nd) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S") {
        return Some(nd.and_utc().timestamp_millis());
    }
    None
}

/// UTC ISO 毫秒（对齐 new Date().toISOString()）。
fn now_iso_utc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 数字 → base36（对齐 Date.now().toString(36)）。
fn base36(mut v: u64) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if v == 0 {
        return "0".into();
    }
    let mut s = String::new();
    while v > 0 {
        s.insert(0, CHARS[(v % 36) as usize] as char);
        v /= 36;
    }
    s
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成 `prefix_<ts36>_<counter36><rand4>`（对齐 newId）。
fn new_id(prefix: &str) -> String {
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts36 = base36(now_ms().max(0) as u64);
    let c36 = base36(counter % 10000);
    let rand = {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(counter.wrapping_mul(0x9E3779B97F4A7C15));
        h.write_u64(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0) as u64,
        );
        format!("{:0>4}", base36(h.finish() % 1_679_616))
    };
    format!("{prefix}_{ts36}_{c36}{rand}")
}

// ── 关键词：噪声过滤 / 归属抽取（消息侧与线索签名侧同源） ───────────────────

/// 过滤功能词碎片与超短英文/数字词（对齐 filterNoiseTokens）。
pub fn filter_noise_tokens(kws: &[String]) -> Vec<String> {
    kws.iter()
        .filter(|k| {
            let t = k.trim();
            if t.is_empty() {
                return false;
            }
            if t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return t.chars().count() >= 3; // 英文/数字词：太短的丢掉
            }
            if noise_token_re().is_match(t) {
                return false;
            }
            t.chars().count() >= 2
        })
        .cloned()
        .collect()
}

/// 抽取用于归属判定的关键词（统一入口，对齐 extractAttributionKeywords）。
pub fn extract_attribution_keywords(text: &str) -> Vec<String> {
    filter_noise_tokens(&extract_keywords(text, KEYWORD_EXTRACT_BUDGET))
}

// ── 一次性叶子 / 指代性问询 ─────────────────────────────────────────────────

/// 明显的一次性叶子查询（对齐 isLikelyOneOffLeaf）：不该开线索的消息。
pub fn is_likely_one_off_leaf(body: &str) -> bool {
    let text = body.trim();
    if text.is_empty() {
        return false;
    }
    if sustained_re().is_match(text) {
        return false;
    }
    if hello_re().is_match(text) {
        return true;
    }
    text.chars().count() <= 40 && one_off_leaf_re().is_match(text)
}

/// 指代性进度问询（对齐 isIndexicalProgressQuery）：设计上不含主题词，靠句式识别。
pub fn is_indexical_progress_query(body: &str) -> bool {
    let text = body.trim();
    if text.is_empty() || text.chars().count() > 25 {
        return false;
    }
    indexical_progress_re().is_match(text)
}

// ── 指代-就近 / 精确回指分类（对齐 classifyReference） ─────────────────────

/// 指代分类结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceClass {
    pub kind: ReferenceKind,
    /// 句子剥掉操作动词/泛称后剩的实质话题词
    pub substantive: Vec<String>,
    /// precise-callback 时的精确指代词（= substantive）
    pub referent_kws: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    None,
    AnaphoraRecent,
    PreciseCallback,
}

/// 把一条用户消息分类为 none / anaphora-recent / precise-callback（对齐 classifyReference）。
pub fn classify_reference(body: &str) -> ReferenceClass {
    let text = body.trim();
    if text.is_empty() {
        return ReferenceClass {
            kind: ReferenceKind::None,
            substantive: Vec::new(),
            referent_kws: Vec::new(),
        };
    }
    let has_demon = demonstrative_re().is_match(text);
    let acts_on_generic =
        generic_object_re().is_match(text) && (operation_verb_re().is_match(text) || has_demon);
    let substantive: Vec<String> = extract_attribution_keywords(text)
        .into_iter()
        .filter(|k| !operation_verb_re().is_match(k) && !generic_object_re().is_match(k))
        .collect();
    let bare_operation = operation_verb_re().is_match(text) && substantive.is_empty();
    if acts_on_generic || bare_operation {
        return ReferenceClass {
            kind: ReferenceKind::AnaphoraRecent,
            substantive,
            referent_kws: Vec::new(),
        };
    }
    if has_demon && substantive.is_empty() {
        return ReferenceClass {
            kind: ReferenceKind::AnaphoraRecent,
            substantive,
            referent_kws: Vec::new(),
        };
    }
    if has_demon && !substantive.is_empty() {
        return ReferenceClass {
            kind: ReferenceKind::PreciseCallback,
            referent_kws: substantive.clone(),
            substantive,
        };
    }
    ReferenceClass {
        kind: ReferenceKind::None,
        substantive,
        referent_kws: Vec::new(),
    }
}

// ── 消息信封剥离 ────────────────────────────────────────────────────────────

fn is_tick_message(message: &str) -> bool {
    let t = message.trim();
    let mut chars = t.chars();
    let head: String = chars.by_ref().take(4).collect();
    head.eq_ignore_ascii_case("TICK") && chars.next().is_some_and(char::is_whitespace)
}

/// 与 focus.js 同款信封剥离：`[ID:xxx] 时间戳 [渠道] 正文`（对齐 stripMessageEnvelope）。
pub fn strip_message_envelope(message: &str) -> String {
    if message.is_empty() {
        return String::new();
    }
    if is_tick_message(message) {
        return String::new();
    }
    match envelope_re().captures(message) {
        Some(c) => c
            .get(1)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default(),
        None => message.trim().to_string(),
    }
}

// ── 线索工厂 ────────────────────────────────────────────────────────────────

/// 新建线索（对齐 makeThread）。
pub fn make_thread(
    topic: &[String],
    tick: i64,
    label: &str,
    signature: Option<&[String]>,
) -> Thread {
    let now = now_iso_utc();
    let topic_arr: Vec<String> = topic.iter().take(TOPIC_KEYWORDS_LIMIT).cloned().collect();
    let sig: Vec<String> = match signature {
        Some(s) if !s.is_empty() => s.iter().take(SIGNATURE_LIMIT).cloned().collect(),
        _ => topic_arr.clone(),
    };
    Thread {
        id: new_id("th"),
        topic: topic_arr,
        signature: sig,
        label: label.to_string(),
        summary: String::new(),
        conclusions: Vec::new(),
        status: "open".into(),
        created_at: now.clone(),
        last_event_at: now,
        last_event_tick: tick,
        hit_count: 1,
        last_summary_at: String::new(),
        updated_at: String::new(),
    }
}

// ── ThreadState 访问器 ──────────────────────────────────────────────────────

/// 前台线索（对齐 getForegroundThread）。
pub fn get_foreground_thread(ts: &ThreadState) -> Option<&Thread> {
    let fg = ts.foreground_id.as_ref()?;
    ts.threads.iter().find(|t| &t.id == fg)
}

/// 按 id 查线索（对齐 getThreadById）。
pub fn get_thread_by_id<'a>(ts: &'a ThreadState, id: &str) -> Option<&'a Thread> {
    if id.is_empty() {
        return None;
    }
    ts.threads.iter().find(|t| t.id == id)
}

/// 开放承诺列表（对齐 getOpenCommitments）。
pub fn get_open_commitments(ts: &ThreadState) -> Vec<&Commitment> {
    ts.commitments
        .iter()
        .filter(|c| c.status == "open")
        .collect()
}

fn newest_open(list: Vec<&Commitment>) -> Option<&Commitment> {
    let mut best: Option<&Commitment> = None;
    let mut best_ts: i64 = i64::MIN;
    for c in list {
        let ts_v = parse_ts_ms(&c.created_at).unwrap_or(0);
        if ts_v >= best_ts {
            best = Some(c);
            best_ts = ts_v;
        }
    }
    best
}

/// 最近的开放承诺（指代性问询的解析锚点）。channel 给了就优先同渠道；
/// 同毫秒创建的承诺按数组序（创建序）后者胜（对齐 latestOpenCommitment）。
pub fn latest_open_commitment<'a>(ts: &'a ThreadState, channel: &str) -> Option<&'a Commitment> {
    let open: Vec<&Commitment> = ts
        .commitments
        .iter()
        .filter(|c| c.status == "open")
        .collect();
    if open.is_empty() {
        return None;
    }
    if !channel.is_empty() {
        let same: Vec<&Commitment> = open
            .iter()
            .copied()
            .filter(|c| c.channel == channel)
            .collect();
        if !same.is_empty() {
            return newest_open(same);
        }
    }
    newest_open(open)
}

// ── 承诺生命周期（行动者写入路径之一：set_task / clear_task 钩子调这里） ────

/// M5（审计修复）：commitments 容量上限——closed 承诺（done/cancelled）按
/// 创建序淘汰最老项，open（活动）承诺永不淘汰。阈值远高于真实长会话用量，
/// 仅防无界累积。
fn enforce_commitments_cap(ts: &mut ThreadState) {
    const COMMITMENTS_CAP: usize = 256;
    if ts.commitments.len() <= COMMITMENTS_CAP {
        return;
    }
    let overflow = ts.commitments.len() - COMMITMENTS_CAP;
    let mut removed: Vec<usize> = Vec::new();
    for (i, c) in ts.commitments.iter().enumerate() {
        if removed.len() >= overflow {
            break;
        }
        if c.status != "open" {
            removed.push(i);
        }
    }
    if removed.is_empty() {
        return; // 全为 open（理论不会：open 单例）→ 保留
    }
    let mut keep = Vec::with_capacity(ts.commitments.len() - removed.len());
    for (i, c) in ts.commitments.drain(..).enumerate() {
        if !removed.contains(&i) {
            keep.push(c);
        }
    }
    ts.commitments = keep;
}

/// "好的我去做" = 开承诺，钉住线索温度。thread_id 缺省挂到前台线索；
/// 前台为空就为这个承诺开一条新线索（对齐 openCommitment）。
pub fn open_commitment<'a>(
    ts: &'a mut ThreadState,
    text: &str,
    thread_id: Option<&str>,
    channel: &str,
    tick: i64,
) -> &'a mut Commitment {
    let thread = match thread_id {
        Some(id) => ts.threads.iter().position(|t| t.id == id),
        None => ts
            .foreground_id
            .as_ref()
            .and_then(|f| ts.threads.iter().position(|t| &t.id == f)),
    };
    let thread_idx = match thread {
        Some(i) => i,
        None => {
            // 前台为空 → 为承诺开新线索
            let kws = extract_attribution_keywords(text);
            let topic: Vec<String> = if kws.is_empty() {
                vec!["任务".to_string()]
            } else {
                kws.iter().take(TOPIC_KEYWORDS_LIMIT).cloned().collect()
            };
            let t = make_thread(&topic, tick, "", Some(&kws));
            ts.threads.push(t);
            let idx = ts.threads.len() - 1;
            ts.foreground_id = Some(ts.threads[idx].id.clone());
            idx
        }
    };
    let thread_id = ts.threads[thread_idx].id.clone();
    // M5（审计修复）：commitments 有界——closed（done/cancelled）承诺无限累积
    // 会造成长会话内存膨胀；超上限时按创建序淘汰最老已关闭项，open（活动）
    // 承诺是单例且钉住线索，永不淘汰。
    enforce_commitments_cap(ts);
    {
        // 同一线索上已有开放承诺 → 更新文本而不是叠加（task 是单例的）
        if let Some(existing) = ts
            .commitments
            .iter_mut()
            .find(|c| c.status == "open" && c.thread_id == thread_id)
        {
            existing.text = text.to_string();
        } else {
            let commitment = Commitment {
                id: new_id("cm"),
                thread_id: thread_id.clone(),
                text: text.to_string(),
                status: "open".into(),
                channel: channel.to_string(),
                created_at: now_iso_utc(),
                closed_at: None,
            };
            ts.commitments.push(commitment);
        }
    }
    touch_thread(ts, &thread_id, tick);
    ts.commitments
        .iter_mut()
        .find(|c| c.status == "open" && c.thread_id == thread_id)
        .unwrap()
}

/// 交差/取消。承诺关闭后线索不再被钉住，按 last_event_at 自然降温（对齐 closeCommitment）。
pub fn close_commitment<'a>(
    ts: &'a mut ThreadState,
    thread_id: Option<&str>,
    commitment_id: Option<&str>,
    status: &str,
) -> Option<&'a mut Commitment> {
    let target = match commitment_id {
        Some(cid) => ts
            .commitments
            .iter_mut()
            .find(|c| c.id == cid && c.status == "open"),
        None => ts.commitments.iter_mut().find(|c| {
            c.status == "open" && (thread_id.is_none() || Some(c.thread_id.as_str()) == thread_id)
        }),
    }?;
    target.status = if status == "cancelled" {
        "cancelled"
    } else {
        "done"
    }
    .into();
    target.closed_at = Some(now_iso_utc());
    Some(target)
}

// ── 行动者写入路径之二：Agent 干活就是注意力事件 ────────────────────────────

/// 给线索一个注意力事件（对齐 touchThread）：刷新时间、命中计数。
pub fn touch_thread(ts: &mut ThreadState, thread_id: &str, tick: i64) -> bool {
    let Some(thread) = ts.threads.iter_mut().find(|t| t.id == thread_id) else {
        return false;
    };
    thread.last_event_at = now_iso_utc();
    thread.last_event_tick = tick;
    thread.hit_count += 1;
    true
}

/// touch 最近开放承诺的线索；无承诺则 touch 前台（对齐 touchCommitmentThread）。
pub fn touch_commitment_thread(ts: &mut ThreadState, tick: i64) -> bool {
    let target_id = match latest_open_commitment(ts, "") {
        Some(c) => c.thread_id.clone(),
        None => match &ts.foreground_id {
            Some(f) => f.clone(),
            None => return false,
        },
    };
    touch_thread(ts, &target_id, tick)
}

// ── 读时温度函数（注入粒度由这里决定，永不写回） ───────────────────────────

/// 线索温度（对齐 threadTemperature）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temperature {
    Foreground,
    Warm,
    Cool,
    Cold,
}

pub fn thread_temperature(ts: &ThreadState, thread: &Thread, now: i64) -> Temperature {
    if ts.foreground_id.as_deref() == Some(thread.id.as_str()) {
        return Temperature::Foreground;
    }
    // 开放承诺钉住温度，无视时间
    if ts
        .commitments
        .iter()
        .any(|c| c.status == "open" && c.thread_id == thread.id)
    {
        return Temperature::Warm;
    }
    let last = parse_ts_ms(&thread.last_event_at)
        .or_else(|| parse_ts_ms(&thread.created_at))
        .unwrap_or(i64::MIN);
    let age = now.saturating_sub(last);
    if age < WARM_WINDOW_MS {
        Temperature::Warm
    } else if age < COOL_WINDOW_MS {
        Temperature::Cool
    } else {
        Temperature::Cold
    }
}

/// 注入视图：prompt 渲染的唯一入口。每轮重算（读时减法），不缓存、不落库（对齐 buildThreadView）。
#[derive(Debug, Clone, Default)]
pub struct ThreadView {
    pub foreground: Option<Thread>,
    pub foreground_commitment: Option<Commitment>,
    /// warm 的后台线索（按 last_event_at 倒序，最多 MAX_WARM_INJECTED 条）
    pub background: Vec<(Thread, Temperature)>,
    pub open_commitments: Vec<Commitment>,
}

pub fn build_thread_view(ts: &ThreadState, now: i64) -> ThreadView {
    let foreground = get_foreground_thread(ts).cloned();
    let open_commitments: Vec<Commitment> = get_open_commitments(ts).into_iter().cloned().collect();
    let foreground_commitment = foreground.as_ref().and_then(|f| {
        open_commitments
            .iter()
            .find(|c| c.thread_id == f.id)
            .cloned()
    });
    let mut background: Vec<(Thread, Temperature)> = ts
        .threads
        .iter()
        .filter(|t| ts.foreground_id.as_ref() != Some(&t.id))
        .map(|t| (t.clone(), thread_temperature(ts, t, now)))
        .filter(|(_, temp)| *temp == Temperature::Warm)
        .collect();
    background.sort_by(|a, b| {
        let ta = parse_ts_ms(&a.0.last_event_at).unwrap_or(0);
        let tb = parse_ts_ms(&b.0.last_event_at).unwrap_or(0);
        tb.cmp(&ta)
    });
    background.truncate(MAX_WARM_INJECTED);
    ThreadView {
        foreground,
        foreground_commitment,
        background,
        open_commitments,
    }
}

// ── 关键词重叠：对线索 signature ∪ topic 做字面交集（精确匹配，对齐 overlapCount） ──

fn overlap_count(thread: &Thread, kws: &[String]) -> usize {
    let set: HashSet<&str> = thread
        .signature
        .iter()
        .chain(thread.topic.iter())
        .map(|s| s.as_str())
        .collect();
    if set.is_empty() {
        return 0;
    }
    kws.iter().filter(|k| set.contains(k.as_str())).count()
}

// ── 冷线索守卫 ──────────────────────────────────────────────────────────────

/// 按 last_event_at 判断线索是否已冷（无视前台短路；开放承诺仍钉住温度）。
fn is_thread_cold_by_age(ts: &ThreadState, thread: &Thread, now: i64) -> bool {
    if ts
        .commitments
        .iter()
        .any(|c| c.status == "open" && c.thread_id == thread.id)
    {
        return false;
    }
    let last = parse_ts_ms(&thread.last_event_at)
        .or_else(|| parse_ts_ms(&thread.created_at))
        .unwrap_or(0);
    now.saturating_sub(last) >= COOL_WINDOW_MS
}

// ── 用户消息归属判定（唯一需要"判断"的入口；Agent 侧走声明，不经过这里） ────

/// 归属事件（对齐 attributeUserMessage 返回值）。
#[derive(Debug, Clone, PartialEq)]
pub enum AttributionKind {
    /// 新建线索并置前台
    Created,
    /// 命中前台线索
    Continued,
    /// 前台切到既有后台线索（指代性问询路由 / 重叠≥2）
    Resumed,
    /// 叶子/太短/TICK，不动
    Noop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attribution {
    pub kind: AttributionKind,
    pub thread_id: Option<String>,
    /// resumed/created 导致前台易主时的旧前台线索
    pub switched_from: Option<String>,
    pub via: Option<&'static str>,
    /// 重叠=1 的弱信号候选（created 时附带，留给后台分类器仲裁）
    pub ambiguous_with: Option<String>,
}

impl Attribution {
    fn noop() -> Self {
        Attribution {
            kind: AttributionKind::Noop,
            thread_id: None,
            switched_from: None,
            via: None,
            ambiguous_with: None,
        }
    }
}

/// 用户消息归属判定（对齐 attributeUserMessage）。
/// 返回 { event, thread, switchedFrom }：created/continued/resumed/noop。
pub fn attribute_user_message(
    ts: &mut ThreadState,
    message: &str,
    tick: i64,
    channel: &str,
) -> Attribution {
    let body = strip_message_envelope(message);
    if body.is_empty() || body.chars().count() < MIN_MESSAGE_LENGTH {
        return Attribution::noop();
    }
    if is_likely_one_off_leaf(&body) {
        return Attribution::noop();
    }

    let kws = extract_attribution_keywords(&body);
    let foreground = get_foreground_thread(ts).cloned();
    let now = now_ms();

    // 1) 指代性进度问询 → 最近开放承诺的线索。
    //    守卫：消息显式点名了另一条线索（重叠≥2）时，明示压过暗示，走常规归属。
    if is_indexical_progress_query(&body) {
        if let Some(commitment) = latest_open_commitment(ts, channel) {
            let target_id = commitment.thread_id.clone();
            let target = get_thread_by_id(ts, &target_id).cloned();
            let names_other_thread = target.as_ref().is_some()
                && ts.threads.iter().any(|t| {
                    target.as_ref().map(|tg| tg.id != t.id).unwrap_or(false)
                        && overlap_count(t, &kws) >= BACKGROUND_RESUME_OVERLAP_MIN
                });
            if target.is_some() && !names_other_thread {
                let switched_from = foreground
                    .as_ref()
                    .filter(|f| f.id != target_id)
                    .map(|f| f.id.clone());
                let kind = if switched_from.is_some() {
                    AttributionKind::Resumed
                } else {
                    AttributionKind::Continued
                };
                ts.foreground_id = Some(target_id.clone());
                touch_thread(ts, &target_id, tick);
                return Attribution {
                    kind,
                    thread_id: Some(target_id),
                    switched_from,
                    via: Some("commitment"),
                    ambiguous_with: None,
                };
            }
            // namesOtherThread：不在这里 return，落到常规归属规则
        } else {
            // 没有开放承诺的进度问句：当作对前台的继续（若前台还没冷），否则 noop
            if let Some(fg) = &foreground {
                if !is_thread_cold_by_age(ts, fg, now) {
                    touch_thread(ts, &fg.id, tick);
                    return Attribution {
                        kind: AttributionKind::Continued,
                        thread_id: Some(fg.id.clone()),
                        switched_from: None,
                        via: None,
                        ambiguous_with: None,
                    };
                }
            }
            return Attribution::noop();
        }
    }

    // 1.5) 指代-就近 / 精确回指。在常规关键词归属之前拦截。
    let ref_class = classify_reference(&body);
    if ref_class.kind == ReferenceKind::AnaphoraRecent {
        // 规律①：续前台，绝不新建/切走。
        // 守卫一（明示压过暗示）：实质词强匹配某后台线索（≥2）→ 落常规切换。
        let names_other = !ref_class.substantive.is_empty()
            && ts.threads.iter().any(|t| {
                foreground.as_ref().map(|f| f.id != t.id).unwrap_or(true)
                    && overlap_count(t, &ref_class.substantive) >= BACKGROUND_RESUME_OVERLAP_MIN
            });
        // 守卫二：冷掉的前台不当就近锚点。
        if !names_other {
            if let Some(fg) = &foreground {
                if !is_thread_cold_by_age(ts, fg, now) {
                    touch_thread(ts, &fg.id, tick);
                    return Attribution {
                        kind: AttributionKind::Continued,
                        thread_id: Some(fg.id.clone()),
                        switched_from: None,
                        via: Some("anaphora-recent"),
                        ambiguous_with: None,
                    };
                }
            }
        }
    } else if ref_class.kind == ReferenceKind::PreciseCallback && !ref_class.referent_kws.is_empty()
    {
        // 规律②：那个+具体名词是强 resume 信号。命中前台→continued，命中后台→resume（门槛≥1）。
        let rkws = &ref_class.referent_kws;
        if let Some(fg) = &foreground {
            if overlap_count(fg, rkws) >= 1 {
                touch_thread(ts, &fg.id, tick);
                return Attribution {
                    kind: AttributionKind::Continued,
                    thread_id: Some(fg.id.clone()),
                    switched_from: None,
                    via: Some("callback"),
                    ambiguous_with: None,
                };
            }
        }
        let mut cb_best_id: Option<String> = None;
        let mut cb_overlap = 0;
        for t in &ts.threads {
            if foreground.as_ref().map(|f| f.id == t.id).unwrap_or(false) {
                continue;
            }
            let n = overlap_count(t, rkws);
            if n > cb_overlap {
                cb_best_id = Some(t.id.clone());
                cb_overlap = n;
            }
        }
        // 收紧：切到后台线索需 ≥2 个关键词命中。
        if let Some(best_id) = cb_best_id {
            if cb_overlap >= BACKGROUND_RESUME_OVERLAP_MIN {
                let switched_from = foreground
                    .as_ref()
                    .filter(|f| f.id != best_id)
                    .map(|f| f.id.clone());
                let kind = if switched_from.is_some() {
                    AttributionKind::Resumed
                } else {
                    AttributionKind::Continued
                };
                ts.foreground_id = Some(best_id.clone());
                touch_thread(ts, &best_id, tick);
                return Attribution {
                    kind,
                    thread_id: Some(best_id),
                    switched_from,
                    via: Some("callback"),
                    ambiguous_with: None,
                };
            }
        }
        // 落空 fallback：带指代词的句子哪怕一个既有线索都没字面命中，也续前台而非新建。
        if let Some(fg) = &foreground {
            if !is_thread_cold_by_age(ts, fg, now) {
                touch_thread(ts, &fg.id, tick);
                return Attribution {
                    kind: AttributionKind::Continued,
                    thread_id: Some(fg.id.clone()),
                    switched_from: None,
                    via: Some("callback-fallback"),
                    ambiguous_with: None,
                };
            }
        }
    }

    // 2) 关键词稀薄的短消息：续前台（廉价、可自愈）；冷掉的前台不续命。
    if kws.len() < MIN_KEYWORDS_FOR_THREAD {
        if let Some(fg) = &foreground {
            if !is_thread_cold_by_age(ts, fg, now) {
                touch_thread(ts, &fg.id, tick);
                return Attribution {
                    kind: AttributionKind::Continued,
                    thread_id: Some(fg.id.clone()),
                    switched_from: None,
                    via: None,
                    ambiguous_with: None,
                };
            }
        }
        return Attribution::noop();
    }

    // 3) 前台重叠 ≥1 → continued
    if let Some(fg) = &foreground {
        if overlap_count(fg, &kws) >= FOREGROUND_OVERLAP_MIN {
            touch_thread(ts, &fg.id, tick);
            return Attribution {
                kind: AttributionKind::Continued,
                thread_id: Some(fg.id.clone()),
                switched_from: None,
                via: None,
                ambiguous_with: None,
            };
        }
    }

    // 4) 后台线索：≥2 切换；=1 不切换、记 ambiguous 给分类器
    let mut best_id: Option<String> = None;
    let mut best_overlap = 0;
    for t in &ts.threads {
        if foreground.as_ref().map(|f| f.id == t.id).unwrap_or(false) {
            continue;
        }
        let n = overlap_count(t, &kws);
        if n > best_overlap {
            best_id = Some(t.id.clone());
            best_overlap = n;
        }
    }
    if let Some(ref b_id) = best_id {
        if best_overlap >= BACKGROUND_RESUME_OVERLAP_MIN {
            let switched_from = foreground.as_ref().map(|f| f.id.clone());
            ts.foreground_id = Some(b_id.clone());
            touch_thread(ts, b_id, tick);
            return Attribution {
                kind: AttributionKind::Resumed,
                thread_id: Some(b_id.clone()),
                switched_from,
                via: None,
                ambiguous_with: None,
            };
        }
    }

    // 5) 新建线索置前台。误判的代价是多一条线索（合并可修正），不是失忆。
    let topic: Vec<String> = kws.iter().take(TOPIC_KEYWORDS_LIMIT).cloned().collect();
    let created = make_thread(&topic, tick, "", Some(&kws));
    let created_id = created.id.clone();
    let switched_from = foreground.as_ref().map(|f| f.id.clone());
    ts.threads.push(created);
    ts.foreground_id = Some(created_id.clone());
    evict_cold_threads(ts, now);
    if let Some(b_id) = best_id {
        if best_overlap == 1 {
            // 弱信号候选：留给后台分类器仲裁，确认是同一事则 merge（合并永远安全）
            return Attribution {
                kind: AttributionKind::Created,
                thread_id: Some(created_id),
                switched_from,
                via: None,
                ambiguous_with: Some(b_id),
            };
        }
    }
    Attribution {
        kind: AttributionKind::Created,
        thread_id: Some(created_id),
        switched_from,
        via: None,
        ambiguous_with: None,
    }
}

// ── 合并（分类器事后仲裁"其实是同一条线索"时的修正动作；无栈序，永远安全） ────

/// 把 source 并入 target：内存侧合 topic/结论/计数（db 行 thread_id 重写由上层做，对齐 mergeThreads）。
pub fn merge_threads(ts: &mut ThreadState, source_id: &str, target_id: &str) -> Option<String> {
    if source_id == target_id {
        return None;
    }
    let source_idx = ts.threads.iter().position(|t| t.id == source_id)?;
    let target_idx = ts.threads.iter().position(|t| t.id == target_id)?;
    if source_idx == target_idx {
        return None;
    }
    let source = ts.threads[source_idx].clone();
    let target = &mut ts.threads[target_idx];
    let mut topic_set: Vec<String> = Vec::new();
    for k in target.topic.iter().chain(source.topic.iter()) {
        if !topic_set.contains(k) {
            topic_set.push(k.clone());
        }
    }
    target.topic = topic_set.into_iter().take(TOPIC_KEYWORDS_LIMIT).collect();
    let mut sig_set: Vec<String> = Vec::new();
    for k in target.signature.iter().chain(source.signature.iter()) {
        if !sig_set.contains(k) {
            sig_set.push(k.clone());
        }
    }
    target.signature = sig_set.into_iter().take(SIGNATURE_LIMIT).collect();
    for c in &source.conclusions {
        if !target.conclusions.contains(c) {
            target.conclusions.push(c.clone());
        }
    }
    while target.conclusions.len() > THREAD_CONCLUSIONS_LIMIT {
        target.conclusions.remove(0);
    }
    if !source.summary.is_empty() && target.summary.is_empty() {
        target.summary = source.summary.clone();
    }
    target.hit_count += source.hit_count;
    let src_last = parse_ts_ms(&source.last_event_at).unwrap_or(0);
    let tgt_last = parse_ts_ms(&target.last_event_at).unwrap_or(0);
    if src_last > tgt_last {
        target.last_event_at = source.last_event_at.clone();
        target.last_event_tick = source.last_event_tick;
    }
    // 承诺过户
    for c in &mut ts.commitments {
        if c.thread_id == source_id {
            c.thread_id = target_id.to_string();
        }
    }
    // 移除 source（保持原相对顺序）
    ts.threads.retain(|t| t.id != source_id);
    if ts.foreground_id.as_deref() == Some(source_id) {
        ts.foreground_id = Some(target_id.to_string());
    }
    Some(target_id.to_string())
}

// ── 结论回填 ────────────────────────────────────────────────────────────────

/// 给线索挂结论（增量摘要器回填用）。滚动上限，绝不替换原文（对齐 appendConclusion）。
pub fn append_conclusion(thread: &mut Thread, conclusion: &str) {
    let text = conclusion.trim();
    if text.is_empty() || thread.conclusions.contains(&text.to_string()) {
        return;
    }
    thread.conclusions.push(text.to_string());
    while thread.conclusions.len() > THREAD_CONCLUSIONS_LIMIT {
        thread.conclusions.remove(0);
    }
}

// ── 内存瘦身（不是遗忘） ────────────────────────────────────────────────────

/// 超限时把 cold 且非前台、按 last_event_at 最老的线索移出内存（db 里仍在，对齐 evictColdThreads）。
pub fn evict_cold_threads(ts: &mut ThreadState, now: i64) -> Vec<String> {
    if ts.threads.len() <= MAX_THREADS_IN_MEMORY {
        return Vec::new();
    }
    let mut evictable: Vec<usize> = ts
        .threads
        .iter()
        .enumerate()
        .filter(|(_, t)| ts.foreground_id.as_ref() != Some(&t.id))
        .filter(|(_, t)| thread_temperature(ts, t, now) == Temperature::Cold)
        .map(|(i, _)| i)
        .collect();
    evictable.sort_by_key(|&i| parse_ts_ms(&ts.threads[i].last_event_at).unwrap_or(0));
    let excess = ts.threads.len() - MAX_THREADS_IN_MEMORY;
    let evicted: Vec<usize> = evictable.into_iter().take(excess).collect();
    if evicted.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = evicted.iter().map(|&i| ts.threads[i].id.clone()).collect();
    let id_set: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
    ts.threads.retain(|t| !id_set.contains(t.id.as_str()));
    ids
}

// ── 从专注栈一次性迁移（首启 threads 为空且 focus_stack 有货时） ─────────────

/// 帧→线索：栈顶=前台，其余=后台。承诺无从恢复，留空（对齐 migrateFocusStackToThreads）。
pub fn migrate_focus_stack_to_threads(focus_stack: &[FocusFrame], tick: i64) -> ThreadState {
    let mut threads: Vec<Thread> = Vec::new();
    for frame in focus_stack {
        if frame.topic.is_empty() {
            continue;
        }
        let mut t = make_thread(&frame.topic, tick, "", Some(&frame.topic));
        t.created_at = frame.started_at.clone();
        t.last_event_at = frame.started_at.clone();
        t.last_event_tick = frame.last_seen_tick;
        t.hit_count = frame.hit_count.max(1);
        let n = frame.conclusions.len();
        let start = n.saturating_sub(THREAD_CONCLUSIONS_LIMIT);
        t.conclusions = frame.conclusions[start..].to_vec();
        threads.push(t);
    }
    let foreground_id = threads.last().map(|t| t.id.clone());
    ThreadState {
        threads,
        foreground_id,
        commitments: Vec::new(),
    }
}

// ── 持久化接缝（对齐 index.js 启动流程） ────────────────────────────────────

/// 启动时恢复线索状态（对齐 index.js `initThreadState`）：
/// 1. threads 表有货 → 直接加载（读时过滤：开放承诺钉住 + 7 天窗口）。
/// 2. 空但 focus_stack 有货 → 一次性迁移为线索（栈顶=前台）并立即落库。
/// 3. 都空 → 全新空状态。
pub fn init_thread_state(db: &Db, tick: i64) -> Result<ThreadState> {
    if let Some(ts) = load_thread_state(db)? {
        return Ok(ts);
    }
    let legacy = load_focus_stack(db)?;
    if !legacy.is_empty() {
        let migrated = migrate_focus_stack_to_threads(&legacy, tick);
        save_state(db, &migrated, None)?;
        return Ok(migrated);
    }
    Ok(ThreadState::default())
}

// ── 便捷：渲染线索为单行人话 ────────────────────────────────────────────────

/// 渲染线索为单行人话（brain-ui / 日志用，对齐 describeThread）。
pub fn describe_thread(thread: &Thread) -> String {
    let label = if !thread.label.is_empty() {
        thread.label.clone()
    } else {
        thread.topic.join(",")
    };
    match thread.conclusions.last() {
        Some(last) if !last.is_empty() => format!("{label} — {last}"),
        _ => label,
    }
}

// ── 测试（对齐 threads.js；纯内存状态为主，持久化接缝测试用临时库） ──────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::now_iso;
    use crate::db::repositories::threads::save_focus_stack;
    use crate::db::{open_database, Db};
    use chrono::{Duration, Utc};

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        open_database(path).unwrap()
    }

    fn iso_hours_ago(h: i64) -> String {
        (Utc::now() - Duration::hours(h)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// 建线索并置为前台。
    fn make_topic(ts: &mut ThreadState, kws: &[&str], tick: i64) -> String {
        let topic: Vec<String> = kws.iter().map(|s| s.to_string()).collect();
        let t = make_thread(&topic, tick, "", Some(&topic));
        let id = t.id.clone();
        ts.threads.push(t);
        ts.foreground_id = Some(id.clone());
        id
    }

    /// 建后台线索（不碰前台指针）。
    fn make_bg(ts: &mut ThreadState, kws: &[&str]) -> String {
        let topic: Vec<String> = kws.iter().map(|s| s.to_string()).collect();
        let t = make_thread(&topic, 0, "", Some(&topic));
        let id = t.id.clone();
        ts.threads.push(t);
        id
    }

    /// 建后台线索并模拟冷却（不碰前台指针）。
    fn cold_thread(ts: &mut ThreadState, kws: &[String], hours: i64) -> String {
        let topic: Vec<String> = kws.to_vec();
        let t = make_thread(&topic, 0, "", Some(&topic));
        let id = t.id.clone();
        ts.threads.push(t);
        ts.threads
            .iter_mut()
            .find(|t| t.id == id)
            .unwrap()
            .last_event_at = iso_hours_ago(hours);
        id
    }

    // ── 消息信封剥离 ──

    #[test]
    fn strips_message_envelope() {
        assert_eq!(
            strip_message_envelope("[ID:abc123] 2026-04-13-10:00:00 [wechat] 部署脚本优化一下"),
            "部署脚本优化一下"
        );
        assert_eq!(strip_message_envelope("普通消息"), "普通消息");
        assert_eq!(strip_message_envelope("TICK 12345"), "");
        assert_eq!(strip_message_envelope(""), "");
    }

    // ── 叶子 / 指代分类 ──

    #[test]
    fn one_off_leaf_and_progress_query() {
        assert!(is_likely_one_off_leaf("今天天气怎么样"));
        assert!(!is_likely_one_off_leaf("部署脚本优化一下")); // sustained 拦截
        assert!(is_likely_one_off_leaf("早上好"));
        assert!(is_indexical_progress_query("那个任务进度怎么样了"));
        assert!(!is_indexical_progress_query(
            "这个句子的长度超过二十五个字符那么长的进度问询消息啊"
        ));
    }

    #[test]
    fn classifies_reference() {
        assert_eq!(classify_reference("部署方案").kind, ReferenceKind::None);
        assert_eq!(
            classify_reference("把那个网页打开").kind,
            ReferenceKind::AnaphoraRecent
        );
        assert_eq!(
            classify_reference("那套部署方案").kind,
            ReferenceKind::PreciseCallback
        );
        assert_eq!(classify_reference("").kind, ReferenceKind::None);
    }

    // ── 温度 ──

    #[test]
    fn thread_temperature_grading() {
        let mut ts = ThreadState::default();
        let fg_id = make_topic(&mut ts, &["部署"], 1);
        let bg_id = make_bg(&mut ts, &["数据库"]);
        let now = now_ms();
        assert_eq!(
            thread_temperature(&ts, get_thread_by_id(&ts, &fg_id).unwrap(), now),
            Temperature::Foreground
        );
        // 新建 → 6h 内 → warm
        assert_eq!(
            thread_temperature(&ts, get_thread_by_id(&ts, &bg_id).unwrap(), now),
            Temperature::Warm
        );
        // 10h → cool
        ts.threads
            .iter_mut()
            .find(|t| t.id == bg_id)
            .unwrap()
            .last_event_at = iso_hours_ago(10);
        assert_eq!(
            thread_temperature(&ts, get_thread_by_id(&ts, &bg_id).unwrap(), now),
            Temperature::Cool
        );
        // 50h → cold
        ts.threads
            .iter_mut()
            .find(|t| t.id == bg_id)
            .unwrap()
            .last_event_at = iso_hours_ago(50);
        assert_eq!(
            thread_temperature(&ts, get_thread_by_id(&ts, &bg_id).unwrap(), now),
            Temperature::Cold
        );
        // 开放承诺钉住温度（无视时间）
        open_commitment(&mut ts, "挂着", Some(&bg_id), "", 1);
        assert_eq!(
            thread_temperature(&ts, get_thread_by_id(&ts, &bg_id).unwrap(), now),
            Temperature::Warm
        );
    }

    // ── 承诺生命周期 ──

    #[test]
    fn open_commitment_creates_thread_when_no_foreground() {
        let mut ts = ThreadState::default();
        let cid = {
            let c = open_commitment(&mut ts, "明天把部署脚本搞定", None, "wechat", 1);
            c.id.clone()
        };
        assert_eq!(ts.threads.len(), 1);
        assert_eq!(ts.foreground_id.as_deref(), Some(ts.threads[0].id.as_str()));
        let c = ts.commitments.iter().find(|c| c.id == cid).unwrap();
        assert_eq!(c.thread_id, ts.threads[0].id);
        assert_eq!(c.status, "open");
    }

    #[test]
    fn open_commitment_hangs_on_foreground_and_updates_text() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["部署"], 1);
        open_commitment(&mut ts, "第一版", Some(&fg), "wechat", 2);
        let c2 = open_commitment(&mut ts, "第二版", Some(&fg), "wechat", 3);
        assert_eq!(c2.thread_id, fg);
        assert_eq!(c2.text, "第二版");
        assert_eq!(ts.commitments.len(), 1); // 单例：更新不叠加
        assert_eq!(ts.threads.len(), 1); // 不新建线索
    }

    #[test]
    fn close_commitment_marks_done_or_cancelled() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["部署"], 1);
        let cid = open_commitment(&mut ts, "任务", Some(&fg), "", 1)
            .id
            .clone();
        let closed = close_commitment(&mut ts, None, Some(&cid), "done").unwrap();
        assert_eq!(closed.status, "done");
        assert!(closed.closed_at.is_some());
        assert!(close_commitment(&mut ts, None, Some(&cid), "done").is_none()); // 已关
        let cid2 = open_commitment(&mut ts, "任务二", Some(&fg), "", 2)
            .id
            .clone();
        close_commitment(&mut ts, None, Some(&cid2), "cancelled");
        assert_eq!(
            ts.commitments.iter().find(|c| c.id == cid2).unwrap().status,
            "cancelled"
        );
    }

    #[test]
    fn latest_open_commitment_prefers_channel_then_newest() {
        let mut ts = ThreadState::default();
        let t1 = make_bg(&mut ts, &["甲"]);
        let t2 = make_bg(&mut ts, &["乙"]);
        let t3 = make_bg(&mut ts, &["丙"]);
        open_commitment(&mut ts, "任务一", Some(&t1), "wechat", 1);
        let c3_id = {
            let c3 = open_commitment(&mut ts, "任务三", Some(&t2), "dingtalk", 3);
            c3.id.clone()
        };
        assert_eq!(
            latest_open_commitment(&ts, "wechat").unwrap().text,
            "任务一"
        );
        assert_eq!(latest_open_commitment(&ts, "").unwrap().id, c3_id);
        // 同 created_at 时数组序后者胜
        let b_id = {
            let b = open_commitment(&mut ts, "任务b", Some(&t3), "wechat", 6);
            b.id.clone()
        };
        for c in &mut ts.commitments {
            c.created_at = iso_hours_ago(1);
        }
        assert_eq!(latest_open_commitment(&ts, "wechat").unwrap().id, b_id);
    }

    // ── 注入视图 ──

    #[test]
    fn build_view_picks_warm_background_desc() {
        let mut ts = ThreadState::default();
        let fg_id = make_topic(&mut ts, &["部署"], 1);
        let w1 = make_bg(&mut ts, &["a"]);
        let w2 = make_bg(&mut ts, &["b"]);
        let w3 = make_bg(&mut ts, &["c"]);
        let cool = make_bg(&mut ts, &["d"]);
        ts.threads
            .iter_mut()
            .find(|t| t.id == cool)
            .unwrap()
            .last_event_at = iso_hours_ago(10);
        // 拉开 warm 后台的时间差保证排序可断言
        for (i, id) in [&w1, &w2, &w3].iter().enumerate() {
            ts.threads
                .iter_mut()
                .find(|t| t.id == **id)
                .unwrap()
                .last_event_at = iso_hours_ago(5 - i as i64);
        }
        open_commitment(&mut ts, "任务", Some(&fg_id), "wechat", 1);
        let view = build_thread_view(&ts, now_ms());
        assert_eq!(view.foreground.as_ref().unwrap().id, fg_id);
        assert!(view.foreground_commitment.is_some());
        assert_eq!(view.open_commitments.len(), 1);
        assert_eq!(view.background.len(), 3); // cool 不进背景
        let ids: Vec<String> = view.background.iter().map(|(t, _)| t.id.clone()).collect();
        assert_eq!(ids, vec![w3, w2, w1]); // 最近活跃在前
    }

    // ── 归属判定 ──

    #[test]
    fn attribute_noop_for_leaf_short_and_tick() {
        let mut ts = ThreadState::default();
        assert_eq!(
            attribute_user_message(&mut ts, "今天天气怎么样", 1, "wechat").kind,
            AttributionKind::Noop
        );
        assert_eq!(
            attribute_user_message(&mut ts, "好的", 1, "wechat").kind,
            AttributionKind::Noop
        );
        assert_eq!(
            attribute_user_message(&mut ts, "TICK 123", 1, "wechat").kind,
            AttributionKind::Noop
        );
        assert_eq!(
            attribute_user_message(&mut ts, "", 1, "wechat").kind,
            AttributionKind::Noop
        );
        assert!(ts.threads.is_empty());
    }

    #[test]
    fn attribute_routes_progress_query_to_commitment_thread() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["数据库"], 1);
        let bg = make_bg(&mut ts, &["部署", "脚本"]);
        open_commitment(&mut ts, "去把部署脚本搞定", Some(&bg), "wechat", 1);
        let a = attribute_user_message(&mut ts, "那个任务进度怎么样了", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Resumed);
        assert_eq!(a.thread_id.as_deref(), Some(bg.as_str()));
        assert_eq!(a.switched_from.as_deref(), Some(fg.as_str()));
        assert_eq!(a.via, Some("commitment"));
        assert_eq!(ts.foreground_id.as_deref(), Some(bg.as_str()));
    }

    #[test]
    fn attribute_progress_without_commitment_continues_foreground() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["部署"], 1);
        let a = attribute_user_message(&mut ts, "进度怎么样了", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Continued);
        assert_eq!(a.thread_id.as_deref(), Some(fg.as_str()));
        assert_eq!(a.switched_from, None);
    }

    #[test]
    fn attribute_progress_with_cold_foreground_noops() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["部署"], 1);
        ts.threads
            .iter_mut()
            .find(|t| t.id == fg)
            .unwrap()
            .last_event_at = iso_hours_ago(50);
        let a = attribute_user_message(&mut ts, "进度怎么样了", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Noop);
    }

    #[test]
    fn attribute_anaphora_recent_continues_foreground() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["浏览器"], 1);
        let a = attribute_user_message(&mut ts, "把那个网页打开", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Continued);
        assert_eq!(a.thread_id.as_deref(), Some(fg.as_str()));
        assert_eq!(a.via, Some("anaphora-recent"));
    }

    #[test]
    fn attribute_precise_callback_continues_foreground_on_hit() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["部署", "方案"], 1);
        let a = attribute_user_message(&mut ts, "那个部署方案再看看", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Continued);
        assert_eq!(a.thread_id.as_deref(), Some(fg.as_str()));
        assert_eq!(a.via, Some("callback"));
    }

    #[test]
    fn attribute_precise_callback_resumes_background_thread() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["数据库"], 1);
        let bg = make_bg(&mut ts, &["部署", "方案"]);
        let a = attribute_user_message(&mut ts, "那套部署方案改一下", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Resumed);
        assert_eq!(a.thread_id.as_deref(), Some(bg.as_str()));
        assert_eq!(a.switched_from.as_deref(), Some(fg.as_str()));
        assert_eq!(a.via, Some("callback"));
        assert_eq!(ts.foreground_id.as_deref(), Some(bg.as_str()));
    }

    #[test]
    fn attribute_callback_fallback_continues_foreground() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["数据库"], 1);
        let a = attribute_user_message(&mut ts, "那个排名再看看", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Continued);
        assert_eq!(a.thread_id.as_deref(), Some(fg.as_str()));
        assert_eq!(a.via, Some("callback-fallback"));
    }

    #[test]
    fn attribute_sparse_keywords_continue_foreground() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["数据库"], 1);
        let a = attribute_user_message(&mut ts, "ok ok ok", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Continued);
        assert_eq!(a.thread_id.as_deref(), Some(fg.as_str()));
    }

    #[test]
    fn attribute_sparse_keywords_cold_foreground_noops() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["数据库"], 1);
        ts.threads
            .iter_mut()
            .find(|t| t.id == fg)
            .unwrap()
            .last_event_at = iso_hours_ago(50);
        let a = attribute_user_message(&mut ts, "ok ok ok", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Noop);
    }

    #[test]
    fn attribute_foreground_overlap_continues() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["部署", "脚本"], 1);
        let a = attribute_user_message(&mut ts, "部署脚本怎么优化", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Continued);
        assert_eq!(a.thread_id.as_deref(), Some(fg.as_str()));
        assert_eq!(a.switched_from, None);
    }

    #[test]
    fn attribute_background_overlap_two_resumes() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["数据库"], 1);
        let bg = make_bg(&mut ts, &["部署", "脚本"]);
        let a = attribute_user_message(&mut ts, "部署脚本怎么优化", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Resumed);
        assert_eq!(a.thread_id.as_deref(), Some(bg.as_str()));
        assert_eq!(a.switched_from.as_deref(), Some(fg.as_str()));
        assert_eq!(ts.foreground_id.as_deref(), Some(bg.as_str()));
    }

    #[test]
    fn attribute_background_overlap_one_creates_with_ambiguous() {
        let mut ts = ThreadState::default();
        let _fg = make_topic(&mut ts, &["数据库"], 1);
        let bg = make_bg(&mut ts, &["部署"]);
        let a = attribute_user_message(&mut ts, "部署和流程怎么协调", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Created);
        assert_eq!(a.ambiguous_with.as_deref(), Some(bg.as_str()));
        assert_eq!(ts.threads.len(), 3);
        assert!(ts.foreground_id.as_deref() != Some(bg.as_str()));
    }

    #[test]
    fn attribute_new_topic_creates_foreground() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["数据库"], 1);
        let a = attribute_user_message(&mut ts, "明天下午开周会", 2, "wechat");
        assert_eq!(a.kind, AttributionKind::Created);
        assert_eq!(a.ambiguous_with, None);
        assert_eq!(a.switched_from.as_deref(), Some(fg.as_str()));
        assert_eq!(ts.threads.len(), 2);
        assert!(get_foreground_thread(&ts).is_some());
    }

    // ── 合并 / 结论 / 驱逐 / 迁移 / 渲染 ──

    #[test]
    fn merge_threads_absorbs_source_into_target() {
        let mut ts = ThreadState::default();
        let src = make_topic(&mut ts, &["部署", "脚本"], 1); // 前台
        let tgt = make_bg(&mut ts, &["数据库"]);
        let cid = open_commitment(&mut ts, "去部署", Some(&src), "wechat", 1)
            .id
            .clone();
        ts.threads
            .iter_mut()
            .find(|t| t.id == src)
            .unwrap()
            .conclusions
            .push("部署完成".to_string());
        let res = merge_threads(&mut ts, &src, &tgt);
        assert_eq!(res, Some(tgt.clone()));
        assert!(get_thread_by_id(&ts, &src).is_none()); // source 移除
        assert_eq!(ts.foreground_id.as_deref(), Some(tgt.as_str())); // 前台校正
                                                                     // 承诺过户
        assert_eq!(
            ts.commitments
                .iter()
                .find(|c| c.id == cid)
                .unwrap()
                .thread_id,
            tgt
        );
        let merged = get_thread_by_id(&ts, &tgt).unwrap();
        assert!(merged.topic.contains(&"数据库".to_string()));
        assert!(merged.topic.contains(&"部署".to_string()));
        assert!(merged.topic.contains(&"脚本".to_string()));
        assert_eq!(merged.topic.len(), 3);
        assert_eq!(merged.conclusions, vec!["部署完成"]);
        assert_eq!(merged.hit_count, 3); // 1(建) + touch(承诺) + 1(目标)
                                         // 同 id / 不存在 → None
        assert!(merge_threads(&mut ts, &tgt, &tgt).is_none());
        assert!(merge_threads(&mut ts, "不存在", &tgt).is_none());
    }

    #[test]
    fn append_conclusion_dedupes_and_rolls() {
        let mut t = make_thread(&["x".to_string()], 0, "", None);
        for i in 0..7 {
            append_conclusion(&mut t, &format!("c{i}"));
        }
        assert_eq!(t.conclusions.len(), THREAD_CONCLUSIONS_LIMIT);
        assert_eq!(t.conclusions, vec!["c2", "c3", "c4", "c5", "c6"]);
        append_conclusion(&mut t, "c4"); // 去重
        assert_eq!(t.conclusions.len(), THREAD_CONCLUSIONS_LIMIT);
    }

    #[test]
    fn evicts_coldest_non_foreground_when_over_cap() {
        let mut ts = ThreadState::default();
        let fg = make_topic(&mut ts, &["前台"], 1);
        let mut cold_ids = Vec::new();
        for i in 0..9 {
            cold_ids.push(cold_thread(&mut ts, &[format!("cold{i}")], 60 + i));
        }
        for i in 0..3 {
            cold_thread(&mut ts, &[format!("warm{i}")], 1);
        }
        assert_eq!(ts.threads.len(), 13); // 1 前台 + 9 cold + 3 warm
        let evicted = evict_cold_threads(&mut ts, now_ms());
        assert_eq!(evicted.len(), 1); // 只驱逐超出的 1 条
        assert_eq!(ts.threads.len(), 12);
        assert_eq!(evicted[0], cold_ids[8]); // 最老（最早 last_event_at）的那条
        assert!(get_thread_by_id(&ts, &fg).is_some()); // 前台永不出列
        assert!(get_thread_by_id(&ts, &evicted[0]).is_none());
        // 不超限 → 不驱逐
        assert!(evict_cold_threads(&mut ts, now_ms()).is_empty());
    }

    #[test]
    fn migrates_focus_stack_to_threads() {
        let frames = vec![
            FocusFrame {
                topic: vec!["部署".into()],
                started_at: iso_hours_ago(48),
                started_at_tick: 1,
                last_seen_tick: 1,
                hit_count: 2,
                conclusions: Vec::new(),
            },
            FocusFrame {
                topic: vec!["数据库".into()],
                started_at: iso_hours_ago(24),
                started_at_tick: 2,
                last_seen_tick: 3,
                hit_count: 4,
                conclusions: (0..7).map(|i| format!("c{i}")).collect(),
            },
        ];
        let ts = migrate_focus_stack_to_threads(&frames, 5);
        assert_eq!(ts.threads.len(), 2);
        assert_eq!(ts.foreground_id.as_deref(), Some(ts.threads[1].id.as_str())); // 栈顶=前台
        assert_eq!(ts.threads[1].conclusions.len(), THREAD_CONCLUSIONS_LIMIT);
        assert_eq!(ts.threads[1].conclusions[0], "c2"); // 取后 5 条
        assert_eq!(ts.threads[0].hit_count, 2);
        assert!(ts.commitments.is_empty());
        // 空 topic 帧跳过
        let empty = migrate_focus_stack_to_threads(
            &[FocusFrame {
                topic: Vec::new(),
                started_at: iso_hours_ago(1),
                started_at_tick: 0,
                last_seen_tick: 0,
                hit_count: 0,
                conclusions: Vec::new(),
            }],
            0,
        );
        assert!(empty.threads.is_empty());
        assert_eq!(empty.foreground_id, None);
    }

    #[test]
    fn describes_thread_label_and_conclusion() {
        let mut t = make_thread(&["部署".to_string(), "脚本".to_string()], 0, "", None);
        assert_eq!(describe_thread(&t), "部署,脚本");
        append_conclusion(&mut t, "完成");
        assert_eq!(describe_thread(&t), "部署,脚本 — 完成");
    }

    // ── 持久化接缝：init_thread_state（对齐 index.js initThreadState） ──

    #[test]
    fn init_restores_persisted_threads() {
        let db = test_db();
        let t = make_thread(&["恢复".to_string(), "线索".to_string()], 3, "", None);
        let id = t.id.clone();
        let ts = ThreadState {
            threads: vec![t],
            foreground_id: Some(id.clone()),
            commitments: Vec::new(),
        };
        save_state(&db, &ts, None).unwrap();
        // threads 表有货 → 直接恢复（不碰 focus_stack）
        let got = init_thread_state(&db, 0).unwrap();
        assert_eq!(got.threads.len(), 1);
        assert_eq!(got.foreground_id.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn init_migrates_legacy_focus_stack() {
        let db = test_db();
        let f = FocusFrame {
            topic: vec!["部署".into(), "脚本".into()],
            started_at: now_iso(),
            started_at_tick: 0,
            last_seen_tick: 2,
            hit_count: 2,
            conclusions: vec!["已回填".into()],
        };
        save_focus_stack(&db, &[f]).unwrap();
        let got = init_thread_state(&db, 5).unwrap();
        assert_eq!(got.threads.len(), 1);
        assert_eq!(
            got.threads[0].topic,
            vec!["部署".to_string(), "脚本".to_string()]
        );
        assert_eq!(got.threads[0].hit_count, 2);
        assert!(got.foreground_id.is_some()); // 栈顶=前台
                                              // 迁移已落库 → 二次初始化走恢复路径，结果一致
        let again = init_thread_state(&db, 0).unwrap();
        assert_eq!(again.threads.len(), 1);
        assert_eq!(again.foreground_id, got.foreground_id);
    }

    #[test]
    fn init_returns_empty_when_nothing_persisted() {
        let db = test_db();
        let got = init_thread_state(&db, 0).unwrap();
        assert!(got.threads.is_empty());
        assert!(got.foreground_id.is_none());
        assert!(got.commitments.is_empty());
    }
}
