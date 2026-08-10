# Rust 延伸恢复与 M1/M1.5/M2/M3/M4/P1-1/P1-2 落地 — 成果汇总

## 一、恢复与研判（只分析、未改码）

- **工程恢复**：bailongma-rust 沙箱副本（cargo workspace：crates/core、app、sandbox；依赖 tokio/axum/reqwest(rustls)/rusqlite；rust-version 1.94；非 git 仓库，当日 5 个 rescued 备份）已恢复可编译状态。
- **六线研判报告**：`RESEARCH_RELIABILITY_LLM_SAFETY.md` — 全库实证 LLM 调用可靠性，锁定关键路径（错误覆盖/成功、attempt 取数、重试翻盘、工具台账去重、flusher 落库、周报聚合）。
- **整合稿**：`REVIEW_DRAFT_INTEGRATED.md` — 六线研判 + 埋点设计 + M1 草案 + 与 BaiLongma 设计初衷融合。
- **专家评审落地稿**：`DELIBERATION_FINAL_PLAN.md` — 评审纪要 + 分阶段实施计划（M1→M1.5→M2→M3→M4 + 语料 label 列）。
- **定位结论**：Rust 延伸不是加外挂，而是补上初衷里"承诺过但未兑现"的部分。从代码读出四个核心词：**意识循环**（runtime 归属→注入→渲染）、**长期陪伴**（memory/threads/跨日追踪）、**自我进化**（self_evolution）、**多智能体协作**。

## 二、M1 落地（LLM 指标观测层，P0）

- 新增 `llm_metrics` 仓库 + `metrics` 模块；schema 建表；caller、retry、tool_loop、llm/mod.rs 五处接入指标记录。
- **验证结果**：编译全部 crate 通过；llm_metrics 定向测试 **17 条全过**（metrics::tests 12 + llm_metrics 过滤 5，0 失败）。
- 评审锁关键路径全覆盖：成功覆盖错误 / 错误不覆盖成功 / attempt 取 max、重试失败→成功翻盘、工具台账 attempt 维度去重、flusher 每请求一行去重、周报聚合 + 空窗告警。
- 唯一警告：`FlusherHandle.join` 字段未读（无害，回头顺手消）。
- ✅ **全量回归已补跑**：351 通过、0 失败、1 忽略（core 342 + app 8 + db_compat 1；46s）。
- ✅ **db_compat 基线修正**：老库 29 张表 → Rust 版打开后 32 张，正是 M1 新增的 3 张 LLM 指标表（llm_calls / llm_tool_calls / llm_metrics_daily）；conversations/memories 数据零改动。测试断言已更新为"恰好新增 3 张 + 新表必须存在"，重跑通过。

## 三、M1.5 落地（delegate 注入面加固，P0）

- **注入面定位**：`crates/core/src/agents/delegate.rs` CLI 分支把 `invoke_cmd + invoke_args` 拼成单字符串交给 `cmd.exe /C`（或 `sh -c`）执行；prompt 仅做 `"` 与换行替换，`& | < > % ^` 等 cmd 元字符在引号内照样被解释为命令分隔符/展开——全库唯一肉眼可见注入面（评审 E3 提升至 P0）。
- **改造**：
  - 提取公共执行核心 `spawn_and_wait(program, args, timeout)`（读线程排空 + 50ms 轮询 + 超时强杀）；
  - 新增 `run_command_with_args`：**参数数组直启，不经 shell**；`invoke_cmd` 拆为「程序 + 前缀参数」，`{prompt}` 原样（保留换行）作为独立 argv 元素传递；
  - 保留 `run_command_with_timeout`（shell 兼容路径）与 `kill_process_tree`（Windows `taskkill /T /F` 进程树，直启后子进程树同样强杀）。
- **验收回归 3 条全过**（+ 原有 11 条 delegate 测试 = 定向 14 条全绿）：
  1. 正常调用：node 替身直启回显 prompt+context；
  2. **含元字符参数**：载荷 `hi & echo PWNED_7f3a | more "quoted" %PATH%` 原样回显、exit 0，无注入执行（新增）；
  3. 超时强杀：直启路径 1s 超时 taskkill /T /F 进程树（新增，日志可见 SUCCESS terminated）。
- ✅ **全量回归**：353 通过、0 失败、1 忽略（core 344 + app 8 + db_compat 1；46s；比 M1 多 2 条正是 M1.5 新增回归）。

## 四、M2 落地（工具台账 + 熔断/round_limit 事件，P0）

