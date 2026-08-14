# 代码审计报告 · PHASE 2（基线 68ddac3）

- **基线**：`68ddac3`（2026-08-14 00:30:48 +0800，master）
- **范围**：全库逐文件（`crates/core` + `crates/app` + `crates/sandbox`，约 44 源文件 / 约 2.5 万行）
- **侧重**：安全 + 正确性兼顾（用户确认）
- **方法**：只读审计（不编译、不改码）；5 路并行代理 + 亲读抽查复核
- **证据分级**：🟢 本轮亲读验证 / 🟡 引用代理审计（线号以代理报告为准，建议修复前复核）
- **对比基线**：`AUDIT_PHASE1_F305AEB.md`（f305aeb，2026-08-12）

---

## 0. 基线变更概览（f305aeb → 68ddac3）

- 18 个新提交，44 文件变更，+4118/−1296：wave3–wave6 新增模块（工具熔断/防重放台账、LLM 指标聚合、唤醒可靠性 watchdog、沙箱/审批/委托等）。
- 安全面正面变化：工具分发新增 `guard_approval`/`guard_delegation` 派发级检查、熔断台账、防重放、SSE/权限中间件。

---

## 1. 上一轮遗留复核表（AUDIT_PHASE1_F305AEB 结论 → 本轮状态）

| # | 遗留项 | Phase1 状态 | 本轮状态 | 证据 |
|---|--------|------------|---------|------|
| 1 | 工具执行未走统一分发（B：delete_file 无 ApprovalGate） | 部分落地 | ✅ 已修复 | `tools/mod.rs` 统一 `execute` 分发前 `guard_approval`，delete_file 挂门；测试 `tools/mod.rs:970-1016` | 🟢 |
| 2 | delegate_to_agent 未注册（C） | 未注册 | ✅ 已修复 | `tools/mod.rs` 注册表含 delegate_to_agent / grant_agent_delegation，分发前 `guard_delegation` | 🟢 |
| 3 | 工具无 CallerTrust（D） | 未落地 | ✅ 已修复 | `with_caller_trust`（`tools/mod.rs:121`）+ `CallerTrust::System` 测试（:1017） | 🟢 |
| 4 | 工具 trace 覆盖不足（E） | 部分 | ✅ 已修复 | M2 台账 `record_tool_call` 全覆盖（执行/熔断/重放行），键含 attempt 防重试误伤 | 🟢 |

> ⚠️ 但见 **#1（HIGH-S1）**：分发层挂门与审批装配是两回事——生产装配路径**没有注入 ApprovalGate**，挂门形同虚设。

---

## 2. 问题清单

### 2.1 安全面（亲读：api/security.rs、api/server.rs、policy/mod.rs、capability/mod.rs、approval.rs、tools/mod.rs、api/routes.rs）

