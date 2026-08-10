# bailongma 可靠性 · 效率 · 安全 — 研判与优化路线

> 基线：2026-08-10 对 sandbox/bailongma-rust（与 D:\JimChing\rust-bailongma 同版本）全库精细化审验完成后的研判。
> 本报告只分析、不改码。所有「现状」均来自审验实证（文件级证据），「对策」为设计方向，待确认后另行立项。

---

## 0. 六条线一句话结论

| 线 | 结论 |
|---|---|
| 幂等 | 入站去重与存储层已强；缺口在 **LLM 请求重试** 与 **工具执行台账** 两层，属 at-least-once 与 exactly-once 的经典缝 |
| 唤醒 | TICK 心跳+承诺戳已立；`reminders` 表在 schema 但 Rust 侧无访问器（调度未闭环）；缺唤醒合并与成本闸门 |
| 自动迭代 | 门控意识好（factory review、规则草稿制）；缺「改前快照 → 验证后回滚」闭环与显式收敛判据 |
| LLM 效率 | prompt cache 保稳已做对（STABLE 核心字节一致）；**缺测量层，调优等于盲调**——必须先埋 metrics |
| DL/NN 落地 | 最高 ROI 是「规则层 → 本地小模型层 → 旗舰层」三层架构；不做端到端学习（不可解释、难调试、易翻车） |
| 代码安全 | 网络/数据/进程层已强；唯一肉眼可见注入面 `delegate.rs safe_prompt` 待加固；沙箱 M5 是自动迭代安全的前提 |

---

## 1. 幂等（Idempotency）

### 1.1 现状（实证）

| 层 | 机制 | 证据 |
|---|---|---|
| 入站去重 | 显式 `client_message_id`（8-128 白名单）TTL 10s + 无 ID 时 body 兜底 TTL 1.5s；入队失败立即释放槽位 | `api/routes.rs` Deduper |
| 出站防重 | `find_unanswered_delivered_outbound`：用户任意渠道最近发言都算接茬（跨 channel 语义，防重复投递） | `db/repositories/conversations.rs` |
| 送达语义 | outbound snapshot：send_message 成功即送达证据，无回复≠未送达、不得重发（反话痨硬边界） | `memory/messages.rs` |
| 存储幂等 | 迁移三件套（CREATE IF NOT EXISTS + PRAGMA 补列 + flag 守卫）；`mem_id` 部分唯一索引；SceneStore 内容无变化不广播 | `db/schema.rs` / `scene/store.rs` |
| 重试语义 | 流中断且未出内容才重试；429 不重试（交外层配额）；401 不降级 | `llm/retry.rs` |

### 1.2 缺口与对策

1. **LLM 请求层不幂等（P1）**
   - 风险：provider 端已完成、响应在途丢失/超时误判 → 重发 = 双计费 + 模型侧副作用（同一工具被调两次）。
   - 对策：请求级 idempotency key（provider 支持则透传）；至少记录「已发出请求的 (model, prompt_hash, tool_calls)」短账本，重试前比对。

2. **工具执行无台账（P1）**
   - 风险：`send_message`/`write_file` 已成功但后续失败 → 重放整个 tool_loop → 重复发消息/重复写文件。
   - 对策：工具调用 ledger 表（call_id, tool, args_hash, result_hash, status, turn_id），重放前查重；成功结果缓存，失败重试走指数退避。

3. **去重槽在进程内内存（P2）**
   - 实证：`Deduper` 是 `HashMap<String, Instant>`，重启即失忆 → 重启窗口内重发的消息可能双处理。
   - 对策：去重键落 SQLite（带 TTL 清理任务），或启动时接受 10s 极小概率窗口并文档化。当前桌面单进程可接受，但建议把 Deduper 抽象成 trait，为多实例留口。

4. **`(from_id, timestamp)` 作定位键（P3）**
   - 风险：同秒多条消息碰撞（`update_user_message_focus_topic` 回填路径）。
   - 对策：消息表加自增 seq 或内容 hash 作稳定键，回填用稳定键而非时间戳。

