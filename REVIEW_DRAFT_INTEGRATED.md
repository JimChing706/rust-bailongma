# REVIEW_DRAFT_INTEGRATED.md — 评审综合稿 v1.0（待专家评审）

> 2026-08-10 · 基于沙箱 `bailongma-rust` 实码（与 D 盘同版本）· 只设计未改码
> 前置依据：`RESEARCH_RELIABILITY_LLM_SAFETY.md`（六线研判）、`DESIGN_LLM_METRICS.md`（埋点设计）、`DRAFT_M1_LLM_METRICS.md`（M1 接入草案）、「初衷融合」讨论结论

---

## 一、背景与目标

在 BaiLongMa 既有代码（Rust 移植版，workspace：core 52 文件 / app 壳 + 3 CLI / sandbox 占位）基础上，围绕六条线探索更可靠、安全的处置方式，达成预期：

1. 幂等（LLM 重试 / 工具执行 / 消息去重 / 存储）
2. 唤醒（reminders / TICK / 承诺戳）
3. 自动迭代（快照回滚 / 收敛判据 / 人工介入）
4. LLM 资源优化调用效率（测量层 → 精准调校）
5. DL/NN 在代码开发规划与工程化细节中的高效落地研判
6. 代码安全（注入面 / 沙箱 / trust 分层 / 参数校验）

总原则：**先测量再调校，先安全后放开，观测是镜子不是手。**

---

## 二、事实基线（实证核对，非推测）

| # | 事实 | 实证来源 |
|---|------|---------|
| F1 | schema 共 24 张业务表 + FTS5 trigram + 3 触发器，幂等迁移三件套（IF NOT EXISTS / table_info 补列 / flag 守卫） | `db/schema.rs` |
| F2 | `reminders` 表在 schema 中存在（Node 遗留），但 Rust 侧 8 个 repositories **无其访问器**——提醒调度在 Rust 未闭环 | `db/repositories/*` |
| F3 | `Deduper` 是进程内 `HashMap<String, Instant>`，重启即失忆；显式 client_message_id TTL 10s / body 兜底 1.5s，入队失败立即释放槽位 | `api/routes.rs` |
| F4 | runtime 意识循环止于 M2 归属段（run_user_turn 闭环），LLM 轮未接线；desktop.rs 是纯窗口壳；沙箱副本 LLM 管线当前吃不到任何运行时埋点 | `runtime.rs` / `app/desktop.rs` |
| F5 | `tool_loop.rs` 模块头声称 `maxTotalCalls=30`，结构体无此字段，仅 max_rounds=100 兜底（文档与实现偏差） | `llm/tool_loop.rs` |
| F6 | `delegate.rs` `safe_prompt` 仅转义 `"` 与换行即拼入 `cmd.exe /C`，`%`/`&`/`|` 未处理——全库唯一肉眼可见注入面 | `agents/delegate.rs` |
| F7 | prompt cache 意识已内建：system_prompt STABLE 核心跨轮字节一致 + relocate_sections 重定位机制 | `memory/system_prompt.rs` |
| F8 | `Usage` 结构当前仅 hit/miss 两字段（DeepSeek 式）；OpenAI/Kimi 的 cached 取法需归一化 | `llm/types.rs` |
| F9 | 工具失败熔断对 send_message/express 汇报通道豁免（对齐 silent-exit 教训） | `llm/tool_loop.rs` |
| F10 | clippy 历史 14 项：2 处 too_many_arguments 与 1 处 needless_range_loop 已 #[allow] 带解释；question_mark×2 保留合理 | `clippy_out.txt` |

---

## 三、六线研判浓缩

### 1. 幂等
- **现状强**：入站去重（F3 双层 TTL）、存储幂等（INSERT OR IGNORE / upsert / 部分唯一索引）、重试语义（had_content 判定、429/401 不重试）。
- **真缺口**：①LLM 请求重试不幂等——provider 已完成但响应丢失 → 双计费 + 模型侧工具副作用重复执行；②工具执行无台账——write_file/send_message 成功但后续失败，tool_loop 重放会重复副作用。
- **对策**：request_id 逻辑请求边界 + llm_calls 幂等键（观测侧打底）+ 工具台账（审计侧）+ 幂等修复（P1 行为侧）。