| # | 级别 | 位置 | 问题 | 证据 | 建议 |
|---|------|------|------|------|------|
| S1 | **HIGH** | `crates/app/src/service.rs:182-205` + `tools/mod.rs`（guard_approval） | **生产装配未注入 ApprovalGate**：`build_executor()` 只链 `with_db/with_send_message/with_sandbox`，无 `with_approval`。`approval=None` → `guard_approval` 直接放行 → **exec_command / delete_file / delegate_to_agent 在生产中全部免审批执行**。Phase1 的 B/D 修复停留在代码层+测试层，生产零生效。`approval::init_global` 在 `server.rs` 已初始化全局门，但无人接线。 | 🟢 grep 全库：`with_approval` 仅出现在测试（tools/mod.rs:970/998/1016）与定义处；service.rs:198-204 无调用 | 在 `build_executor()` 增加 `.with_approval(crate::approval::global())`（API 路径 `POST /approval` 已提交到同一全局门，接线即闭环）。属**安全策略变更，需人工批准**（A 级） |
| S2 | MEDIUM | `api/routes.rs:313` + `approval.rs:269-275` | `global()` 未初始化时以临时目录兜底：生产中若 `init_global` 遗漏则审批挂到临时目录门，提交与执行可能分裂到两个实例（S1 修好后此风险同步存在）。 | 🟢 | `global()` 兜底改 panic 或返回 Result，强制显式初始化 |
| S3 | MEDIUM | `approval.rs:130-134` | approval id `ap_{now_ms}_{seq}` 可猜测：攻击者可预生成并回滚/放行指定请求。 | 🟡 | id 改随机（uuid / 8 字节随机） |
| S4 | MEDIUM | `approval.rs:170-177` | 审批回调在 `on_request` 锁内执行：回调慢 → 阻塞同门其他审批请求。 | 🟡 | 锁外回调或异步派发 |
| S5 | INFO | `api/routes.rs` get_trace / /events/history | 回环（127.0.0.1）免 token 可读 trace 与事件历史（设计如此：本机假设可信）。注意 trace 含工具调用详情（args_json），若本机被多用户共用需收敛。 | 🟢 | 维持现状，文档注明假设边界 |
| S6 | INFO | `server.rs` guard_request / OPTIONS | token 常量时间比较、no-store 头、OPTIONS 豁免、静态资源 fallback 均在中间件层，覆盖所有路由。回环 /message 豁免 token 校验（4e8ee18 新行为）有速率限制兜底。 | 🟢 | 维持 |

### 2.2 LLM 层（代理审计 + 亲读抽查 tool_loop.rs:520-609）

| # | 级别 | 位置 | 问题 | 证据 | 建议 |
|---|------|------|------|------|------|
| L1 | **HIGH** | `tool_loop.rs:562,584-587` + `replay.rs:52` + `llm_metrics.rs:244` | **防重放键不含参数指纹**：键 = `(request_id, round, tool_name)`。同一轮 LLM 两次调用同名工具（如两次 `read_file` 不同路径、两次 `send_message`）第二次**命中缓存复用第一次的结果**——既吞真实调用，又是正确性缺陷（重放面反而因参数未进键而错误）。`record` 已存 `normalized`，但 `find_result` 未过滤。 | 🟢 | 键增加参数指纹（hash of normalized）；语义上区分「同参幂等复用」与「不同参必须执行」 |
| L2 | MEDIUM | `tool_loop.rs:550-596,553` | 熔断分支 `state.consecutive_failures = 0` 但不调 `record_outcome`：三连败后熔断，熔断本身又把计数清零 → 下轮可立即再试，**熔断实际失效**（对齐 Node 的行为但语义矛盾）。 | 🟢 | 熔断不计 outcome，但不应清零连续失败；或引入独立冷却期 |
| L3 | MEDIUM | `metrics.rs:457` | 聚合器 `MatchMetricEvent::CallFailed` 分支 `DailyState::Done => unreachable!(...)`：若入口守卫（status=="done" 提前 return）不覆盖全部到达路径（重试链/并发事件乱序），**panic 杀死 flusher 后台任务**，指标静默停更。 | 🟢 | 改 `recover!` / 忽略并告警；上游 `metrics.rs` 事件处理统一收口加 try/catch |
| L4 | MEDIUM | `metrics.rs:618-647` | flusher 失败路径直接清空 pending：落库失败即丢账（LLM 成本账本丢失 → 预算闸门失守）。 | 🟡 | 失败保留 pending 或落盘补偿队列 |
| L5 | MEDIUM | `metrics.rs:534-539` | `pending_ctx` 无界增长：高频短请求下内存泄漏。 | 🟡 | 上限 + LRU / 超时清理 |
| L6 | MEDIUM | `llm_metrics.rs:162-176` | UPSERT 终态守卫不一致：`round_limit` 路径可把非终态降级为 done；`last_error` 无条件覆盖已有错误链。 | 🟡 | 终态转换表 + 只追加错误链 |

### 2.3 DB 层（代理审计 + 亲读 reminders.rs / models.rs / schema.rs 抽查）

