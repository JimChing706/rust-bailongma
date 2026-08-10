# llm_metrics 埋点设计 — P0 观测层（表结构 + 埋点位置清单）

> 基线：2026-08-10，对 sandbox/bailongma-rust（与 D:\JimChing\rust-bailongma 同版本）实证设计。
> 前置：RESEARCH_RELIABILITY_LLM_SAFETY.md 六线研判 → P0「先测量再调校」。
> 范围：只设计不落码；本文件是可直接实施的设计规格。

---

## 〇、定位与六原则

目的：让 LLM 效率调校从「盲调」变「数据驱动」；同时为「重试幂等」打地基（见原则 3）。

1. **零侵入**：不改变任何现有行为，纯旁路观测。
2. **best-effort**：写库失败只 warn，观测绝不拖垮意识循环（对齐 brain_ui_events 边界）。
3. **幂等写入**：`request_id` 是「一次逻辑请求」的稳定 ID，重试共享同一 ID；写库 `INSERT OR IGNORE`——同一请求天然只落一条。这同时是 LLM 重试幂等缺失缺口的第一块地基。
4. **低开销**：流路径上只做内存记账（<1ms/请求），落库全部走后台队列。
5. **可追溯**：request_id 贯穿 llm_calls → llm_tool_calls → turn 上下文，故障可回放。
6. **有界留存**：明细表有行数上限，聚合表永久，避免观测库无限膨胀。

---

## 一、表结构（SQLite DDL，对齐 schema.rs 风格：snake_case / 带时区偏移时间戳 / CREATE IF NOT EXISTS）

### 1. llm_calls — 请求级（一次逻辑请求 = 一组 attempt，共享 request_id）

```sql
CREATE TABLE IF NOT EXISTS llm_calls (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  request_id        TEXT NOT NULL UNIQUE,   -- UUID v4；重试共享；幂等护栏
  ts                TEXT NOT NULL,          -- ISO8601 带时区偏移（对齐 conversations.timestamp）
  provider          TEXT NOT NULL,          -- deepseek / kimi / minimax / ...
  model             TEXT NOT NULL,
  stage             TEXT NOT NULL,          -- run_turn / tool_loop / wakeup / startup / ...
  attempt           INTEGER NOT NULL DEFAULT 1,
  is_retry          INTEGER NOT NULL DEFAULT 0,
  retry_reason      TEXT,                   -- timeout / io / stream_interrupt / provider_error / ...
  downgrade_chain   TEXT,                   -- 实际降级序列 "primary→backup1→backup2"
  ttft_ms           INTEGER,                -- 首个内容 chunk 到达耗时（定义见 §二）
  total_ms          INTEGER,
  prompt_tokens     INTEGER,
  completion_tokens INTEGER,
  total_tokens      INTEGER,
  cached_tokens     INTEGER,                -- 归一化后的缓存命中 token（§二）
  cache_miss_tokens INTEGER,
  finish_reason     TEXT,                   -- stop / length / tool_calls / error / round_limit / ...
  error_code        TEXT,                   -- 失败分类（is_retryable 语义同 retry.rs）
  error_stage       TEXT,                   -- connect / headers / stream / parse / tool_loop
  had_content       INTEGER,                -- 对齐 retry.rs：流出内容后失败（不重试）
  usage_raw         TEXT,                   -- provider 原始 usage JSON（防解析漏字段）
  context_bytes     INTEGER                 -- 注入上下文近似字节数（memory/injector 报数）
);
CREATE INDEX IF NOT EXISTS idx_llm_calls_ts     ON llm_calls(ts);
CREATE INDEX IF NOT EXISTS idx_llm_calls_provider ON llm_calls(provider, ts);
```

### 2. llm_tool_calls — 工具执行台账（同时补「工具执行无台账」的观测侧）

```sql
CREATE TABLE IF NOT EXISTS llm_tool_calls (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  request_id  TEXT NOT NULL,                -- 关联 llm_calls
  round       INTEGER NOT NULL,             -- tool_loop 轮次
  tool_name   TEXT NOT NULL,
  args_json   TEXT,
  args_bytes  INTEGER,
  duration_ms INTEGER,
  status      TEXT NOT NULL,                -- ok / error / skipped / tripped
  error_code  TEXT,
  UNIQUE(request_id, round, tool_name)      -- 重放防护：同轮同工具只落一次
);
CREATE INDEX IF NOT EXISTS idx_llm_tool_calls_req ON llm_tool_calls(request_id);
```