- **M1 已铺地基**：`llm_tool_calls` 表在 M1 建表时即含 `attempt` 列 + `delegated_from` 列（NOT NULL DEFAULT ''，空串 = 主 agent 直调合法）+ `UNIQUE(request_id, round, attempt, tool_name)` 唯一键；`upsert_tool_calls_batch` 为 INSERT OR IGNORE 防重放，重试路径不误伤。M2 补齐两块增量：
- **round_limit 事件**（设计 #6）：
  - `MetricEvent::RoundLimit`：工具循环走满 `max_rounds` 上限退出时，由 `call_llm` 以最后一轮 request_id 发出；
  - 终态语义：`llm_calls.finish_reason = "round_limit"`（异常终止；真实消耗字段保留），日聚合计入 error 口径（Done 纠偏只补 error、不动 total）；
  - UPSERT 终态守卫扩展：`round_limit` 允许覆盖已落库的 `done`（跨 flush 场景），`CallFinished` 加 round_limit 防覆盖守卫（时序防御）；
  - `for` 循环改 `while`，末轮 ctx 留存以携带 request_id。
- **delegated_from 可注入**（评审修订 #9 / Q9）：`CallLlmArgs` 新增 `delegated_from` 字段，`record_tool_call` 原样透传到台账——协作一接线即有信任账本，主 agent 直调为空串（合法）。
- **新增回归 4 条全过**：
  1. `round_limit_marks_terminal_finish_reason`（flusher 层：done→round_limit 终态 + 日聚合纠偏）；
  2. `round_limit_overwrites_done_across_flush`（跨 flush：UPSERT 必须把已落库的 done 改为 round_limit）；
  3. `loop_exhausts_max_rounds_emits_round_limit`（集成：max_rounds=2 走满 → 最后一轮 finish_reason=round_limit，台账两轮齐）；
  4. `delegated_from_propagates_to_tool_ledger`（集成：协作委托来源原样落库）。
- ✅ **全量回归**：357 通过、0 失败、1 忽略（core 348 + app 8 + db_compat 1；57s；比 M1.5 多 4 条正是 M2 新增回归）。

## 五、M3 落地（injector 上下文统计 + turn 级记录，P0）

- **#7 injector 上下文统计**（设计 #7）：`compute_context_stats(&InjectorOutput)` 从「只出聚合数」升级为 **20 个候选 section 逐一报命中（section 名 → 字节数）**；新表 `llm_context_sections`（request_id, section, bytes；UNIQUE 防重放）落 section 明细，`llm_calls.context_bytes` 落总字节——**与 M1 表按 request_id JOIN 可用**，周报"哪些 section 常空/常满 → 缓存友好裁剪"的数据基础。
- **#8 turn 级统计**：新表 `llm_turns`（turn_id 主键：started_at、duration_ms、attribution（created/continued/resumed/noop）、is_tick、sections_hit、context_bytes、calls）；`llm_calls` 补 `stage` 列（run_turn/tool_loop/wakeup/startup，老库幂等 ALTER 补列）；`StreamContext.stage` 穿透到 CallStarted。
- **TurnSession 接线器**：意识循环接线后 `begin → record_context_stats(&injection) → call_llm(request_id 共享) → finish(attribution, is_tick, calls)` 一次闭环，事件经 flusher 落三张表。
- **新增回归 3 条全过**：
  1. `m3_context_and_turn_pipeline_joins_with_calls`（集成：stage=run_turn + 3 个 section 明细 JOIN llm_calls 成功 + section 字节和 = context_bytes + turn 记录齐全——M3 验收核心）；
  2. `turn_session_records_context_and_finish`（TurnSession 闭环：TICK 轮 is_tick=1、空注入零 section、turn 恰 1 行防重放）；
  3. `context_stats_counts_nonempty_sections`（纯函数：20 个 section 的命中与字节统计，含明细断言）。
- ✅ **全量回归**：359 通过、0 失败、1 忽略（core 350 + app 8 + db_compat 1；60s；比 M2 多 2 条正是 M3 新增回归）。
- ✅ **db_compat 基线更新**：34 张表（M1 3 张 + M3 2 张）；老库打开幂等补 `llm_calls.stage`，历史数据零改动。

## 六、M4 落地（周报六指标 + 阈值信号 + 唤醒成本观测，P0）