| # | 级别 | 位置 | 问题 | 证据 | 建议 |
|---|------|------|------|------|------|
| D1 | **HIGH** | `db/repositories/reminders.rs:28` + `db/models.rs:36-38` | `due_at <= ?1` **字符串字典序比较**，格式约定未统一：`now_iso()` 产出 UTC `Z` 后缀（`%.3fZ`），而现有插入/测试全是 `+08:00`。一旦混存，到期判定与排序错乱（同一时刻 `Z` 与 `+08:00` 字典序永远不一致）。 | 🟢 `now_iso()` 注释明示 Z；reminders.rs 头注释「同格式可直接比较」的前提未在写入侧强制 | 全链路统一存储格式（建议一律 UTC `Z`），或在 SQL 用 `strftime` 解析比较 |
| D2 | MEDIUM | `db/schema.rs:247,557,560` | `CREATE UNIQUE INDEX IF NOT EXISTS`：老库若已存在重复数据，建索引直接失败 → **启动中断**。 | 🟡 | 先查重清理/去重迁移，或降级为普通索引+应用层去重 |
| D3 | MEDIUM | `db/schema.rs:757` | 每次启动无条件全量重建 FTS 虚拟表：大库启动慢（分钟级）且磨损磁盘。 | 🟡 | 版本标记 + 增量重建 |
| D4 | MEDIUM | `db/repositories/memories.rs:32-63` | check-then-act 未包事务：并发写入可产生重复/悬空记忆。 | 🟡 | `BEGIN IMMEDIATE` 事务包裹 |
| D5 | INFO | `schema.rs` 全表 | 索引覆盖较全（idx_reminders_due_at 等），未发现缺主键/外键约束。 | 🟡 | 维持 |

### 2.4 Memory 层（代理审计 + 亲读 messages.rs:240-309 抽查）

| # | 级别 | 位置 | 问题 | 证据 | 建议 |
|---|------|------|------|------|------|
| M1 | **HIGH** | `messages.rs:253-257` + `threads.rs:322-327` + `retrieval.rs:40-44` | **TICK 前缀拼接无分隔转义**：固定前缀「[heartbeat tick - no new user message]…」与 `{input}`/检索块直接拼接。记忆/检索文本若含近似指令文本，可干扰模型把**内容当系统前缀**（提示注入放大面；TICK 轮无用户输入，但检索内容可成为伪造载体）。 | 🟢（前缀拼接亲读确认；注入链 🟡） | 前缀后加显式分隔标记（如 `=== TICK CONTEXT START ===`）+ 输入转义/裁剪 |
| M2 | **HIGH** | `injector_format.rs:135-145,600-634,658-716,768-773` | SectionTag 覆盖缺口：`<thread>`、`<threads-background>`、`<task>`、`<active-policies>`、`<task-knowledge>`、`<self-evolution>` 等节未做打标/转义即裸拼接进 system 上下文 → 记忆内容可冒充指令区块。 | 🟡 | 统一 SectionTag 封装（与已覆盖节同机制） |
| M3 | MEDIUM | `messages.rs:584-589` | TICK 轮把整个历史 context 升格为 System role：普通 assistant 内容混入系统层，提升注入影响力。 | 🟡 | TICK 轮历史保持角色不变，仅增量拼接 |
| M4 | MEDIUM | `threads.rs:135` | 信封剥离正则缺 `+`：与 `retrieval.rs:54` 不一致 → 部分信封残留进上下文。 | 🟡 | 统一正则 |
| M5 | MEDIUM | `threads.rs:504-527` | commitments 无界增长：长会话内存膨胀。 | 🟡 | 上限 + 压缩/落库 |

### 2.5 App 层（代理审计 + 亲读 service.rs:180-229 抽查）

