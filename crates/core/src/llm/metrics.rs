//! LLM 调用指标采集（P0 观测层，M1/M2/M3）。
//!
//! 设计要点（详见 DESIGN_LLM_METRICS.md + DELIBERATION_FINAL_PLAN.md §5.2）：
//! - 流路径只做 `mpsc` 无锁队列 send（<1ms），绝不阻塞 TTFT / 意识循环；
//! - 落库由后台 flusher 批量执行（默认 30s 或 100 条），失败仅 warn（best-effort，
//!   对齐 brain_ui_events 的边界：观测历史绝不拖垮主流程）；
//! - `llm_calls` 一行 = 一次逻辑请求（重试/降级共享同一 `request_id`），
//!   UPSERT 语义（成功覆盖错误、attempt 取 MAX、错误不覆盖成功）见仓储层；
//! - 日聚合口径：`total_calls` 只在终态事件计数；错误被后续成功覆盖时自动纠偏
//!   （`DailyState` 状态机）；重试/降级独立计数，不与错误双重计；
//! - 明细 2 万行滚动淘汰，日聚合永久（长期趋势不断档）；
//! - M2 工具台账事件（键含 attempt 维度）与 M3 上下文统计事件一并在此处理。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::db::repositories::llm_metrics::{
    prune_detail, upsert_calls_batch, upsert_context_sections_batch, upsert_daily,
    upsert_tool_calls_batch, upsert_turns_batch, DailyDelta, LlmCallAgg, LlmContextSectionRow,
    LlmToolCallRow, LlmTurnRow,
};
use crate::db::Db;

/// 默认 flush 间隔（对齐设计：30s）
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// 默认批量阈值（对齐设计：100 条）
pub const FLUSH_BATCH: usize = 100;
/// 明细滚动上限（对齐设计：2 万行）
pub const DETAIL_KEEP_ROWS: i64 = 20_000;
/// 内存聚合上限（防长跑进程无界增长；超出后优先淘汰已记账的终态行）
pub const AGG_CAP: usize = 20_000;

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
        /// M3：调用阶段（run_turn / tool_loop / wakeup / startup；空串 = 未标注）
        stage: String,
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
    /// 流错误（caller.rs 错误分支）
    CallFailed {
        request_id: String,
        duration_ms: i64,
        error_stage: String, // connect | http | stream | parse
        error_class: String, // timeout | http | read_failed | idle_timeout | protocol | network
        http_status: Option<u16>,
        had_content: bool,
        retryable: bool,
    },
    /// 重试/降级决策（retry.rs 决策点）
    RetryDecision {
        request_id: String,
        attempt: u32,
        decision: String, // retry | no_retry_had_content | no_retry_not_transient | no_retry_429 | no_retry_401 | fallback | no_fallback_auth
        delay_ms: u64,
        model: Option<String>,
        next_model: Option<String>,
    },
    /// M2：工具执行台账（键含 attempt；delegated_from 预留给协作信任账本）
    ToolCall {
        request_id: String,
        round: i64,
        attempt: u32,
        tool_name: String,
        args_json: String,
        result_json: String,
        /// ok | error | tripped
        status: String,
        duration_ms: i64,
        delegated_from: String,
    },
    /// M3：注入上下文统计（injector 输出 → context_bytes，供缓存友好化分析）
    ContextStats {
        request_id: String,
        /// section 名 → 字节数（命中明细；与 llm_calls JOIN 可用）
        sections: Vec<(String, usize)>,
        context_bytes: usize,
    },
    /// M2：工具循环达 max_rounds 上限（异常终止；llm_calls.finish_reason = "round_limit"）
    RoundLimit { request_id: String },
    /// M3：turn 级记录（run_turn 全流程；turn_id = run_turn 主调用的 request_id）
    TurnRecorded {
        turn_id: String,
        started_at: String,
        duration_ms: i64,
        /// created | continued | resumed | noop
        attribution: String,
        is_tick: bool,
        sections_hit: usize,
        context_bytes: usize,
        calls: u32,
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
    #[allow(dead_code)] // 预留：shutdown 完整等待用（当前只发命令不 await，进程退出最多丢最后 30s）
    join: Arc<JoinHandle<()>>,
}

impl FlusherHandle {
    /// 立即排空当前积压并落库，确认完成后返回（测试确定性用）
    pub async fn flush_now(&self) {
        let (ack, rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(FlushCmd::FlushNow(ack));
        let _ = rx.await;
    }

    /// 排空后停止后台任务（进程退出前调用；M1 可接受不调，最多丢最后 30s）
    pub async fn shutdown(&self) {
        let _ = self.tx.send(FlushCmd::Shutdown);
    }
}

enum FlushCmd {
    FlushNow(tokio::sync::oneshot::Sender<()>),
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
        let mut agg: HashMap<String, AggEntry> = HashMap::new();
        let mut daily: HashMap<String, DailyDelta> = HashMap::new();
        let mut tool_rows: Vec<LlmToolCallRow> = Vec::new();
        let mut section_rows: Vec<LlmContextSectionRow> = Vec::new();
        let mut turn_rows: Vec<LlmTurnRow> = Vec::new();
        let mut last_prune: Option<String> = None; // 进程内记忆，重启后下一个自然日再 prune
        let mut pending = 0usize;
        // M3（波3·片3 装配验收暴露）：ContextStats 可能先于 CallStarted 到达——
        // TurnSession::begin → record_context_stats 在调 LLM 之前执行；未匹配到聚合
        // entry 的 context_bytes 暂存于此，CallStarted 分支补挂到 llm_calls
        let mut pending_ctx: HashMap<String, i64> = HashMap::new();