- **周报六指标 + 阈值信号**（设计 §六 动作映射落地，含于 M3 期一并完成）：
  - `WeeklyReport`（llm_metrics_daily 7 天窗口聚合）：总调用 / 错误 / 重试 / fallback / aborted / 总 tokens / cached tokens / ttft / duration 九项原始 + 五个派生率（error_rate、cache_rate、retry_rate、aborted_rate、avg_ttft、avg_duration）；
  - 阈值信号（空 = 一切正常）：cache_rate < 30% → 查 injector 命中计数（M3 context_bytes 数据基础）、error_rate > 10%、retry_rate > 20%、aborted_rate > 5%、avg_ttft > 3000ms；
  - 空窗口专有信号「观测窗口内无 LLM 调用」。
- **唤醒成本观测**（R7 缓解第一步——先有账再设闸，P1-1 成本闸门的前置）：
  - `WakeupCost`：stage='wakeup' 的 llm_calls 周窗口聚合（次数 / tokens / cached / ttft / duration），派生 cache_rate、avg_ttft、avg_duration、**占总 tokens 份额 share_of_total**；
  - 信号：唤醒 0 观测（TICK 未接线或 stage 未标）→ 账本为空、闸门暂不可用；**唤醒成本占比 > 25%** → 建议 P1-1 合并器 + 周窗口预算闸门；
  - `WeeklyReport.render()` 渲染可读周报文本（api/日志/TUI 直接展示）。
- **新增回归 2 条全过**：
  1. `weekly_report_includes_wakeup_cost`（日聚合 100 调用/200k tokens + 10 次 wakeup/80k tokens → 份额 40% 触发信号，render 文本含「唤醒成本：10 次」）；
  2. `weekly_report_wakeup_no_data_still_renders`（空窗口渲染不 panic）。
- ✅ **全量回归**：361 通过、0 失败、1 忽略（core 352 + app 8 + db_compat 1；48s；比 M3 多 2 条正是 M4 新增回归）。
- **待接线日回访**（M4 验收门槛）：真实调用跑满一周后 `SELECT` llm_metrics_daily + llm_calls(stage='wakeup') 复核六指标全有数，产出 ≥3 条可执行调校项。

## 七、P1-1 落地（唤醒闭环：reminders 访问器 + 合并器 + 周窗口预算闸门）

- **reminders 访问器**（新仓库 `db/repositories/reminders.rs`）：
  - `due_reminders(db, now)`：查 `status='pending' AND due_at <= now`，按 due_at 升序——只暴露到期未触发的提醒；
  - `mark_fired(db, ids, now)`：消费后标记 `status='fired'` + 写 fired_at，防止重复唤醒（幂等、空列表 noop）。
- **合并器**（新顶层模块 `wakeup.rs`）：
  - `coalesce(&[ReminderRow])` 纯函数：N 个触发器 → **1 条合并唤醒消息**（标题「有 N 条到期提醒待处理」+ 逐行 `- [due_at] task`，task 为空回退 system_message），喂给一次 LLM 调用——唤醒次数从 N 降到 1，直接削掉 R7 唤醒成本里「重复唤醒」那一块；
  - `coalesced_wakeup(db, now, days, budget)` 闭环入口：查到期 → 合并 → 闸门 → 标记 fired；闸门拦截时不唤醒、**不消费提醒**（保持 pending 等待预算恢复/人工放行），无到期返回 None。
- **周窗口预算闸门**（消费 M4 唤醒成本账本，先有账再设闸）：
  - `wakeup_budget_gate(db, days, budget_tokens)`：`wakeup_cost_weekly` 本周已耗 wakeup tokens ≥ 预算 → `Blocked{used,budget}`；否则 `Allow{used, remaining}`；
  - `budget_tokens <= 0` = 闸门关闭（纯观测不拦截，默认形态安全）。
- **验收门槛达成——合并器风暴测试**：`coalesce_storm_n_triggers_one_wakeup` 插入 **8 个同时到期的触发器 → 恰好 1 次合并唤醒**（trigger_count=8、消息 1 行标题 + 8 行明细），消费后 8 条全部 fired 不会重复唤醒。
- **新增回归 10 条全过**（reminders 3 + wakeup 7）：
  1. `due_reminders_only_pending_and_due`（pending+到期过滤：future/fired/cancelled 均排除）；
  2. `mark_fired_updates_status_and_fired_at`（fired_at 落 now，标记后访问器不再返回）；
  3. `mark_fired_empty_noop`；
  4. `coalesce_storm_n_triggers_one_wakeup`（验收核心：N→1）；
  5. `coalesce_pure_function_groups_all_rows`（纯函数，空 task 回退 system_message）；
  6. `budget_gate_allows_within_budget`（30k/100k → Allow 剩 70k）；
  7. `budget_gate_blocks_when_over_budget`（120k/100k → Blocked）；
  8. `budget_gate_zero_budget_disabled`（0 = 闸门关闭）；
  9. `coalesced_wakeup_blocked_keeps_reminders_pending`（拦截不消费）；
  10. `coalesced_wakeup_none_when_no_due`（未到期不唤醒）。