5. **TICK 重入幂等（P2）**
   - 现状：tick_counter 递增 + 承诺线索印章已实现；但 TICK 轮持久化在工具循环后（`touchCommitmentThread`），该段尚未接线。
   - 对策：承诺戳写入 + 幂等守卫（同一 commitment 只戳一次）；补「无承诺回退前台」分支的真实测试（现测试注释与断言不符，A 类#3）。

---

## 2. 唤醒（Wakeup）

### 2.1 现状（实证）

- TICK 心跳：`runtime.rs` 归属段（tick_counter、印章到最近开放承诺线索）；承诺落库未接线。
- 规则触发：manage_rule（context / automation；external_content → 禁用草稿；高危脚本规则需显式批准）。
- 提醒：schema 有 `reminders` 表（Node 遗留），但 **Rust 8 个 repositories 无 reminders 访问器** → 调度/到期触发未闭环。
- SSE intent → 注入意识循环（scene.rs）；60s 空闲断开；wttr 30 分钟缓存。

### 2.2 缺口与对策

1. **提醒调度闭环（P0）**：reminders repo（CRUD + 到期扫描）+ tokio interval 调度器 + 触发幂等标记（fired_at 守卫）+ 重启恢复。这是「唤醒」最直接的可靠性缺口。
2. **唤醒合并器（P1）**：同 tick 多触发器（消息+提醒+规则+SSE intent）→ 合并为单次 runTurn，防唤醒风暴；`processing_preempted` 事件已存在，语义闭环可复用。
3. **唤醒成本闸门（P1）**：每次唤醒 = 一次 LLM 调用。加：静默时段、唤醒频率上限、低价值唤醒走规则/小模型直答（不消耗旗舰 token）。
4. **唤醒可靠性（P2）**：TICK 轮 panic 的循环级 watchdog（任务级 panic 需重启 tick 循环，区别于 Mutex 中毒恢复）；壳层 /status 健康检查端点可扩展为外部探活。
5. **假死检测（P3）**：若 45s LLM watchdog 之外的循环卡死，需要心跳探活 + 自愈（重启循环任务并记 brain_ui_events）。

---

## 3. 自动迭代（Auto-iteration / Self-evolution）

### 3.1 现状（实证）

- self_evolution.rs：mem_id 去重、recent 合并、7 天窗口渲染。
- coding_discipline.rs：教训注入（含「PowerShell 读 UTF-8 变 GBK 毁多字节」这类踩坑内化）。
- 工具工厂：propose → review（确定性策略+测试）→ install 三态门控。
- 规则：external_content 一律禁用草稿，显式批准才启用。
- 审计：brain_ui_events 全链路脱敏落库（800 行有界、best-effort）。

### 3.2 缺口与对策

1. **改前快照 → 验证后回滚（P0）**：沙箱 rescue 备份机制已存在（今日 5 个），形式化：自动修改前自动快照（文件 hash 清单），修改后自动跑验证（cargo test / 目标脚本），失败自动回滚到快照并记 audit。
2. **收敛判据（P0）**：自动迭代必须有显式终止条件——目标指标达成 / 预算（轮数+token+时间）耗尽 / 人工介入。`tool_loop` 缺 `maxTotalCalls=30`（A 类#1）正是同类问题的另一处表现：所有循环都要双上限。
3. **变更审计表（P1）**：actor=agent 的每次自动变更记 (before_hash, after_hash, 原因, 验证结果)，支持一键回滚。当前 brain_ui_events 可承载事件，但需要结构化的 code_change 类型。
4. **权限分层（P1）**：自动迭代只允许改沙箱内自有代码；涉及用户目录/系统级/安全模块（sandbox、权限、脱敏）的修改必须人工审批（对齐规则草稿制）。
5. **自我修改的灰度（P2）**：改自身 prompt/规则时先 dry-run（离线渲染对比）→ 低风险场景试用 → 全量；防一次改动破坏核心行为（如 STABLE 核心字节漂移砸掉 prompt cache）。

---

## 4. LLM 资源优化与效率指标精准调校

### 4.1 已做对的部分（实证）

- STABLE 核心跨轮字节一致保 provider prompt cache（system_prompt.rs，段重定位 + strip_level2 防固定区膨胀）。
- retry had_content 语义、429 不重试、MiMo 降级链跳过 deprecated。
- 检索少即是强（salience≥4 提桶、365 天陈旧下沉、fts_floor）。