| # | 级别 | 位置 | 问题 | 证据 | 建议 |
|---|------|------|------|------|------|
| A1 | **HIGH** | `watchdog.rs:152-156` | **supervisor 假死分支挂死**：worker 卡在**同步阻塞**（std Mutex 锁 DB / 同步 SQLite 长查询）时，`worker_handle.abort()` 无法抢占同步段，随后 `worker_handle.await` **永久挂起** → supervisor 连同监控一起死亡，假死自愈失效。 | 🟢（亲读；测试只覆盖 await 点假死，未覆盖同步段卡死） | abort 前 `await tokio::time::timeout(短)` 降级：放弃该 worker、记录 stuck、直接重启新循环（旧任务泄漏但保活） |
| A2 | **HIGH** | `service.rs:507-672` + `wakeup.rs:102` | **提醒 consume-before-deliver 无回滚**：先 `mark_fired` 后广播 LLM 唤醒；广播/LLM 失败（panic/网络）时提醒永久丢失（status='fired' 不可恢复）。 | 🟡 | 广播前不标记；或 mark_fired 前置条件 + 失败补偿（延迟重投/转 fired 后备注 error） |
| A3 | MEDIUM | `service.rs:209-219` | 消息轮裸 `tokio::spawn` 无 panic 守护：单轮 panic 静默丢失用户请求且无探活。 | 🟢 | 复用 LoopSupervisor 或包 `catch_unwind`/tracing::error |
| A4 | MEDIUM | `service.rs:210-211` + `turn_state.rs:46-60` | 消息级幂等键**只写不查**：idempotency_key 落库但从未校验 → 重复消息可能重复执行（重复发消息/重复扣费）。 | 🟡 | 入口校验幂等键，命中直接返回已处理结果 |
| A5 | INFO | `service.rs:180-205` | 交互轮/唤醒轮共用 build_executor（好实践），send_message 落库+事件广播。 | 🟢 | 维持 |

### 2.6 Sandbox 层（代理审计 + 亲读 main.rs:300-343 抽查）

| # | 级别 | 位置 | 问题 | 证据 | 建议 |
|---|------|------|------|------|------|
| B1 | **HIGH** | `main.rs:314-343,275-279` | **junction 逃逸**：`resolve_in_root` 的双保险 canonicalize 仅在**路径已存在**时执行（`if let Ok(canon)`）。新建文件路径（父目录含 junction 指向 root 外）→ canonicalize Err → 跳过二次校验 → **写入落在 root 外**。 | 🟢（亲读：331 行 `if let Ok` 分支） | 改 canonicalize **父目录**（父目录必存在）再拼接文件名；存在即拒 |
| B2 | **HIGH** | `main.rs:197-198,179-189,152-169` | 子进程继承 stdout/stderr 句柄 + join 无限等待：孙进程不退出 → **join 永久挂死**；Unix 超时只杀父进程，句柄链不断。 | 🟡 | 管道改为各自捕获并 `wait` 带超时；超时 kill 进程组 |
| B3 | MEDIUM | `main.rs:517` | JSON-RPC `read_line` 无长度上限：超长行拖垮沙箱（内存/CPU DoS）。 | 🟡 | `take(上限)` |
| B4 | MEDIUM | `main.rs:331` | TOCTOU：词法校验→canonicalize→真实 IO 之间路径可变（符号链接换绑）。 | 🟢 | canonicalize 后**用规范化路径执行 IO**，避免二次解析 |
| B5 | LOW | `main.rs:414-442` | exec_command 元字符黑名单缺 `^ () !`：Windows cmd 转义/子命令面可被绕过黑名单。 | 🟡 | 补齐黑名单或改白名单 |
| B6 | INFO | `main.rs` 整体 | 沙箱根隔离 + 词法+链接双校验设计合理，模块化清晰。 | 🟢 | 维持 |

### 2.7 运行时面（wakeup / approval / evolution / matter / scene/store，代理审计）