### 2. 唤醒
- **现状**：TICK + 承诺戳（focus_stack 落库）已立。
- **缺口**：F2 reminders 无访问器；缺唤醒合并器（防多触发器风暴）；缺成本闸门（每次唤醒 = 一次 LLM 调用）。
- **对策**：reminders 访问器 + 合并器 + 基于 llm_metrics 的唤醒成本观测与周窗口预算。

### 3. 自动迭代
- **现状**：self_evolution / coding_discipline 已内建；门控意识好（factory review、规则草稿制）。
- **缺口**：改前快照→验证后自动回滚闭环未形式化（rescue 备份机制已有）；无显式收敛判据（目标达成 / 预算耗尽 / 人工介入三选一）；tool_loop 缺 maxTotalCalls 是同类循环失控隐患（F5）。
- **对策**：P4 启动前置三件套——人工介入硬通道 + 快照回滚闭环 + 收敛判据，缺一不进入自动改码。

### 4. LLM 效率调校
- **现状**：prompt cache 保稳已做对（F7）；无任何 tokens/TTFT/cache_rate/重试/降级指标落库——**盲调**。
- **对策**：P0 先埋 llm_metrics，一周数据说话，再谈缓存友好化 / 模型路由 / token 预算裁剪。cache_rate 数据可反哺既有 relocate_sections 机制（观测直接驱动 prompt 自调优）。

### 5. DL/NN 落地研判
- **结论**：最高 ROI 不是训练大模型，是三层架构——规则层（已有，0 成本）→ 本地小模型层（ONNX int8：意图分类/敏感检测/嵌入，<10ms）→ 旗舰层。
- **约束**：NN 只用于分类/排序/表征三个确定性子问题，控制逻辑保持符号化，不做端到端学习（工程可靠性底线）。
- **语料**：conversations 表自然积累，蒸馏前复用 brain_ui_events 脱敏链。

### 6. 代码安全
- **现状强**：安全中间件顺序（origin→access→CORS）、常量时间 token 比较、全参数化查询、递归脱敏、进程树回收（taskkill /T /F）、路径穿越防护（canonicalize + starts_with）。
- **待落地按序**：P0 delegate 注入面加固（F6）→ P1 prompt 注入 trust 分层 → P1 全工具参数 schema 校验 → P2 沙箱 M5（自动迭代安全前提）。

---

## 四、观测层设计浓缩（详见 DESIGN_LLM_METRICS.md）

**三张表**
| 表 | 粒度 | 幂等键 | 说明 |
|----|------|--------|------|
| llm_calls | 请求级 | UNIQUE(request_id) | 重试共享同一 ID + INSERT OR IGNORE → 天然幂等，兜住「重试不幂等」观测侧 |
| llm_tool_calls | 工具执行级 | UNIQUE(request_id, round, tool_name) | 防重放，补「工具执行无台账」观测侧 |
| llm_metrics_daily | 日聚合 | 永久保留 | 明细 20000 行滚动淘汰，长期趋势不断档 |

**埋点位置 10 处（P0 必埋 5 处）**：caller.rs（入口/首 chunk/流结束/错误分支）、retry.rs（重试决策点、429/401 分支）、tool_loop.rs（工具前后/熔断/round 上限）、injector.rs（section 命中计数 → context_bytes）、runtime.rs（turn 耗时/归属判定）。

**关键决策**：流路径只做内存记账（<1ms），mpsc 后台队列 30s/100 条批量 flush；cached_tokens 归一化 + usage_raw 兜底；total_calls 只计终态，retry_count 与 error_count 分计。

---

## 五、M1 接入草案摘要（详见 DRAFT_M1_LLM_METRICS.md）