### 4.2 缺口：没有测量层（P0）

没有 metrics，一切调校都是猜。先埋点再调校：

| 指标 | 含义 | 用途 |
|---|---|---|
| input/output tokens | 每轮 token 账 | 成本、裁剪预算 |
| TTFT | 首 token 时延 | 缓存命中探测（命中则 TTFT 显著下降） |
| cache_hit | provider 返回的缓存标记（若有） | 缓存率度量 |
| retry / degrade 次数及原因 | 重试与降级流水 | 重试经济性、provider 优先级 |
| tool 调用数 / 轮数 | 每轮复杂度 | 循环上限调参、死循环告警 |
| 估算成本 | tokens × 单价 | 每日汇总报表 |

落点：新增 `llm_metrics` 表（或复用 brain_ui_events 结构化类型），每条 LLM 轮写一行；提供按日聚合查询。

### 4.3 调校杠杆（按 ROI 排序）

1. **缓存命中率**（收益最大）：固定核心已稳；检查动态段是否有非确定性内容（时间戳格式、随机文本）→ 统一缓存友好格式；注入段排序稳定；用 TTFT 对比验证命中。若 provider 缓存计费折扣，成本可降 30-50%。
2. **token 预算裁剪**：给 memories/threads/task-knowledge 设预算上限，超预算按 salience 截断；FTS5 top-k 调参（当前默认值待实测）。
3. **模型路由**：意图分类/关键词命中/简单问答 → 规则或小模型；复杂推理/代码生成 → 旗舰（K3）。providers.rs 已有 7 家注册表，加 routing policy 层即可。
4. **流式 vs 非流式**：用户可见轮保流式；内部工具循环可走非流式快路径（少解析开销）。
5. **重试经济性**：指数退避+抖动；瞬时（5xx/超时）才重试，4xx 不重试；每轮重试预算上限。
6. **Embedding 落地**：NoopEmbedder → 本地 ONNX 小模型，内容 hash 缓存防重复计算；召回变准 → 注入的无用记忆变少 → 间接省 token。

---

## 5. DL/NN 高效落地研判（代码开发规划 + 工程化细节）

### 5.1 研判结论

**桌面个人助理场景，最高 ROI 不是训练/微调大模型，而是「规则 → 小模型 → 旗舰」三层架构 + 检索增强**：

- L0 规则层：关键词/正则/启发式（已有 keywords/temporal/intent 判定，0 成本，已很强）。
- L1 小模型层（本地推理，CPU 友好）：
  - 意图分类（唤醒判定 / 闲聊 / 任务型 / 代码类）——每个 <10ms；
  - 敏感/PII 检测（进 prompt 前过滤，安全前置）；
  - 嵌入模型（记忆召回，替代 NoopEmbedder）。
- L2 旗舰层：复杂推理/代码生成——把省下的预算集中投这里。

### 5.2 工程化细节研判

1. **推理运行时**：候选 ort（ONNX Runtime，算子覆盖全、int8 量化成熟）与 candle（纯 Rust 无 Python 依赖）。建议 ort + int8；模型文件随资源目录发布，CPU-only 无 GPU 依赖。
2. **蒸馏管线**：用旗舰模型标注历史对话（现有 conversations 表就是天然语料）→ 蒸馏意图小分类器 → 回归测试通过才替换规则层。这是「从既有代码/对话学习」的自然闭环，也是「深度学习落地」最务实的形态。
3. **评估先行**：先立 eval 集（100-200 条真实历史消息 + 标注），任何小模型上线必须过回归门槛（准确率 ≥ 阈值、时延 ≤ 预算、成本下降可量化）。
4. **NN 应用边界（工程可靠性底线）**：只在「分类 / 排序 / 表征」三个确定性子问题上用 NN；控制逻辑保持符号化/确定性。不做端到端学习整个 agent 流程——不可解释、难调试、翻车无法定位。
5. **嵌入索引工程**：内容 hash 去重、增量索引、夜间空闲批量嵌入、维度压缩（512→256）控内存；桌面应用内存预算（百 MB 级）内可承受十万级向量。
6. **LLM 用于代码开发规划**：自动生成 → clippy `-D warnings` → cargo test → 准入（纪律文本已存在，下一步代码化门控）；安全相关模块（sandbox/权限/脱敏）禁止自动修改，必须人工审批；生成代码必须过验证才能合入，失败自动回滚（呼应 §3）。

