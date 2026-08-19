//! LLM 指标仓储（P0 观测层，M1 + M4 周报）。
//! 风格对齐其余 repository：函数第一参数 `&Db`；短事务；best-effort 由调用方（flusher）决定。
//!
//! 评审修订语义（DELIBERATION_FINAL_PLAN.md §5.2）：
//! - `llm_calls` 写入 = **UPSERT**（单条 SQL，`ON CONFLICT(request_id) DO UPDATE`）：
//!   成功终态覆盖错误终态（`WHERE` 守卫保证错误不覆盖成功）、`attempt` 取 MAX；
//! - `llm_tool_calls` 唯一键含 **attempt 维度** `(request_id, round, attempt, tool_name)`：
//!   重试路径下同一 round 的合法调用不被 IGNORE 误伤；`delegated_from` 预留给协作信任账本。

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
    /// M3：调用阶段（run_turn / tool_loop / wakeup / startup；首条 attempt 定）
    pub stage: String,
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
    /// M3：注入上下文总字节数（injector 输出统计；NULL = 未上报）
    pub context_bytes: Option<i64>,
    /// flusher 内部：已收到终态（可落库）
    pub terminal: bool,
}

impl LlmCallAgg {
    pub fn new(request_id: &str, provider: &str, model: &str, started_at: &str, day: &str) -> Self {
        Self {
            request_id: request_id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            started_at: started_at.to_string(),
            day: day.to_string(),
            stage: String::new(),
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
            context_bytes: None,
            terminal: false,
        }
    }
}

/// llm_tool_calls 行（M2 工具台账；键含 attempt 维度）
#[derive(Debug, Clone)]
pub struct LlmToolCallRow {
    pub request_id: String,
    pub round: i64,
    pub attempt: i64,
    pub tool_name: String,
    pub args_json: String,
    pub result_json: String,
    /// ok | error | tripped
    pub status: String,
    pub duration_ms: i64,
    /// 协作信任账本：发起委托的上级 agent（主 agent 直调为 NULL/空串）
    pub delegated_from: String,
}

/// llm_context_sections 行（M3 section 命中明细；UNIQUE(request_id, section) 防重放）
#[derive(Debug, Clone)]
pub struct LlmContextSectionRow {
    pub request_id: String,
    pub section: String,
    pub bytes: i64,
}