- **改动面**：改 7 处 + 新 2 文件，零新依赖（tokio/serde_json/rusqlite/chrono/tracing 均在现有依赖）。
- **关键决策**：request_id 与 MetricsCollector 挂 StreamContext（不改签名，只连累 2 处测试构造补 None）；逻辑请求边界 = call_llm 每轮（round_ctx 印章，重试/降级共享 ID）；一行一请求、flusher 聚合 upsert；两个口径决定（total_calls 终态 / retry 与 error 分计）；cached_tokens 归一化为 hit、原始字段进 usage_raw。
- **如实标注**：沙箱副本 LLM 轮未接线（F4），M1 正确性由单测验收（含「重试失败→成功覆盖为 done」终态测试），接线后挂 request_id+metrics 即自动生效。
- **验收 5 条**：编译 / 单测 / 回归 / 真实调用 SQL / 幂等。

---

## 六、初衷融合定位

**初衷四词（从实码读出）**：意识循环（runtime 归属→注入→渲染）、长期陪伴（memory/threads/跨日追踪）、自我进化（self_evolution/coding_discipline）、多智能体协作（AI Collaborators/委托/capability_registry）。

**六线 → 初衷支撑映射**
| 六线 | 支撑的初衷 | 落点 |
|------|-----------|------|
| 幂等 | 协作的诚实性 | 多 agent 接力不重复动作，交接不掉链子 |
| 唤醒 | 陪伴的守约 | 承诺戳 + 到点醒来，跨 agent 异步节拍 |
| 自动迭代 | 进化的纪律 | 能回滚才敢进化，多 agent 并行改码才敢放开 |
| LLM 效率 | 可持续性 | 效率自觉本就是初衷，缺测量层 |
| DL/NN | 本地优先深化 | 本地小模型贴隐私初衷，协作路由有低成本层 |
| 安全 | 放权的前提 | trust 分层后才能把工具交给协作者 |

**三原则**：延伸以实码为锚（设计写到函数名级，不造平行宇宙）；观测是镜子不是手（不改语义，best-effort）；顺序即融合（P0→P4，每层是下一层使能者）。

**多智能体协作净效果**：骨架已在（归属/委托/能力注册表），缺三样让它敢跑——信任账本（幂等+台账，动作可审计不重复）、成本闸门（per-agent 度量，分级调度）、安全放权（沙箱+注入加固）。补上后协作从「演示级」到「可运行级」。

---

## 七、执行路线 v0（待评审修订）

| 阶段 | 内容 | 出口门槛 |
|------|------|---------|
| P0 | 观测层：M1 建表+caller/retry 埋点 → M2 工具台账 → M3 上下文统计 → M4 周报 | 单测全绿 + 接线后 SQL 验证 |
| P1 | 可靠性地基：唤醒闭环（reminders 访问器+合并器）、幂等修复、delegate 加固 | 回归 + 故障注入测试 |
| P2 | 安全落地：沙箱 M5、trust 分层、工具参数 schema 校验 | 注入面复测 0 告警 |
| P3 | 效率调校：基于周报的缓存友好化 / 模型路由 / token 预算 | 关键指标环比 |
| P4 | 自动迭代成熟 + DL/NN 架构评审与实施 | 三件套齐 + 收敛判据 |

---

## 八、提交专家评审的问题清单

- Q1. M1 在 LLM 轮未接线时落地是否过早？还是「观测必须先于一切」？
- Q2. llm_calls 以 request_id 为逻辑边界 + UNIQUE + INSERT OR IGNORE：attempt=1 失败态先落库、attempt=2 成功态后到，状态翻转语义是否安全？flusher 先 flush 错误态再 flush 成功态怎么办？
- Q3. 工具台账 UNIQUE(request_id, round, tool_name)：重试路径下同一 round 的合法工具调用是否被 IGNORE 误伤？（是否需加 attempt 维度？）
- Q4. cached_tokens 归一化三取法是否足够支撑 cache_rate，还是应全量存 usage_raw？
- Q5. 唤醒：reminders 访问器 + 合并器与成本闸门谁先？与 llm_metrics 的联动关系？
- Q6. 自动迭代：收敛判据与快照回滚，P4 前是否必须预置「人工介入」硬通道？
- Q7. DL/NN：语料从现在开始零成本积累（conversations 自然增长），还是 P4 再启动？
- Q8. delegate 注入面加固应提到 P0 还是维持 P2？（改动小 vs 优先级原则）
- Q9. 多智能体审计维度（工具台账带 delegated_from？）是否应在 M2 就位？