---

## 6. 代码安全落地

### 6.1 已强（实证）

- 网络层：origin → access → token → CORS 固定顺序；WS 授权分级（回环直通但 origin 检查在前）；常量时间 token 比较；路径穿越防护（canonicalize + starts_with，含 %2f 测试）。
- 数据层：全参数化查询；brain_ui_events 递归脱敏（敏感键/明文 sk- 密钥/嵌套截断/有界表）。
- 进程层：`taskkill /T /F` 进程树回收、KillOnDrop（tokio Child drop 不杀进程）、64KB 管道排空。
- 门控：factory review 确定性策略；规则草案制；密钥进 credential store。

### 6.2 待落地（按优先级）

1. **P0 — delegate.rs 注入面加固**：`safe_prompt` 只转义 `"` 和换行就拼 `cmd.exe /C`，`%`/`&`/`|`/`^`/`<`/`>` 未处理。对策：不用 cmd /C 拼串，改 `Command::new` + 参数数组直启（Windows CreateProcess 语义，天然免转义）；或完整转义 + 转义矩阵测试。
2. **P0 — sandbox M5 落地**：能力令牌、路径/命令/域白名单、Windows Job Object + 受限 Token。这是「自动迭代跑在受限沙箱里」的前提（§3 依赖它）。
3. **P1 — Prompt 注入防护**：记忆/外部内容进 system prompt 视为不可信，加 trust 分层（user direct > agent observation > external content）；敏感操作指令必须来自 user 角色或显式授权；STABLE 固定核心永不可被注入覆盖。
4. **P1 — 工具参数 schema 校验**：模型输出为不可信输入，执行前 JSON Schema 校验（factory 已做，推广到全部工具）。
5. **P1 — 密钥最小暴露**：prompt 不含密钥；错误消息中的 URL/参数脱敏；API key 仅 credential store。
6. **P2 — 供应链**：cargo deny/audit 进验证流程；依赖版本固定；rustls 已避开 openssl 供应链面，保持。
7. **P2 — 审计完备**：exec/网络/写文件/发消息四类敏感动作全记 brain_ui_events（redacted 参数），提供审计查询。

---

## 7. 分阶段路线（建议执行顺序）

| 阶段 | 内容 | 前置条件 | 验收度量 |
|---|---|---|---|
| P0 观测先行 | llm_metrics 埋点（tokens/TTFT/retry/degrade/cache/轮数） | 无 | 每轮一行、按日聚合报表可用 |
| P1 可靠性地基 | reminders 闭环 + 唤醒合并器 + 去重键落库 + 工具 ledger + A 类三处文档修正 | P0（用量数据校准预算） | 重启不丢提醒；重放不双发；文档=实现 |
| P2 安全落地 | delegate 加固 + 沙箱 M5 MVP + 全工具参数校验 | 无 | 注入面归零；沙箱拒越权；校验拦非法参数 |
| P3 效率调校 | 缓存友好化 + 模型路由 + token 预算裁剪 + 本地 embedding | P0 数据 + P2 安全 | 缓存命中率↑、成本/轮↓、TTFT↓，均有基线对比 |
| P4 自动迭代成熟 | 快照/回滚闭环 + 审计表 + 收敛判据 + 小模型蒸馏上线 | P2（沙箱）+ P3（成本可控） | 自动修改零失控、回滚可复现、小模型过回归门槛 |

**原则**：先测量再调校（P0 不做，P3 是盲调）；先安全后放开（P2 不做，P4 的自动迭代有失控风险）；每阶段有明确验收度量，符合 grill-me 纪律（diagnose → fix → verify）。

---

## 附：与既有审验报告的衔接

- A 类三处（tool_loop maxTotalCalls / logging 假 winapi / runtime tick 测试注释）纳入 P1「文档=实现」修正。
- C 类一处（delegate safe_prompt 注入面）升级为 P2 首项（安全标注 → 实际加固）。
- 范围缺口（意识循环 M2 后半段、/message 入队占位、sandbox M5 占位、NoopEmbedder）分别对应 P1（唤醒可靠性）、P2（沙箱）、P3（embedding）。