/// llm_turns 行（M3 turn 级记录；turn_id = run_turn 主调用的 request_id）
#[derive(Debug, Clone)]
pub struct LlmTurnRow {
    pub turn_id: String,
    pub started_at: String,
    pub duration_ms: Option<i64>,
    /// created | continued | resumed | noop
    pub attribution: String,
    pub is_tick: bool,
    pub sections_hit: Option<i64>,
    pub context_bytes: Option<i64>,
    pub calls: i64,
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

/// 批量 upsert llm_calls（**单条 UPSERT SQL**，评审修订 #1）：
/// - `ON CONFLICT(request_id) DO UPDATE`：同一逻辑请求的多次 flush 幂等，终态后写覆盖；
/// - `attempt = MAX(旧, 新)`：重试次数只增不减；
/// - `WHERE excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'`：
///   成功覆盖错误；错误**不**覆盖成功（错误永远追加在 last_error，终态以成功为准）。
pub fn upsert_calls_batch(db: &Db, rows: &[LlmCallAgg]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    db.transaction(|tx| {
        let mut stmt = tx.prepare(
            "INSERT INTO llm_calls
               (request_id, provider, model, started_at, ttft_ms, duration_ms, total_tokens,
                cached_tokens, usage_raw, finish_reason, error_stage, error_class, http_status,
                had_content, retryable, attempt, last_error, fallback_used, context_bytes,
                stage, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,
                     strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(request_id) DO UPDATE SET
               ttft_ms       = COALESCE(excluded.ttft_ms, llm_calls.ttft_ms),
               duration_ms   = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.duration_ms ELSE llm_calls.duration_ms END,
               total_tokens  = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.total_tokens ELSE llm_calls.total_tokens END,
               cached_tokens = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.cached_tokens ELSE llm_calls.cached_tokens END,
               usage_raw     = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.usage_raw ELSE llm_calls.usage_raw END,
               finish_reason = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.finish_reason ELSE llm_calls.finish_reason END,
               error_stage   = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.error_stage ELSE llm_calls.error_stage END,
               error_class   = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.error_class ELSE llm_calls.error_class END,
               http_status   = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.http_status ELSE llm_calls.http_status END,
               had_content   = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.had_content ELSE llm_calls.had_content END,
               retryable     = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.retryable ELSE llm_calls.retryable END,
               attempt       = MAX(llm_calls.attempt, excluded.attempt),
               last_error    = CASE WHEN excluded.last_error IS NOT NULL AND excluded.last_error != ''
                                    THEN excluded.last_error ELSE llm_calls.last_error END,
               fallback_used = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                    THEN excluded.fallback_used ELSE llm_calls.fallback_used END,
               context_bytes = COALESCE(excluded.context_bytes, llm_calls.context_bytes),
               stage          = CASE WHEN excluded.finish_reason = 'done' OR llm_calls.finish_reason != 'done'
                                     THEN excluded.stage ELSE llm_calls.stage END",
        )?;
        for r in rows {
            stmt.execute(params![
                r.request_id,
                r.provider,
                r.model,
                r.started_at,
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
                r.context_bytes,
                r.stage,
            ])?;
        }
        Ok(())
    })
}

/// 批量写工具台账（审计 L1 修复：键含参数 → 同键覆盖而非吞掉）。
///
/// 原实现 `INSERT OR IGNORE`（唯一键 request_id+round+attempt+tool_name）会吞掉
/// 同轮同名工具【不同参数】的第二次调用——防重放键缺参数指纹导致结果错乱。
/// 现改为 ON CONFLICT 覆盖：同键不同参数 → 以最新一次参数/结果为准；
/// 同键同参数重试（真防重放场景）→ 幂等覆盖；attempt 变化 → 新行（重试台账保留）。
pub fn upsert_tool_calls_batch(db: &Db, rows: &[LlmToolCallRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    db.transaction(|tx| {
        let mut stmt = tx.prepare(
            "INSERT INTO llm_tool_calls
               (request_id, round, attempt, tool_name, args_json, result_json, status,
                duration_ms, delegated_from, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                     strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(request_id, round, attempt, tool_name, args_json) DO UPDATE SET
               args_json       = excluded.args_json,
               result_json     = excluded.result_json,
               status          = excluded.status,
               duration_ms     = excluded.duration_ms,
               delegated_from  = excluded.delegated_from",
        )?;
        for r in rows {
            stmt.execute(params![
                r.request_id,
                r.round,
                r.attempt,
                r.tool_name,
                r.args_json,
                r.result_json,
                r.status,
                r.duration_ms,
                r.delegated_from,
            ])?;
        }
        Ok(())
    })
}

/// P1-2 工具防重放查询：同一逻辑请求（request_id + round + tool_name + **args**）是否已有成功执行记录。
/// 审计 L1 修复：键纳入 args 参数维度——同轮同名工具不同参数必须重新执行，
/// 不得复用另一参数的结果；同参数重试（provider 完成但响应丢失）命中复用。
/// 只返回 status='ok' 的记录：error/tripped 不视为已成功执行，重试应重新执行。
pub fn find_tool_call_result(
    db: &Db,
    request_id: &str,
    round: i64,
    tool_name: &str,
    args_json: &str,
) -> Result<Option<String>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT result_json FROM llm_tool_calls
         WHERE request_id = ?1 AND round = ?2 AND tool_name = ?3 AND args_json = ?4
           AND status = 'ok'
         ORDER BY attempt DESC, rowid DESC LIMIT 1",
    )?;
    let p = params![request_id, round, tool_name, args_json];
    let mut rows = stmt.query(p)?;
    if let Some(row) = rows.next()? {
        return Ok(Some(row.get(0)?));
    }
    Ok(None)
}

/// 批量写 section 命中明细（M3；INSERT OR IGNORE + UNIQUE(request_id, section) 防重放）
pub fn upsert_context_sections_batch(db: &Db, rows: &[LlmContextSectionRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    db.transaction(|tx| {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO llm_context_sections
               (request_id, section, bytes, created_at)
             VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        )?;
        for r in rows {
            stmt.execute(params![r.request_id, r.section, r.bytes])?;
        }
        Ok(())
    })
}

/// 批量写 turn 级记录（M3；INSERT OR IGNORE + turn_id 主键防重放）
pub fn upsert_turns_batch(db: &Db, rows: &[LlmTurnRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    db.transaction(|tx| {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO llm_turns
               (turn_id, started_at, duration_ms, attribution, is_tick, sections_hit,
                context_bytes, calls, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        )?;
        for r in rows {
            stmt.execute(params![
                r.turn_id,
                r.started_at,
                r.duration_ms,
                r.attribution,
                r.is_tick,
                r.sections_hit,
                r.context_bytes,
                r.calls,
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
            total_tokens, cached_tokens, ttft_sum_ms, ttft_count, duration_sum_ms, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
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
           updated_at      = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
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

// ─────────────────────────────────────────────────────────────
// M4：周报（六指标 + 阈值信号；llm_metrics_daily 聚合，长期趋势不断档）
// ─────────────────────────────────────────────────────────────

/// 周报（M4）。六指标 + 派生率 + 阈值信号。
#[derive(Debug, Clone, Default)]
pub struct WeeklyReport {
    /// 观测窗口天数
    pub days: i64,
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
    /// 阈值信号（空 = 一切正常）
    pub signals: Vec<String>,
    /// M4：唤醒成本观测（stage='wakeup' 周窗口聚合；R7 先有账再设闸）
    pub wakeup: WakeupCost,
}

impl WeeklyReport {
    /// 错误率（%）
    pub fn error_rate_pct(&self) -> f64 {
        if self.total_calls == 0 {
            0.0
        } else {
            self.error_count as f64 / self.total_calls as f64 * 100.0
        }
    }

    /// 缓存命中率（%）：cached / total tokens
    pub fn cache_rate_pct(&self) -> f64 {
        if self.total_tokens == 0 {
            0.0
        } else {
            self.cached_tokens as f64 / self.total_tokens as f64 * 100.0
        }
    }

    /// 平均 TTFT（ms）
    pub fn avg_ttft_ms(&self) -> f64 {
        if self.ttft_count == 0 {
            0.0
        } else {
            self.ttft_sum_ms as f64 / self.ttft_count as f64
        }
    }

    /// 平均单次调用时长（ms）
    pub fn avg_duration_ms(&self) -> f64 {
        if self.total_calls == 0 {
            0.0
        } else {
            self.duration_sum_ms as f64 / self.total_calls as f64
        }
    }

    /// 平均单次调用 tokens（cost/turn 观测：周报环比指标）
    pub fn avg_tokens_per_call(&self) -> f64 {
        if self.total_calls == 0 {
            0.0
        } else {
            self.total_tokens as f64 / self.total_calls as f64
        }
    }
}

/// 生成最近 N 天周报（M4）。
/// 阈值信号（对齐 DESIGN_LLM_METRICS.md 的动作映射）：
/// cache_rate < 30% → 建议查 injector 命中计数（M3 context_bytes）
/// error_rate > 10% / retry_rate > 20% / aborted > 5% → 可靠性告警
/// avg_ttft > 3000ms → 延迟告警
/// P3-3 token 预算闸门决策。
#[derive(Debug, Clone, PartialEq)]
pub enum TokenGateDecision {
    /// 放行：周窗口已用 tokens + 剩余额度
    Allow {
        used_tokens: i64,
        remaining_tokens: i64,
    },
    /// 拦截：周窗口总 tokens 已超预算
    Blocked {
        used_tokens: i64,
        budget_tokens: i64,
    },
}

/// P3-3 基于周报的 token 预算闸门：按周窗口总 tokens（llm_metrics_daily 聚合，含所有 stage）判断是否超预算。
/// budget_tokens <= 0 表示闸门关闭（纯观测不拦截，默认安全形态）。
pub fn token_budget_gate(db: &Db, days: i64, budget_tokens: i64) -> Result<TokenGateDecision> {
    let report = weekly_report(db, days)?;
    let used = report.total_tokens;
    if budget_tokens > 0 && used >= budget_tokens {
        Ok(TokenGateDecision::Blocked {
            used_tokens: used,
            budget_tokens,
        })
    } else {
        let remaining = if budget_tokens > 0 {
            budget_tokens - used
        } else {
            i64::MAX
        };
        Ok(TokenGateDecision::Allow {
            used_tokens: used,
            remaining_tokens: remaining,
        })
    }
}

pub fn weekly_report(db: &Db, days: i64) -> Result<WeeklyReport> {
    let window = format!("-{days} days");
    let (
        total_calls,
        error_count,
        retry_count,
        fallback_count,
        aborted_count,
        total_tokens,
        cached_tokens,
        ttft_sum_ms,
        ttft_count,
        duration_sum_ms,
    ) = db.conn().query_row(
        "SELECT COALESCE(SUM(total_calls),0), COALESCE(SUM(error_count),0),
                    COALESCE(SUM(retry_count),0), COALESCE(SUM(fallback_count),0),
                    COALESCE(SUM(aborted_count),0), COALESCE(SUM(total_tokens),0),
                    COALESCE(SUM(cached_tokens),0), COALESCE(SUM(ttft_sum_ms),0),
                    COALESCE(SUM(ttft_count),0), COALESCE(SUM(duration_sum_ms),0)
             FROM llm_metrics_daily WHERE day >= date('now', ?1)",
        [&window],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
            ))
        },
    )?;

    let mut report = WeeklyReport {
        days,
        total_calls,
        error_count,
        retry_count,
        fallback_count,
        aborted_count,
        total_tokens,
        cached_tokens,
        ttft_sum_ms,
        ttft_count,
        duration_sum_ms,
        signals: Vec::new(),
        wakeup: WakeupCost::default(),
    };

    // M4：唤醒成本观测（与日聚合同窗口；llm_calls.stage='wakeup'）
    report.wakeup = wakeup_cost_weekly(db, days)?;

    if report.total_calls == 0 {
        report
            .signals
            .push("观测窗口内无 LLM 调用（LLM 轮可能尚未接线，或观测未挂载）。".into());
        return Ok(report);
    }

    if report.cache_rate_pct() < 30.0 {
        report.signals.push(format!(
            "cache_rate 偏低（{:.1}% < 30%）：建议用 M3 context_bytes + injector 命中计数定位常变段，移出 STABLE 核心（relocate_sections）。",
            report.cache_rate_pct()
        ));
    }
    if report.error_rate_pct() > 10.0 {
        report.signals.push(format!(
            "error_rate 偏高（{:.1}% > 10%）：检查 provider 状态与 error_stage 分布。",
            report.error_rate_pct()
        ));
    }
    let retry_rate = report.retry_count as f64 / report.total_calls as f64;
    if retry_rate > 0.2 {
        report.signals.push(format!(
            "retry_rate 偏高（{:.1}% > 20%）：瞬时错误增多，可能是网络/限流。",
            retry_rate * 100.0
        ));
    }
    let aborted_rate = report.aborted_count as f64 / report.total_calls as f64;
    if aborted_rate > 0.05 {
        report.signals.push(format!(
            "aborted_rate 偏高（{:.1}% > 5%）：外部中止频繁，检查 watchdog/抢占逻辑。",
            aborted_rate * 100.0
        ));
    }
    if report.avg_ttft_ms() > 3000.0 {
        report.signals.push(format!(
            "avg_ttft 偏高（{:.0}ms > 3000ms）：首 token 延迟大，检查 provider 建连与 queue。",
            report.avg_ttft_ms()
        ));
    }
    // M4：唤醒成本观测信号（R7 缓解第一步——先有账再设闸）
    if report.wakeup.calls == 0 && report.total_calls > 0 {
        report.signals.push(
            "唤醒轮 0 观测（TICK 未接线或 stage 未标注）——唤醒成本账本为空，P1-1 闸门暂不可用。"
                .into(),
        );
    }
    if report.wakeup.calls > 0 {
        let share = report.wakeup.share_of_total(report.total_tokens);
        let per_wakeup = report.wakeup.total_tokens as f64 / report.wakeup.calls as f64;
        if share > 25.0 {
            report.signals.push(format!(
                "唤醒成本占比偏高（{share:.1}% > 25%）：{} 次唤醒耗 {} tokens（均 {per_wakeup:.0}/次），建议 P1-1 合并器 + 周窗口预算闸门。",
                report.wakeup.calls, report.wakeup.total_tokens
            ));
        }
    }
    Ok(report)
}

/// 唤醒成本观测（M4；R7 风险缓解第一步）。P1-1 成本闸门 / 周窗口预算直接消费本数据。
#[derive(Debug, Clone, Default)]
pub struct WakeupCost {
    pub calls: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub ttft_sum_ms: i64,
    pub ttft_count: i64,
    pub duration_sum_ms: i64,
}

impl WakeupCost {
    pub fn cache_rate_pct(&self) -> f64 {
        if self.total_tokens == 0 {
            0.0
        } else {
            self.cached_tokens as f64 / self.total_tokens as f64 * 100.0
        }
    }

    pub fn avg_ttft_ms(&self) -> f64 {
        if self.ttft_count == 0 {
            0.0
        } else {
            self.ttft_sum_ms as f64 / self.ttft_count as f64
        }
    }

    pub fn avg_duration_ms(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.duration_sum_ms as f64 / self.calls as f64
        }
    }

    /// 唤醒轮占总 tokens 的份额（%）：周报口径「唤醒成本占比」
    pub fn share_of_total(&self, total_tokens: i64) -> f64 {
        if total_tokens == 0 {
            0.0
        } else {
            self.total_tokens as f64 / total_tokens as f64 * 100.0
        }
    }
}

/// 查询周窗口内唤醒轮成本（stage='wakeup'；llm_calls 明细，2 万行留存内窗口可用）
pub fn wakeup_cost_weekly(db: &Db, days: i64) -> Result<WakeupCost> {
    let window = format!("-{days} days");
    let (calls, total_tokens, cached_tokens, ttft_sum_ms, ttft_count, duration_sum_ms) =
        db.conn().query_row(
            "SELECT COUNT(*), COALESCE(SUM(total_tokens),0), COALESCE(SUM(cached_tokens),0),
                    COALESCE(SUM(ttft_ms),0), COUNT(ttft_ms), COALESCE(SUM(duration_ms),0)
             FROM llm_calls
             WHERE stage = 'wakeup' AND started_at >= date('now', ?1)",
            [&window],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;
    Ok(WakeupCost {
        calls,
        total_tokens,
        cached_tokens,
        ttft_sum_ms,
        ttft_count,
        duration_sum_ms,
    })
}

impl WeeklyReport {
    /// 渲染为可读周报文本（供 api / 日志 / TUI 直接展示）
    pub fn render(&self) -> String {
        let per_wakeup = if self.wakeup.calls > 0 {
            self.wakeup.total_tokens as f64 / self.wakeup.calls as f64
        } else {
            0.0
        };
        let mut s = format!(
            "LLM 周报（{} 天窗口）：\n总调用 {} ｜ 错误 {}（{:.1}%）｜ 重试 {} ｜ fallback {} ｜ aborted {}\n总 tokens {} ｜ cached {}（{:.1}%）｜ avg_ttft {:.0}ms ｜ avg_duration {:.0}ms\n唤醒成本：{} 次 ｜ {} tokens（占总 {:.1}%）｜ 均 {per_wakeup:.0}/次\n",
            self.days,
            self.total_calls,
            self.error_count,
            self.error_rate_pct(),
            self.retry_count,
            self.fallback_count,
            self.aborted_count,
            self.total_tokens,
            self.cached_tokens,
            self.cache_rate_pct(),
            self.avg_ttft_ms(),
            self.avg_duration_ms(),
            self.wakeup.calls,
            self.wakeup.total_tokens,
            self.wakeup.share_of_total(self.total_tokens),
        );
        if self.signals.is_empty() {
            s.push_str("信号：无（一切正常）\n");
        } else {
            s.push_str("信号：\n");
            for sig in &self.signals {
                s.push_str(&format!("  - {sig}\n"));
            }
        }
        s
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

    fn agg(rid: &str, reason: &str, attempt: u32) -> LlmCallAgg {
        let mut a = LlmCallAgg::new(
            rid,
            "deepseek",
            "deepseek-v4-pro",
            "2026-08-10T10:00:00+08:00",
            "2026-08-10",
        );
        a.finish_reason = reason.into();
        a.attempt = attempt;
        a.terminal = true;
        a
    }

    #[test]
    fn upsert_success_overwrites_error_attempt_takes_max() {
        let db = test_db();
        // 先落 error（attempt 1）
        upsert_calls_batch(&db, &[agg("r1", "error", 1)]).unwrap();
        // 再落 done（attempt 2，重试后成功）
        upsert_calls_batch(&db, &[agg("r1", "done", 2)]).unwrap();
        let (reason, attempt): (String, i64) = db
            .conn()
            .query_row(
                "SELECT finish_reason, attempt FROM llm_calls WHERE request_id = 'r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, "done", "成功必须覆盖错误");
        assert_eq!(attempt, 2, "attempt 必须取 MAX");
        // 仍只有一行
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM llm_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn upsert_error_does_not_overwrite_done() {
        let db = test_db();
        upsert_calls_batch(&db, &[agg("r2", "done", 1)]).unwrap();
        // 迟到的 error 终态（理论上不该发生，防御）不得把 done 打成 error
        upsert_calls_batch(&db, &[agg("r2", "error", 2)]).unwrap();
        let (reason, attempt): (String, i64) = db
            .conn()
            .query_row(
                "SELECT finish_reason, attempt FROM llm_calls WHERE request_id = 'r2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, "done", "错误不得覆盖成功");
        assert_eq!(attempt, 2, "attempt 仍取 MAX（审计不丢）");
    }

    #[test]
    fn upsert_round_limit_does_not_downgrade_done() {
        // L6（审计修复）：round_limit 是异常截断终态，不得把已落库的 done 降级
        let db = test_db();
        upsert_calls_batch(&db, &[agg("r3", "done", 1)]).unwrap();
        upsert_calls_batch(&db, &[agg("r3", "round_limit", 2)]).unwrap();
        let reason: String = db
            .conn()
            .query_row(
                "SELECT finish_reason FROM llm_calls WHERE request_id = 'r3'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason, "done", "round_limit 不得覆盖成功终态");
    }

    #[test]
    fn upsert_keeps_error_text_across_success_overwrite() {
        // L6（审计修复）：错误链只增不清——done 行 last_error 为空不得清掉历史错误文本
        let db = test_db();
        let mut a1 = agg("r4", "error", 1);
        a1.last_error = "boom: timeout".into();
        upsert_calls_batch(&db, &[a1]).unwrap();
        upsert_calls_batch(&db, &[agg("r4", "done", 2)]).unwrap();
        let (reason, last_error): (String, String) = db
            .conn()
            .query_row(
                "SELECT finish_reason, last_error FROM llm_calls WHERE request_id = 'r4'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, "done", "成功终态保持");
        assert_eq!(last_error, "boom: timeout", "错误链保留（L6）");
    }

    #[test]
    fn find_tool_call_result_returns_only_ok_replay() {
        let db = test_db();
        let row = |attempt: i64, status: &str, tool_name: &str, args: &str, result: &str| {
            LlmToolCallRow {
                request_id: "r9".into(),
                round: 3,
                attempt,
                tool_name: tool_name.into(),
                args_json: args.into(),
                result_json: result.into(),
                status: status.into(),
                duration_ms: 5,
                delegated_from: String::new(),
            }
        };
        // 先落 error（attempt 1），再落 ok（attempt 2）——模拟重试后成功
        upsert_tool_calls_batch(
            &db,
            &[row(
                1,
                "error",
                "send_message",
                r#"{"to":"u"}"#,
                r#"{"ok":false}"#,
            )],
        )
        .unwrap();
        upsert_tool_calls_batch(
            &db,
            &[row(
                2,
                "ok",
                "send_message",
                r#"{"to":"u"}"#,
                r#"{"delivered":true}"#,
            )],
        )
        .unwrap();
        // 只复用成功记录，且取最新 attempt
        let hit = find_tool_call_result(&db, "r9", 3, "send_message", r#"{"to":"u"}"#).unwrap();
        assert_eq!(hit.as_deref(), Some(r#"{"delivered":true}"#));
        // 审计 L1：同轮同名工具【不同参数】不得命中复用（必须重新执行）
        let other_args =
            find_tool_call_result(&db, "r9", 3, "send_message", r#"{"to":"v"}"#).unwrap();
        assert!(other_args.is_none(), "不同参数不应复用结果");
        // 未执行过的工具 → None
        assert!(
            find_tool_call_result(&db, "r9", 3, "web_search", r#"{"q":"x"}"#)
                .unwrap()
                .is_none()
        );
        // 只有 error 记录 → None（错误不防重放，允许重试重新执行）
        upsert_tool_calls_batch(
            &db,
            &[row(
                1,
                "error",
                "express",
                r#"{"text":"hi"}"#,
                r#"{"ok":false}"#,
            )],
        )
        .unwrap();
        assert!(
            find_tool_call_result(&db, "r9", 3, "express", r#"{"text":"hi"}"#)
                .unwrap()
                .is_none()
        );
        // 不同 round 不串键
        assert!(
            find_tool_call_result(&db, "r9", 4, "send_message", r#"{"to":"u"}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn tool_batch_keeps_distinct_args_in_same_round() {
        // H8（审计修复）：同轮同名工具不同参数必须各占一行（唯一键含 args_json 维度），
        // 否则第二次覆盖首行、重试时首调用被重复执行。
        let db = test_db();
        let row = |args: &str, result: &str| LlmToolCallRow {
            request_id: "r-h8".into(),
            round: 3,
            attempt: 1,
            tool_name: "send_message".into(),
            args_json: args.into(),
            result_json: result.into(),
            status: "ok".into(),
            duration_ms: 5,
            delegated_from: String::new(),
        };
        upsert_tool_calls_batch(&db, &[row(r#"{"to":"u"}"#, r#"{"r":1}"#)]).unwrap();
        upsert_tool_calls_batch(&db, &[row(r#"{"to":"v"}"#, r#"{"r":2}"#)]).unwrap();

        let n: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM llm_tool_calls WHERE request_id = 'r-h8'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "同轮同名不同参数应各占一行，不得覆盖");

        // 两个参数各自命中自己的结果
        assert_eq!(
            find_tool_call_result(&db, "r-h8", 3, "send_message", r#"{"to":"u"}"#)
                .unwrap()
                .as_deref(),
            Some(r#"{"r":1}"#)
        );
        assert_eq!(
            find_tool_call_result(&db, "r-h8", 3, "send_message", r#"{"to":"v"}"#)
                .unwrap()
                .as_deref(),
            Some(r#"{"r":2}"#)
        );
    }

    #[test]
    fn tool_batch_dedupes_with_attempt_dimension() {
        let db = test_db();
        let row = |attempt: i64| LlmToolCallRow {
            request_id: "r3".into(),
            round: 0,
            attempt,
            tool_name: "web_search".into(),
            args_json: "{}".into(),
            result_json: "ok".into(),
            status: "ok".into(),
            duration_ms: 5,
            delegated_from: String::new(),
        };
        upsert_tool_calls_batch(&db, &[row(1)]).unwrap();
        // 同 round 同工具、attempt 不同 → 合法第二次调用，必须保留（评审修订 #2）
        upsert_tool_calls_batch(&db, &[row(2)]).unwrap();
        // 完全重放（同 attempt）→ IGNORE 防重放
        upsert_tool_calls_batch(&db, &[row(2)]).unwrap();
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM llm_tool_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "attempt 维度下重试路径不误伤，重放仍去重");
    }

    #[test]
    fn token_budget_gate_allows_within_budget() {
        let db = test_db();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        upsert_daily(
            &db,
            &today,
            &DailyDelta {
                total_calls: 100,
                error_count: 0,
                retry_count: 0,
                fallback_count: 0,
                aborted_count: 0,
                total_tokens: 200_000,
                cached_tokens: 40_000,
                ttft_sum_ms: 0,
                ttft_count: 0,
                duration_sum_ms: 0,
            },
        )
        .unwrap();
        let d = token_budget_gate(&db, 7, 250_000).unwrap();
        match d {
            TokenGateDecision::Allow {
                used_tokens,
                remaining_tokens,
            } => {
                assert_eq!(used_tokens, 200_000);
                assert_eq!(remaining_tokens, 50_000);
            }
            _ => panic!("应放行"),
        }
    }

    #[test]
    fn token_budget_gate_blocks_when_over_budget() {
        let db = test_db();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        upsert_daily(
            &db,
            &today,
            &DailyDelta {
                total_calls: 100,
                error_count: 0,
                retry_count: 0,
                fallback_count: 0,
                aborted_count: 0,
                total_tokens: 200_000,
                cached_tokens: 40_000,
                ttft_sum_ms: 0,
                ttft_count: 0,
                duration_sum_ms: 0,
            },
        )
        .unwrap();
        let d = token_budget_gate(&db, 7, 150_000).unwrap();
        match d {
            TokenGateDecision::Blocked {
                used_tokens,
                budget_tokens,
            } => {
                assert_eq!(used_tokens, 200_000);
                assert_eq!(budget_tokens, 150_000);
            }
            _ => panic!("应拦截"),
        }
    }

    #[test]
    fn token_budget_gate_zero_budget_disabled() {
        let db = test_db();
        let d = token_budget_gate(&db, 7, 0).unwrap();
        match d {
            TokenGateDecision::Allow {
                remaining_tokens, ..
            } => {
                assert_eq!(remaining_tokens, i64::MAX);
            }
            _ => panic!("预算=0 闸门关闭，应放行"),
        }
    }

    #[test]
    fn weekly_report_aggregates_and_signals() {
        let db = test_db();
        // 构造 2 天聚合：总调用 100、错误 5、重试 10、fallback 2、aborted 1、
        // tokens 200000（cached 40000 → 20% < 30% 触发信号）、ttft 平均 2s、duration 平均 8s
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let d1 = DailyDelta {
            total_calls: 60,
            error_count: 3,
            retry_count: 6,
            fallback_count: 1,
            aborted_count: 1,
            total_tokens: 120_000,
            cached_tokens: 24_000,
            ttft_sum_ms: 120_000,
            ttft_count: 60,
            duration_sum_ms: 480_000,
        };
        let d2 = DailyDelta {
            total_calls: 40,
            error_count: 2,
            retry_count: 4,
            fallback_count: 1,
            aborted_count: 0,
            total_tokens: 80_000,
            cached_tokens: 16_000,
            ttft_sum_ms: 80_000,
            ttft_count: 40,
            duration_sum_ms: 320_000,
        };
        upsert_daily(&db, &today, &d1).unwrap();
        upsert_daily(&db, &today, &d2).unwrap(); // 同日二次 flush 累加

        let r = weekly_report(&db, 7).unwrap();
        assert_eq!(r.total_calls, 100);
        assert_eq!(r.error_count, 5);
        assert_eq!(r.retry_count, 10);
        assert_eq!(r.fallback_count, 2);
        assert_eq!(r.aborted_count, 1);
        assert_eq!(r.total_tokens, 200_000);
        assert_eq!(r.cached_tokens, 40_000);
        assert!((r.error_rate_pct() - 5.0).abs() < 1e-9);
        assert!((r.cache_rate_pct() - 20.0).abs() < 1e-9);
        assert_eq!(r.avg_ttft_ms(), 2_000.0);
        assert_eq!(r.avg_duration_ms(), 8_000.0);
        // 信号：cache<30 必触发；error 5% 不触发
        assert!(r.signals.iter().any(|s| s.contains("cache_rate 偏低")));
        assert!(!r.signals.iter().any(|s| s.contains("error_rate 偏高")));
    }

    #[test]
    fn weekly_report_empty_window_signals() {
        let db = test_db();
        let r = weekly_report(&db, 7).unwrap();
        assert_eq!(r.total_calls, 0);
        assert!(r.signals.iter().any(|s| s.contains("无 LLM 调用")));
    }

    #[test]
    fn weekly_report_includes_wakeup_cost() {
        let db = test_db();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        // 日聚合：100 调用 / 200k tokens（cached 60k → 30%）
        let d = DailyDelta {
            total_calls: 100,
            error_count: 0,
            retry_count: 0,
            fallback_count: 0,
            aborted_count: 0,
            total_tokens: 200_000,
            cached_tokens: 60_000,
            ttft_sum_ms: 0,
            ttft_count: 0,
            duration_sum_ms: 0,
        };
        upsert_daily(&db, &today, &d).unwrap();
        // 唤醒轮明细：10 次 wakeup、共 80k tokens（占总 40% → 触发信号）
        let mut w = LlmCallAgg::new(
            "wk0",
            "deepseek",
            "deepseek-v4-pro",
            "2026-08-10T09:00:00+08:00",
            &today,
        );
        w.started_at = chrono::Local::now().to_rfc3339();
        w.stage = "wakeup".into();
        w.finish_reason = "done".into();
        w.total_tokens = Some(8_000);
        w.cached_tokens = Some(1_600);
        w.ttft_ms = Some(500);
        w.duration_ms = Some(3_000);
        w.terminal = true;
        let rows: Vec<LlmCallAgg> = (0..10)
            .map(|i| {
                let mut a = w.clone();
                a.request_id = format!("wk{i}");
                a
            })
            .collect();
        upsert_calls_batch(&db, &rows).unwrap();

        let r = weekly_report(&db, 7).unwrap();
        assert_eq!(r.wakeup.calls, 10);
        assert_eq!(r.wakeup.total_tokens, 80_000);
        assert_eq!(r.wakeup.avg_ttft_ms(), 500.0);
        assert!((r.wakeup.share_of_total(r.total_tokens) - 40.0).abs() < 1e-9);
        assert!(
            r.signals.iter().any(|s| s.contains("唤醒成本占比偏高")),
            "40% > 25% 必须触发唤醒成本信号"
        );
        let text = r.render();
        assert!(text.contains("LLM 周报"));
        assert!(text.contains("唤醒成本：10 次"));
    }

    #[test]
    fn weekly_report_wakeup_no_data_still_renders() {
        let db = test_db();
        let r = weekly_report(&db, 7).unwrap();
        assert_eq!(r.wakeup.calls, 0);
        assert!(r.render().contains("唤醒成本：0 次"));
    }
}