### 3. llm_metrics_daily — 日聚合（明细表按行数滚动淘汰后的长期趋势依据）

```sql
CREATE TABLE IF NOT EXISTS llm_metrics_daily (
  day                   TEXT PRIMARY KEY,   -- YYYY-MM-DD
  calls                 INTEGER NOT NULL DEFAULT 0,
  retries               INTEGER NOT NULL DEFAULT 0,
  downgrades            INTEGER NOT NULL DEFAULT 0,
  tool_calls            INTEGER NOT NULL DEFAULT 0,
  tripped               INTEGER NOT NULL DEFAULT 0,  -- 熔断触发次数
  total_prompt_tokens   INTEGER NOT NULL DEFAULT 0,
  total_completion_tokens INTEGER NOT NULL DEFAULT 0,
  cached_tokens         INTEGER NOT NULL DEFAULT 0,
  ttft_avg_ms           INTEGER,
  ttft_p95_ms           INTEGER,
  total_avg_ms          INTEGER,
  error_count           INTEGER NOT NULL DEFAULT 0,
  context_bytes_avg     INTEGER,
  cache_rate            REAL                 -- cached_tokens / prompt_tokens
);
```

**留存策略**：llm_calls 上限 20000 行（超出删最旧，对齐 brain_ui_events 有界思路）；llm_tool_calls 随父请求级联（按 request_id 删）；llm_metrics_daily 永久，由每日聚合任务 upsert。

---

## 二、指标定义（防口径漂移，写死）

- **request_id**：进入 `call_llm` 时生成（UUID v4），本次逻辑请求的所有 attempt、所有工具调用共用；重试失败整体放弃时记录最终 attempt。
- **TTFT**：`t0` = 进入 call_llm（建连前）→ `t1` = 首个「非注释、非 keep-alive、非空」SSE data chunk 解析完成。单位 ms。
- **cached_tokens 归一化**：DeepSeek 系取 `prompt_cache_hit_tokens`，OpenAI 系取 `prompt_tokens_details.cached_tokens`，Kimi 系取响应里缓存字段；无则 0。原始 usage 一律进 `usage_raw` 兜底。
- **had_content**：与 retry.rs 完全同语义——流出过内容再失败 = 1（不可重试）。
- **error_stage 分类**：connect（建连/握手/看门狗 45s）→ headers（状态码/首包）→ stream（流中断）→ parse（SSE 解析）→ tool_loop（熔断/round 上限）。
- **stage 分类**：run_turn（主循环）/ tool_loop（工具子循环）/ wakeup（TICK/提醒唤醒）/ startup（启动自检）/ other。

---

## 三、埋点位置清单（文件 : 函数 : 埋什么）

| # | 位置 | 事件 | 埋点内容 |
|---|------|------|----------|
| 1 | `llm/caller.rs` : `call_llm` 入口 | 请求开始 | 生成 request_id；记 t0、provider、model、stage；attempt 计数 |
| 2 | `llm/caller.rs` : SSE 解析循环（plain_text_stream / 首个 data chunk 判定处） | 首 token | 记 ttft_ms（首个非注释 chunk） |
| 3 | `llm/caller.rs` : 流正常结束（end / Ok 返回） | 请求完成 | usage 四字段 + cached 归一化 + usage_raw + finish_reason + total_ms + context_bytes |
| 4 | `llm/caller.rs` : 各错误分支（建连失败/看门狗超时/流中断/解析错） | 请求失败 | error_code、error_stage、had_content、current attempt |
| 5 | `llm/retry.rs` : 每次重试决策点（should_retry / 降级链切换） | 重试/降级 | attempt+1、retry_reason、downgrade_chain 快照；429/401 不重试分支记 error_code |
| 6 | `llm/tool_loop.rs` : 循环轮次入口 + 每次工具调用前后 | 工具执行 | llm_tool_calls 一行（tool_name/args_bytes/duration/status）；熔断触发写 status=tripped；max_rounds 达限写 finish_reason=round_limit |
| 7 | `memory/injector.rs` : 编排入口与各 section 组装处 | 上下文统计 | 注入 section 数 + 总字节数 → context_bytes；各 section 命中计数（哪些常空/常满，供缓存友好裁剪） |
| 8 | `runtime.rs` : `runTurn` 入口/出口 | turn 级 | turn 总耗时、归属判定结果（created/continued）、是否 TICK 轮；写 llm_calls.stage 用 |
| 9 | `llm/providers.rs` : provider 选择处 | 路由 | （已在 #1 覆盖，此处可选冗余，用于独立核对降级链） |
| 10 | `api/routes.rs` : `deduper.claim` 命中分支 | 入站去重观测 | 内存计数器（不入库；周报口径「重复消息率」用，防误重发窗设计验证） |