        loop {
            tokio::select! {
                biased;
                ev = ev_rx.recv() => {
                    let Some(ev) = ev else { break }; // 所有句柄 drop → 退出
                    apply_event(
                        &ev,
                        &mut agg,
                        &mut daily,
                        &mut tool_rows,
                        &mut section_rows,
                        &mut turn_rows,
                        &mut pending_ctx,
                    );
                    pending += 1;
                    if pending >= batch {
                        flush(
                            &db,
                            &mut agg,
                            &mut daily,
                            &mut tool_rows,
                            &mut section_rows,
                            &mut turn_rows,
                            &mut last_prune,
                        )
                        .await;
                        pending = 0;
                    }
                }
                _ = tokio::time::sleep(interval) => {
                    flush(
                            &db,
                            &mut agg,
                            &mut daily,
                            &mut tool_rows,
                            &mut section_rows,
                            &mut turn_rows,
                            &mut last_prune,
                        )
                        .await;
                    pending = 0;
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(FlushCmd::FlushNow(ack)) => {
                            flush(
                            &db,
                            &mut agg,
                            &mut daily,
                            &mut tool_rows,
                            &mut section_rows,
                            &mut turn_rows,
                            &mut last_prune,
                        )
                        .await;
                            pending = 0;
                            let _ = ack.send(());
                        }
                        Some(FlushCmd::Shutdown) => {
                            flush(
                            &db,
                            &mut agg,
                            &mut daily,
                            &mut tool_rows,
                            &mut section_rows,
                            &mut turn_rows,
                            &mut last_prune,
                        )
                        .await;
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

/// 聚合条目：行 + 日聚合记账状态（防止重放/翻转导致日聚合撒谎）
struct AggEntry {
    row: LlmCallAgg,
    /// 该请求已按哪种终态记入日聚合（Open = 未记账）
    daily: DailyState,
}

/// 日聚合记账状态（评审修订 #1 的纠偏机）：错误被成功覆盖时 error_count 自动回退。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DailyState {
    #[default]
    Open,
    Error,
    Done,
    Aborted,
}

/// 事件 → 聚合态（llm_calls 终态 + 日聚合增量 + 工具台账行）
fn apply_event(
    ev: &MetricEvent,
    agg: &mut HashMap<String, AggEntry>,
    daily: &mut HashMap<String, DailyDelta>,
    tool_rows: &mut Vec<LlmToolCallRow>,
    section_rows: &mut Vec<LlmContextSectionRow>,
    turn_rows: &mut Vec<LlmTurnRow>,
    pending_ctx: &mut HashMap<String, i64>,
) {
    match ev {
        MetricEvent::CallStarted {
            request_id,
            provider,
            model,
            started_at,
            stage,
        } => {
            // 重试的第 2+ 次 attempt 也发 CallStarted：entry 已存在则保留首条（不覆盖）
            let day = started_at.get(..10).unwrap_or("").to_string();
            let is_new = !agg.contains_key(request_id);
            agg.entry(request_id.clone())
                .or_insert_with(|| {
                    let mut a = LlmCallAgg::new(request_id, provider, model, started_at, &day);
                    a.stage = stage.clone();
                    AggEntry {
                        row: a,
                        daily: DailyState::Open,
                    }
                });
            // M3（波3·片3）：ContextStats 先到时（真实装配顺序 record_context_stats →
            // CallStarted）补挂暂存的 context_bytes
            if is_new {
                if let Some(bytes) = pending_ctx.remove(request_id) {
                    if let Some(e) = agg.get_mut(request_id) {
                        e.row.context_bytes = Some(bytes);
                    }
                }
            }
        }
        MetricEvent::Ttft {
            request_id,
            ttft_ms,
        } => {
            if let Some(e) = agg.get_mut(request_id) {
                e.row.ttft_ms = Some(*ttft_ms);
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
            let Some(e) = agg.get_mut(request_id) else {
                return;
            };
            // round_limit 终态后不应再有 CallFinished（循环已退出；防御时序错乱）
            if e.row.finish_reason == "round_limit" {
                return;
            }
            let was_error = e.row.finish_reason == "error";
            let was_aborted = e.row.finish_reason == "aborted";
            e.row.finish_reason = if *aborted { "aborted" } else { "done" }.into();
            e.row.duration_ms = Some(*duration_ms);
            e.row.total_tokens = Some(*total_tokens);
            e.row.cached_tokens = Some(*cached_tokens);
            e.row.usage_raw = usage_raw.clone();
            e.row.terminal = true;

            let d = daily.entry(e.row.day.clone()).or_default();
            match e.daily {
                DailyState::Open => {
                    // 首次终态：完整记账
                    d.total_calls += 1;
                    d.total_tokens += *total_tokens as i64;
                    d.cached_tokens += *cached_tokens as i64;
                    d.duration_sum_ms += *duration_ms;
                    if *aborted {
                        d.aborted_count += 1;
                    }
                    if let Some(t) = e.row.ttft_ms {
                        d.ttft_sum_ms += t;
                        d.ttft_count += 1;
                    }
                }
                DailyState::Error => {
                    // 翻转：成功覆盖错误（评审修订 #1）——错误计数回退，补记成功态字段
                    d.error_count -= 1;
                    d.total_tokens += *total_tokens as i64;
                    d.cached_tokens += *cached_tokens as i64;
                    d.duration_sum_ms += *duration_ms;
                    if let Some(t) = e.row.ttft_ms {
                        d.ttft_sum_ms += t;
                        d.ttft_count += 1;
                    }
                }
                DailyState::Aborted => {
                    d.aborted_count -= 1;
                    d.total_tokens += *total_tokens as i64;
                    d.cached_tokens += *cached_tokens as i64;
                    d.duration_sum_ms += *duration_ms;
                    if let Some(t) = e.row.ttft_ms {
                        d.ttft_sum_ms += t;
                        d.ttft_count += 1;
                    }
                }
                // Done 重放：幂等，什么都不记
                DailyState::Done => {}
            }
            let _ = was_aborted;
            let _ = was_error;
            e.daily = if *aborted {
                DailyState::Aborted
            } else {
                DailyState::Done
            };
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
            let Some(e) = agg.get_mut(request_id) else {
                return;
            };
            // 错误不覆盖成功（评审修订 #1；迟到的失败事件不得把 done 打成 error）
            if e.row.finish_reason == "done" {
                return;
            }
            let was_aborted = e.row.finish_reason == "aborted";
            e.row.finish_reason = "error".into();
            e.row.duration_ms = Some(*duration_ms);
            e.row.error_stage = error_stage.clone();
            e.row.error_class = error_class.clone();
            e.row.http_status = *http_status;
            e.row.had_content = *had_content;
            e.row.retryable = *retryable;
            e.row.terminal = true;
            // 错误明细追加（审计可追溯中间态；成功覆盖后仍留在 last_error）
            let detail = format!("{error_stage}/{error_class}");
            if e.row.last_error.is_empty() {
                e.row.last_error = detail;
            } else {
                e.row.last_error.push_str("; ");
                e.row.last_error.push_str(&detail);
            }

            let d = daily.entry(e.row.day.clone()).or_default();
            match e.daily {
                DailyState::Open => {
                    d.total_calls += 1;
                    d.error_count += 1;
                    d.duration_sum_ms += *duration_ms;
                }
                DailyState::Aborted => {
                    d.aborted_count -= 1;
                    d.error_count += 1;
                    d.duration_sum_ms += *duration_ms;
                }
                DailyState::Error => {
                    // 同请求再次失败（重试链）：total 已计过，仅错误明细已追加，不再重复计
                }
                DailyState::Done => {
                    // 审计 L3 修复：状态机乱序防御。done 后收到迟到的 CallFailed 属
                    // 事件乱序（入口 finish_reason=="done" 提前 return 覆盖主要路径，
                    // 此处为兜底）——不可 unreachable! panic：flusher 是后台任务，
                    // panic 会杀死指标线程使成本账本静默停更。
                    tracing::debug!("metrics: done 状态收到迟到 CallFailed（忽略）");
                }
            }
            let _ = was_aborted;
            e.daily = DailyState::Error;
        }
        MetricEvent::RetryDecision {
            request_id,
            attempt,
            decision,
            delay_ms: _,
            model,
            next_model,
        } => {
            let Some(e) = agg.get_mut(request_id) else {
                return;
            };
            e.row.attempt = e.row.attempt.max(*attempt);
            let d = daily.entry(e.row.day.clone()).or_default();
            match decision.as_str() {
                "retry" => d.retry_count += 1,
                "fallback" => {
                    e.row.fallback_used = true;
                    // 记录最终落点模型（llm_calls.model 更新为降级后模型）
                    let trace = format!(
                        "fallback {} -> {}",
                        model.as_deref().unwrap_or("?"),
                        next_model.as_deref().unwrap_or("?")
                    );
                    if e.row.last_error.is_empty() {
                        e.row.last_error = trace;
                    } else {
                        e.row.last_error.push_str("; ");
                        e.row.last_error.push_str(&trace);
                    }
                    if let Some(nm) = next_model {
                        e.row.model = nm.clone();
                    }
                    d.fallback_count += 1;
                }
                // no_retry_* / no_fallback_* 不计数（终态已在 CallFailed 计 error）
                _ => {}
            }
        }
        MetricEvent::ToolCall {
            request_id,
            round,
            attempt,
            tool_name,
            args_json,
            result_json,
            status,
            duration_ms,
            delegated_from,
        } => {
            // 台账不依赖聚合 entry（INSERT OR IGNORE 自带防重放），直接收集待 flush
            tool_rows.push(LlmToolCallRow {
                request_id: request_id.clone(),
                round: *round,
                attempt: *attempt as i64,
                tool_name: tool_name.clone(),
                args_json: args_json.clone(),
                result_json: result_json.clone(),
                status: status.clone(),
                duration_ms: *duration_ms,
                delegated_from: delegated_from.clone(),
            });
        }
        MetricEvent::ContextStats {
            request_id,
            sections,
            context_bytes,
        } => {
            let bytes = *context_bytes as i64;
            let attached = agg
                .get_mut(request_id)
                .map(|e| e.row.context_bytes = Some(bytes))
                .is_some();
            if !attached {
                // M3（波3·片3 装配验收暴露）：真实装配顺序 record_context_stats →
                // CallStarted（TurnSession::begin → 统计 → 调 LLM）；聚合 entry 未到
                // 时暂存 context_bytes，CallStarted 分支补挂，绝不丢失
                // L5（审计修复）：有界——孤儿 request（CallStarted 永不出现，如
                // 崩溃/事件丢失）不清理会无限增长；满上限淘汰任一旧暂存腾位。
                const MAX_PENDING_CTX: usize = 4096;
                if pending_ctx.len() >= MAX_PENDING_CTX {
                    if let Some(stale) = pending_ctx.keys().next().cloned() {
                        pending_ctx.remove(&stale);
                    }
                }
                pending_ctx.insert(request_id.clone(), bytes);
            }
            for (name, bytes) in sections {
                section_rows.push(LlmContextSectionRow {
                    request_id: request_id.clone(),
                    section: name.clone(),
                    bytes: *bytes as i64,
                });
            }
        }
        MetricEvent::RoundLimit { request_id } => {
            let Some(e) = agg.get_mut(request_id) else {
                return;
            };
            // 终态：工具循环达上限（异常终止；真实消耗字段保留，仅终态标注）
            e.row.finish_reason = "round_limit".into();
            e.row.terminal = true;
            let d = daily.entry(e.row.day.clone()).or_default();
            match e.daily {
                DailyState::Open => {
                    d.total_calls += 1;
                    d.error_count += 1;
                }
                DailyState::Done => {
                    // 该轮已按 done 记账：终态改为异常（total 保持 1 次调用，error 补记；
                    // tokens/时长保留——真实消耗）
                    d.error_count += 1;
                }
                DailyState::Error | DailyState::Aborted => {}
            }
            e.daily = DailyState::Error;
        }
        MetricEvent::TurnRecorded {
            turn_id,
            started_at,
            duration_ms,
            attribution,
            is_tick,
            sections_hit,
            context_bytes,
            calls,
        } => {
            turn_rows.push(LlmTurnRow {
                turn_id: turn_id.clone(),
                started_at: started_at.clone(),
                duration_ms: Some(*duration_ms),
                attribution: attribution.clone(),
                is_tick: *is_tick,
                sections_hit: Some(*sections_hit as i64),
                context_bytes: Some(*context_bytes as i64),
                calls: *calls as i64,
            });
        }
    }
}

/// 落库：明细（终态行 UPSERT）+ 日聚合增量 + 工具台账 + 每日一次滚动淘汰。
/// 全部 best-effort：任何一步失败只 warn，不 panic、不阻塞。
async fn flush(
    db: &Db,
    agg: &mut HashMap<String, AggEntry>,
    daily: &mut HashMap<String, DailyDelta>,
    tool_rows: &mut Vec<LlmToolCallRow>,
    section_rows: &mut Vec<LlmContextSectionRow>,
    turn_rows: &mut Vec<LlmTurnRow>,
    last_prune: &mut Option<String>,
) {
    // 1) 明细：只写 terminal 行（UPSERT 幂等；行保留在内存供翻转纠偏）
    let terminal: Vec<LlmCallAgg> = agg
        .values()
        .filter(|e| e.row.terminal)
        .map(|e| e.row.clone())
        .collect();
    if !terminal.is_empty() {
        if let Err(err) = upsert_calls_batch(db, &terminal) {
            tracing::warn!(%err, "[llm_metrics] 明细落库失败（best-effort，跳过）");
        }
    }

    // 2) 日聚合：增量累加（ON CONFLICT +=；L4 修复：失败保留 pending 待下轮重试——
    // 日聚合是预算闸门的账本，失败即丢会使 budget 闸门失守；同 day 后续有新增量时合并累加）
    let days: Vec<(String, DailyDelta)> = daily.drain().collect();
    for (day, delta) in days {
        if let Err(err) = upsert_daily(db, &day, &delta) {
            tracing::warn!(%err, "[llm_metrics] 日聚合落库失败（保留待重试）");
            let merged = daily.entry(day).or_default();
            merged.total_calls += delta.total_calls;
            merged.error_count += delta.error_count;
            merged.retry_count += delta.retry_count;
            merged.fallback_count += delta.fallback_count;
            merged.aborted_count += delta.aborted_count;
            merged.total_tokens += delta.total_tokens;
            merged.cached_tokens += delta.cached_tokens;
            merged.ttft_sum_ms += delta.ttft_sum_ms;
            merged.ttft_count += delta.ttft_count;
            merged.duration_sum_ms += delta.duration_sum_ms;
        }
    }

    // 3) 工具台账（M2；L4：失败保留 pending 待重试，成功才清空）
    if !tool_rows.is_empty() {
        if let Err(err) = upsert_tool_calls_batch(db, tool_rows) {
            tracing::warn!(%err, "[llm_metrics] 工具台账落库失败（保留待重试）");
        } else {
            tool_rows.clear();
        }
    }

    // 3.5) 上下文 section 明细（M3；INSERT OR IGNORE 防重放；L4：失败保留待重试）
    if !section_rows.is_empty() {
        if let Err(err) = upsert_context_sections_batch(db, section_rows) {
            tracing::warn!(%err, "[llm_metrics] section 明细落库失败（保留待重试）");
        } else {
            section_rows.clear();
        }
    }

    // 3.6) turn 级记录（M3；L4：失败保留待重试）
    if !turn_rows.is_empty() {
        if let Err(err) = upsert_turns_batch(db, turn_rows) {
            tracing::warn!(%err, "[llm_metrics] turn 记录落库失败（保留待重试）");
        } else {
            turn_rows.clear();
        }
    }

    // 4) 每日一次明细滚动淘汰（对齐 2 万行上限；聚合永久）
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    if last_prune.as_deref() != Some(today.as_str()) {
        if let Err(err) = prune_detail(db, DETAIL_KEEP_ROWS) {
            tracing::warn!(%err, "[llm_metrics] 明细滚动淘汰失败（跳过）");
        }
        *last_prune = Some(today);
    }

    // 5) 内存上限：优先淘汰已记账的终态行（它们已落库，只剩翻转纠偏价值）
    if agg.len() > AGG_CAP {
        let evict: Vec<String> = agg
            .iter()
            .filter(|(_, e)| e.row.terminal && e.daily != DailyState::Open)
            .map(|(k, _)| k.clone())
            .take(agg.len() - AGG_CAP)
            .collect();
        for k in evict {
            agg.remove(&k);
        }
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

// ─────────────────────────────────────────────────────────────
// M3：注入上下文统计（injector 输出 → section 命中 + 总字节数）
// ─────────────────────────────────────────────────────────────

/// M3 上下文统计结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextStats {
    /// 非空 section 数量（20 个候选 section）
    pub sections_hit: usize,
    /// 上下文渲染总字节数（Debug 序列化近似；供缓存友好化分析）
    pub context_bytes: usize,
    /// section 名 → 字节数（命中明细；供「哪些常空/常满」裁剪分析）
    pub sections: Vec<(String, usize)>,
}

/// 统计一次注入输出的 section 命中数与总字节数（M3）。
/// 纯函数，不依赖 DB；LLM 轮接线后由 turn 管线调用，经
/// [`MetricEvent::ContextStats`] 记入 `llm_calls.context_bytes`（与 M1 表 JOIN 可用）。
pub fn compute_context_stats(out: &crate::memory::injector::InjectorOutput) -> ContextStats {
    let mut sections_hit = 0usize;
    let mut context_bytes = 0usize;
    let mut sections: Vec<(String, usize)> = Vec::new();
    macro_rules! section {
        ($name:expr, $present:expr, $fmt:expr $(, $arg:expr)*) => {
            if $present {
                sections_hit += 1;
                let bytes = format!($fmt, $($arg),*).len();
                context_bytes += bytes;
                sections.push(($name.to_string(), bytes));
            }
        };
    }
    section!("memories", !out.memories.is_empty(), "{:?}", out.memories);
    section!("active_policies", !out.active_policies.is_empty(), "{:?}", out.active_policies);
    section!("recall_memories", !out.recall_memories.is_empty(), "{:?}", out.recall_memories);
    section!(
        "conversation_window",
        !out.conversation_window.is_empty(),
        "{:?}",
        out.conversation_window
    );
    section!("person_memory", out.person_memory.is_some(), "{:?}", out.person_memory);
    section!(
        "user_profile",
        !out.user_profile.as_deref().unwrap_or("").is_empty(),
        "{:?}",
        out.user_profile
    );
    section!("directions", !out.directions.is_empty(), "{:?}", out.directions);
    section!("constraints", !out.constraints.is_empty(), "{:?}", out.constraints);
    section!(
        "thought",
        !out.thought.as_deref().unwrap_or("").is_empty(),
        "{:?}",
        out.thought
    );
    section!("task_knowledge", !out.task_knowledge.is_empty(), "{:?}", out.task_knowledge);
    section!("tools", !out.tools.is_empty(), "{:?}", out.tools);
    section!("action_log", !out.action_log.is_empty(), "{:?}", out.action_log);
    section!(
        "prefetched_items",
        !out.prefetched_items.is_empty(),
        "{:?}",
        out.prefetched_items
    );
    section!(
        "ui_signal_summary",
        !out.ui_signal_summary.is_empty(),
        "{:?}",
        out.ui_signal_summary
    );
    section!("temporal_recall", out.temporal_recall.is_some(), "{:?}", out.temporal_recall);
    section!(
        "self_perception",
        !out.self_perception.as_deref().unwrap_or("").is_empty(),
        "{:?}",
        out.self_perception
    );
    section!(
        "self_snapshot",
        !out.self_snapshot.as_deref().unwrap_or("").is_empty(),
        "{:?}",
        out.self_snapshot
    );
    section!("self_evolution", !out.self_evolution.is_empty(), "{:?}", out.self_evolution);
    section!(
        "browser_runtime_text",
        !out.browser_runtime_text
            .as_deref()
            .unwrap_or("")
            .is_empty(),
        "{:?}",
        out.browser_runtime_text
    );
    section!(
        "weather_runtime_text",
        !out.weather_runtime_text
            .as_deref()
            .unwrap_or("")
            .is_empty(),
        "{:?}",
        out.weather_runtime_text
    );
    ContextStats {
        sections_hit,
        context_bytes,
        sections,
    }
}


/// P3-1 数据驱动缓存友好化：基于历史命中统计重排上下文 section 顺序。
/// `history`：(section 名, 字节波动率 std/mean)。波动率低 → 内容稳定 → 应前置；
/// 波动率高 → 内容常变 → 应后置（避免打断 prompt 前缀命中）。
/// 无历史数据（接线前）时退化为静态分级（对齐 CACHE_FRIENDLY_ORDER）。
/// 返回按「稳定优先」排列的 section 名列表；未知 section 按稳定级 9 置尾。
pub fn relocate_sections(history: &[(String, f64)]) -> Vec<String> {
    // 静态稳定级（0 = 最稳定；与 injector_format::CACHE_FRIENDLY_ORDER 一致）
    let level = |name: &str| -> u32 {
        match name {
            "self_evolution" | "self_perception" | "constraints" | "active_policies"
            | "person" | "user_profile" | "task" | "thread" | "threads_background"
            | "task_knowledge" => 0,
            "self_snapshot" => 1,
            "temporal" | "memories" | "directions" | "extra" => 2,
            _ => 9,
        }
    };
    let mut items: Vec<(u32, f64, String)> = history
        .iter()
        .map(|(name, vol)| (level(name), if vol.is_finite() { vol.abs() } else { f64::MAX }, name.clone()))
        .collect();
    // 补上静态表中未出现在历史里的 section（波动率按最大 → 排同级别末尾）
    let known: std::collections::HashSet<&str> = history.iter().map(|(n, _)| n.as_str()).collect();
    for name in crate::memory::injector_format::CACHE_FRIENDLY_ORDER {
        if !known.contains(*name) {
            items.push((level(name), f64::MAX, (*name).to_string()));
        }
    }
    items.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)));
    items.into_iter().map(|(_, _, n)| n).collect()
}

/// M3：一次 turn 的观测会话（意识循环接线后用）：
/// 用法：begin → record_context_stats(&injection) → 调 LLM（StreamContext.request_id
/// 用 TurnSession::request_id）→ finish(attribution, is_tick, calls)；
/// 事件经 flusher 落 llm_calls / llm_context_sections / llm_turns 三张表。
pub struct TurnSession {
    request_id: String,
    started_at_iso: String,
    started: std::time::Instant,
    collector: MetricsCollector,
    sections_hit: usize,
    context_bytes: usize,
}

impl TurnSession {
    /// 开启一次 turn 观测（生成稳定 request_id，供本 turn 所有 LLM 调用共享）
    pub fn begin(collector: MetricsCollector) -> Self {
        Self {
            request_id: new_request_id(),
            started_at_iso: chrono::Local::now().to_rfc3339(),
            started: std::time::Instant::now(),
            collector,
            sections_hit: 0,
            context_bytes: 0,
        }
    }

    /// turn 的稳定 ID（传给 StreamContext.request_id，让 llm_calls 与 turn 关联）
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// 注入上下文统计：算 section 命中 + 总字节，emit ContextStats（llm_calls.context_bytes）
    pub fn record_context_stats(
        &mut self,
        out: &crate::memory::injector::InjectorOutput,
    ) -> ContextStats {
        let stats = compute_context_stats(out);
        self.sections_hit = stats.sections_hit;
        self.context_bytes = stats.context_bytes;
        self.collector.record(MetricEvent::ContextStats {
            request_id: self.request_id.clone(),
            sections: stats.sections.clone(),
            context_bytes: stats.context_bytes,
        });
        stats
    }

    /// turn 收尾：emit TurnRecorded（归属 + 是否 TICK + 总耗时 + 上下文统计）
    pub fn finish(self, attribution: &str, is_tick: bool, calls: u32) {
        self.collector.record(MetricEvent::TurnRecorded {
            turn_id: self.request_id.clone(),
            started_at: self.started_at_iso.clone(),
            duration_ms: self.started.elapsed().as_millis() as i64,
            attribution: attribution.to_string(),
            is_tick,
            sections_hit: self.sections_hit,
            context_bytes: self.context_bytes,
            calls,
        });
    }
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
            stage: String::new(),
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
        // 幂等验证：同 request_id 重放结束事件 → 仍只有一行、日聚合不重复累加
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
            stage: String::new(),
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
            stage: String::new(),
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
        // 终态覆盖为 done，attempt 保留 max（RetryDecision attempt=1）
        assert_eq!(reason, "done");
        assert_eq!(attempt, 1);
        // 日聚合：1 次调用（成功口径）、错误回退为 0、重试计 1
        assert_eq!(err_count, 0);
        assert_eq!(retry_count, 1);
    }

    #[tokio::test]
    async fn cross_flush_retry_flip_corrects_daily() {
        let db = test_db();
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);
        let rid = new_request_id();

        // attempt1 失败 → 先 flush（DB 落 error 行、日聚合 error=1）
        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-10T10:00:00+08:00".into(),
            stage: String::new(),
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
        flusher.flush_now().await;
        let reason_before: String = db
            .conn()
            .query_row(
                "SELECT finish_reason FROM llm_calls WHERE request_id = ?1",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason_before, "error");

        // attempt2 成功 → 再 flush（同 request_id 覆盖为 done，日聚合 error 回退）
        col.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 3200,
            total_tokens: 256,
            cached_tokens: 64,
            usage_raw: "{}".into(),
            aborted: false,
        });
        flusher.flush_now().await;

