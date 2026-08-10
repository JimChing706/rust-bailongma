# 工具能力模型（TOOL CAPABILITY MODEL）

> 落点：`crates/core/src/capability/mod.rs`（Phase 1-3）。

## 模型结构

每个内置工具声明一份 `ToolCapability`，字段如下：

| 字段 | 含义 |
|------|------|
| `name` | 工具名（策略引擎按名匹配） |
| `risk_level` | 风险等级：`Low` / `Medium` / `High` / `Critical` |
| `side_effect` | 副作用描述（是否写磁盘 / 网络 / 执行外部进程等） |
| `scopes` | 允许的作用域（文件路径根、网络域等） |
| `allowed_paths` / `deny_paths` | 路径白名单 / 黑名单 |
| `output_policy` | 输出策略（是否需脱敏 / 审计） |
| `requires_approval` | 是否显式要求人工确认 |

**审批判定规则**（`needs_approval()`）：

```
requires_approval == true 或 risk_level >= High → 默认需人工确认
```

## 内置工具清单（11 个）

| 工具 | 风险 | 默认审批 | 说明 |
|------|------|---------|------|
| `get_timestamp` | Low | 否 | 只读时间，无副作用 |
| `read_file` | Low | 否 | 只读，受 policy 路径约束 |
| `list_dir` | Low | 否 | 只读目录 |
| `write_file` | Low | 否 | 写文件，受路径约束 + 审计 |
| `make_dir` | Low | 否 | 建目录 |
| `delete_file` | High | **是** | 破坏性操作，需人工确认 |
| `exec_command` | Critical | **是** | 执行外部进程，最高风险，默认全量审批 |
| `search_memory` | Low | 否 | 记忆检索，只读 |
| `send_message` | Medium | 否 | 外发消息，有副作用但可控 |
| `collect_agents` | Low | 否 | 收集本机 Agent 信息（Phase 0 补齐） |
| `remind` | Medium | 否 | 创建提醒（Phase 0 补齐） |

> 风险等级逐项以 `capability/mod.rs` 中 `builtin()` 声明为准，`builtin_table_sane` 测试断言关键约束
> （exec_command 必须 Critical + approval；delete_file ≥ High；低风险工具免审批）。

## 决策语义

`PolicyEngine` 五类出口（`policy/mod.rs`）：

| 决策 | 含义 | 是否放行 |
|------|------|---------|
| `Allow` | 直接放行 | ✅ |
| `Deny(reason)` | 拒绝，记录原因 | ❌ |
| `RequireApproval(reason)` | 挂起等待人工确认 | ⏸（等用户） |
| `Sanitize { note, redacted }` | 脱敏后放行 | ✅ |
| `LimitScope { note, scope }` | 缩小作用域后放行 | ✅ |

未知工具 → 一律 `Deny`（fail-closed），即使标记已确认。

## 执行链接线

```
LLM 工具循环
  → NativeToolExecutor（tools/mod.rs，11 工具）
  → PolicyEngine::check_tool_call（capability 匹配 → 决策）
  → RequireApproval 时 → ApprovalGate（approval.rs）挂起
      → WS /scene 推送 choice 卡片 → 用户四抉择 → POST /approval 回传
      → 120s 超时按拒绝处理（fail-closed）
  → 放行 → 真实执行 → llm_tool_calls 台账落库（含 attempt 幂等键）
```


## P2-2：信任分层（TrustTier / CallerTrust）

- **TrustTier**（工具维度，`capability::trust_tier`）：由能力声明自动推导——
  `Trusted`（纯查询 / 沙箱内读写 / Medium 可控副作用）直接放行；
  `Approval`（`needs_approval()`，即 risk ≥ High 或 requires_approval）需人工确认；
  `Denied`（未知工具 / 未声明能力）恒拒。fail-closed：查不到声明即 Denied。
- **CallerTrust**（来源维度）：`System`（内部自动化）可放行 Approval 工具；
  `User` / `Agent` 需已获人工确认（`check_tool_call_with_caller`）。
  默认入口 `check_tool_call(name, approved)` 按 User 语义委托，保持旧行为兼容。
- **全工具参数 schema 校验**（`tools::validate`）：`execute` 分发前统一校验，
  未知参数 / 必填缺失 / null 必填 / 类型不符 / enum 越界 / 数组元素类型不符
  一律拒绝，替代各工具内部静默兜底（如 get_timestamp 的 format 越界原走 iso）。
  校验纯决策、无副作用，可重放。落地时实锤一处 schema 与实现不一致：
  `remind` 的 `now`（测试/调试时间注入）漏声明，已补入 schema。
