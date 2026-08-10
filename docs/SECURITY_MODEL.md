# 安全模型（SECURITY MODEL）

> bailongma-rust 安全基线总览（Phase 1：显式 Turn 状态机 + trace + policy/capability + 人工确认 + 注入防护）。

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

## 网络访问策略

- 云元数据地址 / 保留地址（`169.254.169.254`、`metadata.*.internal`）→ **拒绝**
- 私网地址段（`10.`/`192.168.`/`172.16-31.`/`127.`/`::1`/`fe80:`）→ **需人工确认**
- 公网 → 放行
- `host_of` 剥离端口后判定（IPv4 数字端口 / IPv6 方括号），避免 `192.168.1.10:8000` 误判

## 人工确认协议

1. 工具调用被 `PolicyEngine` 判为 `RequireApproval` → `ApprovalGate`（全同步 mpsc）挂起调用线程。
2. 场景面板推送 **confront 级 choice 卡片**：`[审批] 工具名：原因` + 四个选项。
3. 用户抉择回传 `POST /approval`：
   - `AllowOnce`：本次放行
   - `AllowSession`：本会话放行（会话级缓存）
   - `Deny`：拒绝
   - `Modify`：改参后重放（预留，Phase 2 细化）
4. **120 秒超时未回应 → 按拒绝处理**（fail-closed）。
5. 抉择结果写入审计轨迹；场景卡片自动移除。

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
# 全量回归（含 Phase 1 新增）：
#   turn_state 3 测 / turn 4 测 / capability 表断言 / policy 12 测
#   approval 9 测 / injector_format 注入测试 / security_regression 13 测（步骤 7）
#   server.rs token/LAN 集成测试（round 4/5）
```