**优先级**：#1–#5 是 P0-M1 必埋（一次真实调用即闭环）；#6 是 P0-M2；#7–#8 是 P0-M3；#10 可选。

---

## 四、写入通道设计（关键约束）

```
流路径（caller/tool_loop）         后台（专用线程，spawn_blocking）
┌─────────────┐   mpsc 队列   ┌───────────────────────┐
│ 内存记账  <1ms │ ──────────► │ 每 30s 或攒 100 条 flush │
│ INSERT 不进这里 │             │ INSERT OR IGNORE → SQLite │
└─────────────┘               └───────────────────────┘
```

- 流路径**绝不**同步写库（否则 TTFT 观测本身拖慢 TTFT）。
- flush 失败只 warn、丢弃本批（下批续写；request_id 幂等保证不重复）。
- 队列满（>5000 条）时丢最旧并计数，防观测自身 OOM。
- 每日聚合：00:05 由 runTurn TICK 前触发一次 upsert + 滚动清理明细。

---

## 五、关键埋点代码草图（示意，非最终实现）

### 1. caller.rs 的 TTFT 计时骨架

```rust
let t0 = Instant::now();
let request_id = Uuid::new_v4().to_string();   // 重试共享
let mut ttft: Option<u128> = None;

// SSE 解析循环内，首个内容 chunk：
if ttft.is_none() && is_content_chunk(&chunk) {
    ttft = Some(t0.elapsed().as_millis());
    metrics.record(|| Metric::Ttft(request_id, ttft));   // 只入内存队列
}
```

### 2. 后台 flush 骨架

```rust
// 专用线程：recv_timeout(30s) 攒批 → INSERT OR IGNORE
// llm_calls 唯一键 = request_id；llm_tool_calls 唯一键 = (request_id, round, tool_name)
// 失败：warn!("llm_metrics flush failed: {e}");  不重试不阻塞
```

### 3. retry.rs 的降级链快照

```rust
// 在降级链切换处：
metrics.record(|| Metric::Downgrade {
    request_id, attempt, chain: current_chain.to_string(),
});
```

---

## 六、观测闭环：从数据到动作

| 周报指标 | 阈值信号 | 动作 |
|----------|----------|------|
| cache_rate < 30% | 上下文不稳定区太多 | 查 injector 各 section 命中计数 → 把常变段移出 STABLE 核心（system_prompt.rs 已具备 relocate_sections 机制） |
| ttft_p95 > 5s | 建连/首包慢 | 查 provider 端点 + 看门狗 45s 是否常触发 |
| retries/calls > 10% | 重试语义或网络不稳 | 查 error_stage 分布；对照 is_retryable 判定是否过宽 |
| downgrades > 3% | 主 provider 不稳定 | 评估模型路由权重调整 / 换主 |
| tripped > 0 | 工具循环异常 | 查对应 tool_name 指纹 → 修工具或调熔断参数 |
| tool_calls/round 均值高 | 效率问题 | 评估 batch 化工具、减少空转轮次 |

**周报 SQL 模板**（聚合查询直接查 llm_metrics_daily + 明细表，示例）：

```sql
SELECT day, calls, retries, downgrades, tripped,
       round(cache_rate*100,1) AS cache_pct,
       ttft_p95_ms, total_avg_ms, error_count
FROM llm_metrics_daily ORDER BY day DESC LIMIT 7;
```

---

## 七、落地顺序与验收

| 里程碑 | 内容 | 验收标准 |
|--------|------|----------|
| M1 | 建 3 表 + #1–#5 埋点 | 一次真实调用后：llm_calls 恰 1 行、字段全非空（或显式 NULL 合理）、重试场景 attempt>1 且仍 1 行 |
| M2 | #6 工具台账 | 一次含工具调用 turn：llm_tool_calls 行数 = 实际调用数，UNIQUE 无冲突 |
| M3 | #7–#8 上下文/turn 统计 | run_turn 与 tool_loop 的 context_bytes 有值，turn 级记录齐全 |
| M4 | 跑满一周 → 首份周报 | 六项指标全有数；基于数据产出 ≥3 条调校项（可执行、可验证） |

**贯穿验收**：埋点自身开销 单请求内存侧 <1ms；落库失败零影响原流程；重启后 request_id 仍唯一（幂等复查）；明细表滚动淘汰生效。