- ✅ **全量回归**：371 通过、0 失败、1 忽略（core 362 + app 8 + db_compat 1；46s；比 M4 多 10 条正是 P1-1 新增回归）。
- **接线待办**：合并器出 1 次唤醒后，需在 runtime TICK 轮把 `coalesced_wakeup` 的输出喂给一次 `run_user_turn`（stage='wakeup'），唤醒成本账本即有真实数据（M4 周报"唤醒 0 观测"信号随之消除）。

## 八、P1-2 落地（幂等修复：LLM 重试幂等键 + 工具执行防重放）

- **验收门槛达成——故障注入测试**：`tool_replay_guard_prevents_double_execution_on_lost_response` 模拟「provider 完成但响应丢失」→ 以同一逻辑请求（round request_id 由 `round_request_id_seed` 固定为 `fault_inject_turn#0`）重放整轮 → **副作用工具只执行 1 次**（第 2 次调用命中台账复用记录结果，执行计数不再增长），复用结果与首次逐字一致。
- **#1 LLM 重试幂等键（provider 侧去重，M1 已打底）**：
  - `caller::idempotency_key(request_id)`：由逻辑请求 ID 派生 `blm-{request_id}`；`stream_once` 在 `ctx.request_id` 存在时给 POST 附加 `Idempotency-Key` 头；
  - 重试共享：`stream_once_with_retry` 内所有 attempt 复用同一 `ctx.request_id`（M1 语义），故同一逻辑请求的重试携带同一幂等键——provider 完成但响应丢失时，重试不会在 provider 侧产生第二次执行/计费；
  - 匿名调用（无 request_id）不承诺幂等，不附加头。
- **#2 工具执行防重放（本地台账，M1/M2 打底 + 本件补齐读侧）**：
  - 新模块 `llm/replay.rs`：`ToolReplayGuard` trait（可插拔）+ `DbToolReplayGuard`（基于 llm_tool_calls 台账）；
  - 执行前查：同逻辑请求（request_id + round + tool_name）已有 `status='ok'` 记录 → 复用记录结果，**不重复执行**（error/tripped 不视为已成功，重试照常重新执行）；
  - 执行后同步落账（不等 flusher）：`guard.record` 与 `record_tool_call` 同键（attempt=1），INSERT OR IGNORE 天然去重——**执行了就有账**，响应丢失后重试可复用；
  - 仓库新增读侧 `find_tool_call_result`：只取 `status='ok'` 且 `ORDER BY attempt DESC, rowid DESC LIMIT 1`（多 attempt 取最新成功）；
  - `call_llm` 新增 `replay_guard: Option<&dyn ToolReplayGuard>`（默认 None = 零侵入，既有行为不变）；`CallLlmArgs` 新增测试接缝 `round_request_id_seed`（None = 生产行为）。
- **真实接线**：`crates/app/src/bin/chat.rs`（M2 验证 CLI）在 `call_llm` 调用处接上 `DbToolReplayGuard::new((*db).clone())`——P1-2 防重放在真实二进制里生效，台账与 serve 入口共用 `data/jarvis.db`。
- **新增回归 3 条全过**：
  1. `tool_replay_guard_prevents_double_execution_on_lost_response`（验收核心，见上）；
  2. `find_tool_call_result_returns_only_ok_replay`（只复用 ok 且取最新 attempt；未执行过/只有 error/不同 round 均 None）；
  3. `idempotency_key_is_stable_across_retries`（同请求键稳定、异请求键不同、可作 HTTP header 值）。
- ✅ **全量回归**：374 通过、0 失败、1 忽略（core 365 + app 8 + db_compat 1；46s；比 P1-1 多 3 条正是 P1-2 新增回归；schema 未动，db_compat 基线仍 34 张表）。

## 九、UI 增强落地（Agent 场景面板）