| # | 级别 | 位置 | 问题 | 证据 | 建议 |
|---|------|------|------|------|------|
| R1 | MEDIUM | `wakeup.rs:97-104` | gate-check 与 `mark_fired` **非原子**：并发唤醒轮可双双通过闸门；`fired_at` 标记先于 LLM 调用（与 A2 同源）。 | 🟡 | 单连接内串行 + 事务化消费 |
| R2 | MEDIUM | `evolution/mod.rs:127` | `?` 提前返回跳过清理/补偿路径。 | 🟡 | guard 化 + 失败路径补记 |
| R3 | MEDIUM | `matter.rs:344-392` | completed 状态先持久化后做 additivity 校验：校验失败留下已完成的错误账。 | 🟡 | 校验通过再持久化，或带事务回滚 |
| R4 | MEDIUM | `scene/store.rs:268-304` | rev 在消息构建时读取、`base = rev()-1`：并发下乐观锁失效（丢更新/脏覆盖）。 | 🟡 | 写入时重新读取 rev 并 CAS |
| R5 | INFO | `intervention.rs` | Q6 人工介入暂停语义（派发级检查点）亲读确认有效。 | 🟢 | 维持 |

---

## 3. 新增发现分级

### A 级（需人工决策 / 属安全策略变更，按 REVIEW_PROCESS C 档）
1. **生产 ApprovalGate 接线**（S1）：修 = 在 `build_executor()` 加 `.with_approval(approval::global())`。决定默认策略：exec_command/delete_file/delegate 三类高危工具生产默认进审批门（120s fail-closed）。
2. **reminders 写入方缺失**（功能缺口，非缺陷）：全库 `INSERT INTO reminders` 仅在测试代码；生产无提醒创建路径，P1-1 闭环当前只覆盖查询/合并/唤醒段。需确认是否本轮补提醒创建工具/API。
3. **防重放键语义**（L1）：决定「同参幂等」还是「参数指纹区分」两种语义（影响台账复用面）。

### B 级（建议修复）
S3、S4、L2、L3、L4、L5、L6、D1、D2、D3、D4、M1、M2、M3、M4、M5、A1、A2、A3、A4、B1、B2、B3、B4、R1、R2、R3、R4
> 首推排序（按风险/成本比）：**S1 → B1（junction 逃逸）→ A1（watchdog 挂死）→ L1（防重放键）→ D1（时区字典序）→ L3（unreachable panic）**。

### C 级（提示/后续）
B5（黑名单补元字符）、S5（回环信任边界文档化）、D5（维持）。

---

## 4. 正面确认（代码质量亮点）

- **安全原语完整**：常量时间 token 比较、回环 /message 豁免 + 限流兜底、SSE 事件鉴权、no-store 头。
- **策略/能力模块化**：policy、capability、approval 分层清晰；ApprovalGate 120s fail-closed 语义有测试。
- **Phase 1 遗留全数落地**：统一分发 + guard_approval/guard_delegation + CallerTrust + M2 台账全覆盖（含 attempt 去重）。
- **审计/可观测性好**：每工具执行/熔断/重放均有台账；/status 探活（watchdog 心跳）+ /trace + /metrics/weekly。
- **唤醒架构**：N 提醒→1 次 LLM（成本合并）+ 周预算闸门先有账再设闸（R7 对齐）。
- **沙箱**：词法 + 链接双校验 + Windows 路径规范化（盘符大小写统一）设计用心。

---

## 5. 统计与总结

- **问题数**：HIGH 7（S1/L1/D1/M1/M2/A1/A2/B1/B2 —— 含并列共 9 处标注 HIGH 级行项），MEDIUM 20，LOW 1，INFO 5。
- **上一轮遗留**：4/4 已修复（但见 S1 装配缺口，修复需接线才生效）。
- **主线判断**：代码库处于「功能全、生产接线未闭环」状态——最紧急的是 **S1（审批门生产未接线）** 与 **B1（沙箱 junction 逃逸）**，均为本轮亲读实锤；其余 HIGH 属可靠性/一致性面，建议在修复轮内按 B 级排序处理。
- **未决事项**：A 级 3 项需用户拍板；本报告为只读交付，未编译、未改码。

> 下一步建议：用户批准后进入修复轮（先从 S1 + B1 + A1 三项高危入手），逐项带测试；修复前复核 🟡 锚点行号。
