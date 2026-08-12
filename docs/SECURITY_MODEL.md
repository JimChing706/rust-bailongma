# 安全模型（SECURITY MODEL）

> bailongma-rust 安全基线总览（Phase 1：显式 Turn 状态机 + trace + policy/capability + 人工确认 + 注入防护；
> Phase 2：改参执行闭环 + 工具调用轨迹可观测）。

## 信任分级

上下文 section 按来源分四级信任（`memory/injector_format.rs::SectionTag`）：

| 来源 | 信任 | instruction_allowed | can_trigger_tool | 说明 |
|------|------|--------------------|------------------|------|
| `system` | 最高 | ✅ true | ✅ true | 系统提示词，唯一可携带指令 |
| `memory` | 中 | ❌ false | ❌ false | 记忆内容只作背景，不可带指令 |
| `user` | 中 | ❌ false | ❌ false | 用户消息按数据对待，不可越权 |
| `external` | 最低 | ❌ false | ❌ false | 外部抓取内容，渲染前转义 |

渲染时每段带 `<!-- SECTION source=.. trust=.. instruction_allowed=.. can_trigger_tool=.. -->`
头注，LLM 侧可识别分区边界；不可信内容中的 `<`/`>` 等转义，杜绝 `</context><system>...` 闭合注入。

另有 `ToolResult` 源（工具执行结果回喂）：系统生成内容，归最高信任层（不携带
不可信指令），作为扩展来源由 injector_format 注入（Phase 1 审计补记）。

## 网络访问策略

- 云元数据地址 / 保留地址（`169.254.169.254`、`metadata.*.internal`）→ **拒绝**
- 私网地址段（`10.`/`192.168.`/`172.16-31.`/`127.`/`::1`/`fe80:`）→ **需人工确认**
- 公网 → 放行
- `host_of` 剥离端口后判定（IPv4 数字端口 / IPv6 方括号），避免 `192.168.1.10:8000` 误判

## 人工确认协议

1. 工具调用被 `PolicyEngine` 判为 `RequireApproval` → `ApprovalGate`（全同步 mpsc）挂起调用线程。
2. 场景面板推送 **confront 级 choice 卡片**：`[审批] 工具名：原因` + 四个选项。
3. 用户抉择回传 `POST /approval`（`{ id, decision }`）：
   - `allow_once`：本次放行
   - `allow_session`：本会话放行（会话级缓存）
   - `deny`：拒绝
   - `modify:<新参数>`：**改参后放行**（Phase 2 落地）——执行端以新参数替换原参数执行
4. **未知抉择值 fail-closed 按拒绝处理**（不静默当作任何意图，杜绝拼写错误变相放行）。
5. **120 秒超时未回应 → 按拒绝处理**（fail-closed）。
6. 抉择结果写入轨迹（stage=`approval`）；场景卡片自动移除。

## 工具调用轨迹（Phase 2 可观测性）

`trace` 模块记录每次工具调用的决策链路，三 stage 全链路：

| stage | 内容 | decision 取值 |
|-------|------|---------------|
| `guard` | 策略判定 | `allow` / `deny` / `require_approval` |
| `approval` | 人工确认结果（含审批耗时 ms） | `approved` / `modify` / `denied` / 超时 |
| `execute` | 实际执行结果（含耗时 ms，全工具统一记录） | `ok` / `timeout` / `err` |

- 内存环形缓冲（上限 10k 条，先进先出），进程级单例。
- `execute` stage 由 `NativeToolExecutor::execute` 分发层统一记录（Phase 1 修复 E），
  不再只覆盖 exec_command；`guard`/`approval` stage 由 ApprovalGate 记录。
- 查询：`GET /trace?limit=50&tool=exec_command`（时间倒序，limit 上限 500）。
- 工具调用台账落库由 `llm_tool_calls`（工具循环层）承接，本层专注决策链路。

## 状态机与恢复

`run_conscious_turn` 全程落 `turn_state`：`received → running → completed / failed`。
启动时扫描未终态记录，按 `recover_policy` 恢复（避免崩溃后悬挂、重复执行）。

## 幂等与防重放

- `llm_tool_calls` 台账唯一键含 attempt 维度 → 同一次调用重试不双写。
- LLM 重试共享 `request_id`，`Idempotency-Key` 头保证服务端幂等。
- 工具成功终态不覆盖错误终态。

## 限流

`/message` 来源级限流：默认 30 次 / 10 秒，auth 探测也占槽位，防洪泛。

## 审计

`PolicyEngine` 每次判定（工具 / 文件 / 网络 / 记忆 / 输出）写入 `audit_trail`（环形缓冲，上限 10k）。
每条含 `ts_ms / action / decision`。后续接线落库支持跨重启追查。

## 验收

```bash
cargo test --workspace
# 全量回归（含 Phase 1/2 新增）：
#   turn_state 3 测 / turn 4 测 / capability 表断言 / policy 12 测
#   approval 11 测（含 modify 改参回传、parse 未知值 fail-closed）
#   trace 3 测 / injector_format 注入测试 / security_regression 15 测
#   server.rs token/LAN 集成测试（round 4/5）
```