- **前端 `resources/index.html`（37.9KB）**：新增 Agent 场景面板，实时消费 WS `/scene`——全量快照 + 增量 patch 渲染；卡片渲染器覆盖 text/metric/weather/choice/image/media，未知 kind 降级 JSON 向前兼容；intent 三级视觉（ambient 淡化 / inform 常规 / confront 高亮描边）+ focus 描边 + order 排序；choice 点击选项回传 select intent，交互闭环打通。
- **协议层 `crates/core/src/scene/store.rs`（19.9KB）**：新增 `SceneStore::set_many` 原子批量——一次 rev+1、单条 patch 带多 ops，避免多卡片依次更新中间态闪烁；任一输入非法整体拒绝，不半应用。
- ✅ **全量回归**：368 通过（core 定向，含新增 3 条 set_many 测试）；日志 `_cargo_test_ui_0810.log`。

## 十、R1 落地（sandbox 真实化：M5 占位 → JSON-RPC 执行器）

- **定位**：`crates/sandbox/src/main.rs` 原为 M5 占位——只打印一行 JSON 无真实执行能力。
- **改造**：JSON-RPC 协议（id/method/params）+ 能力令牌校验 + `exec_command`（超时/输出截断/参数脱敏）+ `read_file` / `write_file` / `list_dir`；spawn 错误带 cwd 调试信息；命令直启不经 shell 解释。
- **修复两个测试基建坑**：`normalize_absolute` 的 Windows prefix 截断（`s[..1]` 只取盘符丢冒号，产出 `C\...` 非法路径）→ 修复为 `s[..2]`；test 函数返回 `PathBuf` 导致 `TempDir` 提前 drop（目录被删，write_file 靠 create_dir_all 侥幸通过、exec 的 current_dir 直接 267）→ 返回 `TempDir` 持有生命周期。
- ✅ **验证**：8 条测试全过 + 二进制自测 ALL PASS（日志 `_r1_sandbox_test6.log`）。

## 十一、R2 落地（工具能力层：9 个真实工具）

- **新增 `crates/core/src/tools/`**（778 行）：
  - `get_timestamp`（unix/iso/日期/时间/周几 5 格式）、`read_file`、`write_file`（父目录自动创建）、`list_dir`、`exec_command`（**默认委托 sandbox JSON-RPC 子进程**，未找到 sandbox 时直接执行兜底）、`search_memory`（memories 模糊查询）、`send_message`（conversations 落库 + message_out 广播）、`collect_agents`、`remind`（reminders 表 + 到期触发）；
  - `NativeToolExecutor`：`with_db` / `with_send_message` / `with_sandbox` 链式装配；`all_tool_schemas()` 把 9 个工具的 schema 注册进 LLM 工具循环。
- **修复 1 个测试断言 bug**：unix 时间戳返回整数，测试却用 `as_str()` 断言（实现正确、断言错误），顺带清掉 2 个 unused import。
- ✅ **验证**：core 380 通过、0 失败；sandbox 8 通过（日志 `_r2_test2.log`）。

## 十二、R3 落地（意识闭环：占位 conversation_id=0 → 真实链路）

- **定位**：`crates/app/src/api_host.rs` 的 inbound 闭包原只打日志返回占位 `conversation_id=0`——消息进来不会真正被处理成回复。
- **改造**：入站 → `conversations::insert` 落库拿**真实 conversation_id** → `tokio::spawn` 异步 `run_conscious_turn`（LLM 激活检查（未激活降级回复不空转）→ `run_user_turn` 归属/注入/渲染 → `NativeToolExecutor` 9 工具装配 → `call_llm` 流式工具循环 → 回复落库 + `message_out` 事件广播）。
- **修复 4 处编译错误**：sandbox 可选路径、`LlmMessage`→`ChatMessage` 转换、stream `Arc` 解引用、`std::sync::Mutex` guard 跨 await 不 Send → 换 `tokio::sync::Mutex`（`state.lock().await`）。
- ✅ **全量回归**：397 通过、0 失败（core 380 + app 1 + sandbox 8 + db_compat 8；日志 `_cargo_test_r3_0810.log`）。

## 十三、后续推进

- 已批准：事项 1、2、3、4、6；事项 5（人工介入硬通道）待定。
- 路线：M1→M1.5→M2→M3→M4 ✅（P0 全部落地）→ P1-1 唤醒闭环 ✅ → **P1-2 幂等修复 ✅** → **R1 sandbox 真实化 ✅ → R2 工具能力层 ✅ → R3 意识闭环 ✅** → R4 文档补齐/封装打包/GitHub 推送 → P2 沙箱 trust 分层 / 参数 schema 校验 → P3 基于周报的缓存友好化 / 模型路由 / token 预算 → P4 自动迭代 + 语料蒸馏。
- 接线日回访项（M4 前置验收）：真实调用 SQL 检查。