        let (reason, err_count, total_calls): (String, i64, i64) = db
            .conn()
            .query_row(
                "SELECT c.finish_reason,
                        (SELECT error_count FROM llm_metrics_daily),
                        (SELECT total_calls FROM llm_metrics_daily)
                 FROM llm_calls c WHERE request_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(reason, "done", "跨 flush 的成功必须覆盖已落库的错误");
        assert_eq!(err_count, 0, "日聚合错误计数必须回退");
        assert_eq!(total_calls, 1, "总调用数不重复");
    }

    #[tokio::test]
    async fn failed_twice_counts_error_once() {
        let db = test_db();
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);
        let rid = new_request_id();

        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-10T10:00:00+08:00".into(),
            stage: String::new(),
        });
        // 两次失败（重试链中途）
        for i in 0..2 {
            col.record(MetricEvent::CallFailed {
                request_id: rid.clone(),
                duration_ms: 800 + i * 100,
                error_stage: "stream".into(),
                error_class: "idle_timeout".into(),
                http_status: None,
                had_content: false,
                retryable: true,
            });
            col.record(MetricEvent::RetryDecision {
                request_id: rid.clone(),
                attempt: (i + 1) as u32,
                decision: "retry".into(),
                delay_ms: 800,
                model: None,
                next_model: None,
            });
        }
        // 最终仍失败（不可重试错误）→ 终态 error
        col.record(MetricEvent::CallFailed {
            request_id: rid.clone(),
            duration_ms: 1000,
            error_stage: "http".into(),
            error_class: "http".into(),
            http_status: Some(500),
            had_content: false,
            retryable: true,
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
        assert_eq!(reason, "error");
        assert_eq!(attempt, 2, "attempt 取 MAX（两次 RetryDecision）");
        assert_eq!(err_count, 1, "失败→失败只计一次错误");
        assert_eq!(retry_count, 2);
        // 错误明细追加（审计可追溯）
        let last_error: String = db
            .conn()
            .query_row(
                "SELECT last_error FROM llm_calls WHERE request_id = ?1",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(last_error.contains("stream/idle_timeout; stream/idle_timeout; http/http"));
    }

    #[tokio::test]
    async fn tool_ledger_rows_persist_with_attempt_dimension() {
        let db = test_db();
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);
        let rid = new_request_id();

        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-10T10:00:00+08:00".into(),
            stage: String::new(),
        });
        for attempt in 1..=2u32 {
            col.record(MetricEvent::ToolCall {
                request_id: rid.clone(),
                round: 0,
                attempt,
                tool_name: "web_search".into(),
                args_json: r#"{"query":"rust"}"#.into(),
                result_json: "ok".into(),
                status: "ok".into(),
                duration_ms: 5,
                delegated_from: String::new(),
            });
        }
        flusher.flush_now().await;

        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM llm_tool_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "attempt 维度下两次合法调用都保留");
        let (round, attempt, status): (i64, i64, String) = db
            .conn()
            .query_row(
                "SELECT round, attempt, status FROM llm_tool_calls WHERE attempt = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(round, 0);
        assert_eq!(attempt, 2);
        assert_eq!(status, "ok");
    }

    #[test]
    fn relocate_sections_orders_by_stability_then_volatility() {
        // 无历史 → 静态分级顺序（constraints 等稳定段前置）
        let no_hist = relocate_sections(&[]);
        let idx = |n: &str| no_hist.iter().position(|s| s == n).unwrap();
        assert!(idx("constraints") < idx("memories"));
        assert!(idx("extra") > idx("memories"));

        // 有历史 → 同级别内波动率低者前置
        let hist = vec![
            ("memories".to_string(), 3.0),
            ("directions".to_string(), 1.0),
            ("constraints".to_string(), 0.1),
            ("extra".to_string(), 5.0),
        ];
        let ordered = relocate_sections(&hist);
        let i = |n: &str| ordered.iter().position(|s| s == n).unwrap();
        assert!(i("constraints") < i("memories"), "低波动 constraints 前置");
        assert!(i("directions") < i("memories"), "directions 波动低于 memories 应前置");
        assert!(i("extra") > i("memories"), "extra 波动最高应最后");

        // 未知 section 置尾
        let with_unknown = relocate_sections(&[("mystery_section".to_string(), 0.0)]);
        let last = with_unknown.last().unwrap();
        assert_eq!(last, "mystery_section");
    }

    #[test]
    fn relocate_sections_handles_nan_volatility() {
        let ordered = relocate_sections(&[("memories".to_string(), f64::NAN)]);
        assert!(ordered.iter().any(|s| s == "memories"));
    }

    #[test]
    fn context_stats_counts_nonempty_sections() {
        use crate::memory::injector::InjectorOutput;
        let empty = InjectorOutput::default();
        let s = compute_context_stats(&empty);
        assert_eq!(s.sections_hit, 0);
        assert_eq!(s.context_bytes, 0);
        assert!(s.sections.is_empty());

        let mut out = InjectorOutput::default();
        out.directions.push("用 manage_api_capability 配置".into());
        out.tools.push("web_search".into());
        out.self_evolution = "进化上下文".into();
        let s2 = compute_context_stats(&out);
        assert_eq!(s2.sections_hit, 3);
        assert!(s2.context_bytes > 0);
        assert_eq!(s2.sections.len(), 3);
        assert!(s2.sections.iter().any(|(n, _)| n == "directions"));
    }

    #[tokio::test]
    async fn round_limit_marks_terminal_finish_reason() {
        let db = test_db();
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);
        let rid = new_request_id();
        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-10T10:00:00+08:00".into(),
            stage: String::new(),
        });
        col.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 3000,
            total_tokens: 100,
            cached_tokens: 0,
            usage_raw: "{}".into(),
            aborted: false,
        });
        // 循环走满上限 → round_limit 终态（把已按 done 记账的口径纠偏为异常）
        col.record(MetricEvent::RoundLimit {
            request_id: rid.clone(),
        });
        flusher.flush_now().await;

        let (reason, err_count, total_calls): (String, i64, i64) = db
            .conn()
            .query_row(
                "SELECT c.finish_reason,
                        (SELECT error_count FROM llm_metrics_daily),
                        (SELECT total_calls FROM llm_metrics_daily)
                 FROM llm_calls c WHERE request_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(reason, "round_limit");
        assert_eq!(err_count, 1, "round_limit 计入异常口径");
        assert_eq!(total_calls, 1, "总调用数不重复");
    }

    #[tokio::test]
    async fn round_limit_does_not_downgrade_done_across_flush() {
        // L6（审计修复）：done 先落库，round_limit 后到 → 成功终态不可被异常截断降级
        let db = test_db();
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);
        let rid = new_request_id();
        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-10T10:00:00+08:00".into(),
            stage: String::new(),
        });
        col.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 3000,
            total_tokens: 100,
            cached_tokens: 0,
            usage_raw: "{}".into(),
            aborted: false,
        });
        flusher.flush_now().await;
        let before: String = db
            .conn()
            .query_row(
                "SELECT finish_reason FROM llm_calls WHERE request_id = ?1",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, "done");

        col.record(MetricEvent::RoundLimit {
            request_id: rid.clone(),
        });
        flusher.flush_now().await;
        let (after, err_count): (String, i64) = db
            .conn()
            .query_row(
                "SELECT c.finish_reason, (SELECT error_count FROM llm_metrics_daily)
                 FROM llm_calls c WHERE request_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, "done", "跨 flush 的 round_limit 不得把已落库的 done 降级（L6）");
        assert_eq!(err_count, 1, "日聚合错误计数必须 +1");
    }

    #[tokio::test]
    async fn m3_context_and_turn_pipeline_joins_with_calls() {
        // M3 验收：一次 turn 的上下文统计 + turn 级记录与 llm_calls JOIN 可用
        let db = test_db();
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);
        let rid = new_request_id();
        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-10T10:00:00+08:00".into(),
            stage: "run_turn".into(),
        });
        col.record(MetricEvent::ContextStats {
            request_id: rid.clone(),
            sections: vec![
                ("memories".to_string(), 512),
                ("directions".to_string(), 128),
                ("self_evolution".to_string(), 64),
            ],
            context_bytes: 704,
        });
        col.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 2500,
            total_tokens: 120,
            cached_tokens: 40,
            usage_raw: "{}".into(),
            aborted: false,
        });
        col.record(MetricEvent::TurnRecorded {
            turn_id: rid.clone(),
            started_at: "2026-08-10T10:00:00+08:00".into(),
            duration_ms: 2600,
            attribution: "continued".into(),
            is_tick: false,
            sections_hit: 3,
            context_bytes: 704,
            calls: 1,
        });
        flusher.flush_now().await;

        // llm_calls.stage + context_bytes 落库
        let (stage, ctx_bytes): (String, Option<i64>) = db
            .conn()
            .query_row(
                "SELECT stage, context_bytes FROM llm_calls WHERE request_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stage, "run_turn");
        assert_eq!(ctx_bytes, Some(704));

        // section 明细与 llm_calls JOIN 可用（M3 验收核心）
        let joins: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM llm_context_sections s
                 JOIN llm_calls c ON c.request_id = s.request_id
                 WHERE c.request_id = ?1",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(joins, 3, "3 个 section 明细都应 JOIN 到 llm_calls");
        let total_bytes: i64 = db
            .conn()
            .query_row(
                "SELECT SUM(bytes) FROM llm_context_sections WHERE request_id = ?1",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total_bytes, 704, "section 字节和 = context_bytes");

        // turn 级记录齐全
        let (attribution, is_tick, sections_hit, calls): (String, i64, Option<i64>, i64) = db
            .conn()
            .query_row(
                "SELECT attribution, is_tick, sections_hit, calls FROM llm_turns WHERE turn_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(attribution, "continued");
        assert_eq!(is_tick, 0);
        assert_eq!(sections_hit, Some(3));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn context_stats_before_started_still_attaches_bytes() {
        // M3 回归（波3·片3 装配验收暴露）：真实装配顺序 record_context_stats →
        // CallStarted（TurnSession::begin → 统计 → 调 LLM），context_bytes 不得丢失
        let db = test_db();
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);
        let rid = new_request_id();
        col.record(MetricEvent::ContextStats {
            request_id: rid.clone(),
            sections: vec![("directions".to_string(), 22), ("tools".to_string(), 14)],
            context_bytes: 36,
        });
        col.record(MetricEvent::CallStarted {
            request_id: rid.clone(),
            provider: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            started_at: "2026-08-12T10:00:00+08:00".into(),
            stage: "interactive".into(),
        });
        col.record(MetricEvent::CallFinished {
            request_id: rid.clone(),
            duration_ms: 1200,
            total_tokens: 80,
            cached_tokens: 20,
            usage_raw: "{}".into(),
            aborted: false,
        });
        flusher.flush_now().await;

        let ctx_bytes: Option<i64> = db
            .conn()
            .query_row(
                "SELECT context_bytes FROM llm_calls WHERE request_id = ?1",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ctx_bytes, Some(36), "先到的 ContextStats 必须补挂到 llm_calls");
    }

    #[tokio::test]
    async fn turn_session_records_context_and_finish() {
        let db = test_db();
        let (col, flusher) = init_with(db.clone(), Duration::from_secs(60_000), 10_000);
        let mut session = TurnSession::begin(col.clone());
        let rid = session.request_id().to_string();
        let stats = session.record_context_stats(&crate::memory::injector::InjectorOutput::default());
        assert_eq!(stats.sections_hit, 0);
        session.finish("created", true, 1);
        flusher.flush_now().await;

        let (n, is_tick): (i64, i64) = db
            .conn()
            .query_row(
                "SELECT COUNT(*), MAX(is_tick) FROM llm_turns WHERE turn_id = ?1",
                [&rid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "turn 记录应恰 1 行（turn_id 主键防重放）");
        assert_eq!(is_tick, 1, "TICK 轮应标记 is_tick");
        let sec_n: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM llm_context_sections WHERE request_id = ?1",
                [&rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sec_n, 0, "空注入无 section 明细");
    }

    #[test]
    fn request_id_unique_and_monotonic() {
        let a = new_request_id();
        let b = new_request_id();
        assert_ne!(a, b);
        assert!(a.starts_with("llm-"));
    }
}
