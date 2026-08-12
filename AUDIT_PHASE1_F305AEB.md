# Phase 1 三件套逐条审计（基线 f305aeb，2026-08-12）

> 审计对象：`docs/THREAT_MODEL.md`、`docs/TOOL_CAPABILITY_MODEL.md`、`docs/SECURITY_MODEL.md`
> （commit f7b4e71「Phase1-7」交付的 docs 三件套）。
> 方法：文档承诺 → 代码锚点 → 落地 / 部分落地 / 未落地 + 证据行。
> 证据分级：🟢=本轮直接读码确认；🟡=先前轮次（R2–R5 / M1–M5）已验证，本轮未重读。

---

## 一、THREAT_MODEL.md（12 条威胁清单）

| # | 威胁 | 结论 | 代码锚点 | 证据 |
|---|------|------|---------|------|
| 1 | 未授权访问（LAN 裸读） | ✅ 落地 | `crates/app/src/api/server.rs::guard_request`、`crates/core/src/api/security.rs` | 🟡 R4 重写非回环分支强制 Bearer token；R2 四组矩阵实测全绿；commit 51cf210 含 3 回归测试 |
| 2 | Token 缺失暴露态 | ✅ 落地 | `api/security.rs::lan_exposure_check` | 🟡 R3 fail-closed：allowLanAccess=true 但无 token 拒绝启动；运行期空 token → 远端一律 403 |
| 3 | 路径穿越 | ✅ 落地（policy 层） | `crates/core/src/policy/mod.rs::is_within`、`tools/mod.rs::resolve_under_root` | 🟢 is_within 拒绝 ParentDir 组件 + 组件级比较；测试 `is_within_boundaries`、`prefix_collision_sibling_rejected`。static_assets.rs / main.rs `--root` 属 R5 审计面，状态待核 |
| 4 | 命令注入 | ⚠️ 部分落地 | `agents/delegate.rs::run_command_with_args`、`tools/mod.rs::exec_command` | 🟢 delegate 侧：参数数组直启不经 shell + 超时强杀进程树 ✅（测试 `cli_metachar_prompt_not_injected`）；**缺**：文档承诺的「argv[0] 精确匹配白名单」未实现（invoke_cmd 直启前无白名单校验）；`exec_command` 工具本身仍走 `cmd /C`（shell 路径），未参数数组化 |
| 5 | Prompt/上下文注入 | ✅ 落地 | `memory/injector_format.rs` | 🟢 SectionTag 四级（system/memory/user/external）+ `sanitize_untrusted` 转义 + section_kind 打标签；测试 `context_block_tags_untrusted_sections_and_escapes_content` 等 3 项 |
| 6 | 高风险工具滥用 | ⚠️ 部分落地 | `capability/mod.rs`、`policy/mod.rs`、`approval.rs`、`tools/mod.rs` | 🟢 声明层全 ✅：needs_approval（risk≥High）+ RequireApproval + ApprovalGate 全同步 mpsc + 四抉择 + modify 改参 + 120s 超时 + 未知值 fail-closed；**缺**：执行链路只有 `exec_command` 挂了门（tools/mod.rs 内部 guard_tool_call），`delete_file`（High）在 execute 分发里不过 ApprovalGate |
| 7 | SSRF / 云元数据 | ✅ 落地 | `policy/mod.rs::network_decision` | 🟢 NETWORK_DENY_EXACT + 私网段前缀 + `host_of` 剥离端口；测试 3 项全过 |
| 8 | 敏感信息泄露 | ✅ 落地 | `policy/mod.rs::sanitize_content` | 🟢 SECRET_PATTERNS（PEM/AWS/sk-/ghp_/xox）+ 超长 hex/base64 兜底；测试 `output_private_key_redacted`。注：无正则依赖，关键词扫描实现 |
| 9 | 重放 / 幂等破坏 | ✅ 落地 | `db/schema.rs`、`llm/replay.rs`、`llm/tool_loop.rs` | 🟢 tool_loop 台账唯一键含 attempt（E1 修复：UNIQUE(request_id,round,attempt,tool_name)）；DbToolReplayGuard 重试复用（测试 `tool_replay_guard_prevents_double_execution_on_lost_response`）；成功终态不覆盖错误 🟡 |
| 10 | 洪泛 / DoS | ✅ 落地 | `api/security.rs::RateLimiter` | 🟡 来源级限流默认 30 次/10s，auth 探测占槽位；R2 API 实测 |
| 11 | 审计缺失 | ✅ 落地 | `policy/mod.rs::audit_trail` | 🟢 环形缓冲上限 10k + 每次判定 record；测试 `audit_trail_recorded` |
| 12 | Turn 悬挂 | ✅ 落地（核心） | `turn/mod.rs`、`db/repositories/turn_state.rs` | 🟢 六态状态机 + recover_policy 恢复（文档明言 resume/cancel/replay API 后续小步接入，与本轮状态一致）；turn_state 3 测 🟡 |

**已知边界**：DNS 解析级 SSRF 文档明说不做，代码 network_decision 静态判定一致 ✅；审计仅内存环形一致 ✅。

---

## 二、TOOL_CAPABILITY_MODEL.md

| 条目 | 结论 | 证据 |
|------|------|------|
| 工具清单「11 个」 | ⚠️ 文档滞后 | 🟢 代码 BUILTIN 实际 13 个（新增 matter_create / matter_query，M5 落地），文档未回写 |
| 风险等级表 | ⚠️ 3 处不符 | 🟢 代码（capability/mod.rs BUILTIN）vs 文档：read_file=Medium（文档 Low）、write_file=Medium（文档 Low）、remind=Low（文档 Medium）；其余 9 个一致 |
| 审批判定 needs_approval | ✅ 落地 | 🟢 risk≥High 或 requires_approval；测试 `builtin_table_sane` 断言 exec_command=Critical+approval、delete_file≥High |
| 五类出口 + 未知工具 Deny | ✅ 落地 | 🟢 PolicyDecision 五变体 + builtin(name)=None → Deny |
| 执行链接线（工具循环→PolicyEngine→ApprovalGate） | ⚠️ 部分落地 | 🟢 **实际**：NativeToolExecutor::execute 只调 `validate::validate_args`，工具循环层不经过 PolicyEngine；ApprovalGate 仅在 exec_command 内部直接 guard_tool_call（其内部委托 policy.check_tool_call ✅）。「ApprovalGate→PolicyEngine」真实，「工具循环→PolicyEngine」未接线 |
| P2-2 TrustTier / CallerTrust | ⚠️ 实现 ✅ 接线未用 | 🟢 推导+检查函数+单测全（`trust_tier_derivation`、`caller_trust_tiers`）；但生产调用链只有 approval.guard_tool_call 走默认 User 语义，check_tool_call_with_caller 无真实调用方 |
| 全工具参数 schema 校验 | ✅ 落地 | 🟢 tools/validate.rs 分发前统一校验（未知参数/必填缺失/null/类型/enum/数组元素），测试齐全；remind 的 now 已补声明 |
| **新发现**：delegate 工具未注册 | ⚠️ 未落地 | 🟢 `delegate_to_agent` / `grant_agent_delegation` 有完整 schema + exec + 14 项单测（agents/delegate.rs），但不在 `all_tool_schemas()` 与 NativeToolExecutor::execute 分发中 → 工具循环当前**不可调用**；validate 亦不覆盖。verify_hint 本轮已加入 schema，未来注册后 P2-2 参数校验自动同步 |

---

## 三、SECURITY_MODEL.md

| 条目 | 结论 | 证据 |
|------|------|------|
| 信任分级 SectionTag | ✅ 落地 | 🟢 四级 + instruction_allowed/can_trigger_tool + render_header + 转义。注：代码另有 ToolResult 源（文档未列，属扩展） |
| 网络访问策略 | ✅ 落地 | 🟢 同威胁 7 |
| 人工确认协议（四抉择/modify/未知值/120s/轨迹） | ✅ 落地 | 🟢 approval.rs 全项；「场景卡片推送」on_request 回调由 API 层注入（server.rs 桥接，🟡 先前轮次接线），`cancel_all` 进程关闭兜底 |
| 工具调用轨迹三 stage | ⚠️ 部分落地 | 🟢 trace.rs guard/approval/execute 三 stage + 环形 10k ✅；**缺**：execute stage 只在 exec_command 路径记录（tools/mod.rs），其余工具无 execute 轨迹；GET /trace 接口在 app 层（routes.rs），🟡 未核 |
| 状态机与恢复 | ✅ 落地 | 🟢 turn/mod.rs 六态 + recover；turn_state 3 测 🟡 |
| 幂等与防重放 | ✅ 落地 | 🟢 同威胁 9 |
| 限流 | ✅ 落地 | 🟡 同威胁 10 |
| 审计 | ✅ 落地 | 🟢 同威胁 11 |
| 验收测试数（turn_state 3/turn 4/approval 11/trace 3/policy 12/security_regression 15…） | ⏳ 待全量核对 | 🟡 基线记忆 567 passed / 0 failed / 1 ignored（f305aeb）；逐数对照需跑全量回归 |

---

## 四、汇总

**全落地（15 项）**：威胁 1/2/5/7/8/9/10/11/12；TOOL_CAPABILITY 审批判定/五类出口/参数校验；SECURITY_MODEL 信任分级/网络/确认协议/状态机/幂等/限流/审计。

**部分落地（4 处，均为「声明/实现有，生产链路缺」）**：
1. 威胁 4：命令注入缺 argv[0] 白名单层；exec_command 工具仍走 shell。
2. 威胁 6：delete_file 声明 High 需确认，执行链路未挂 ApprovalGate。
3. TOOL_CAPABILITY 执行链接线：工具循环层不经过 PolicyEngine / CallerTrust 分层未用（与外部审查「主链未端到端闭环」结论同源）。
4. trace execute stage 仅覆盖 exec_command。

**文档滞后（3 处）**：工具清单 11→13；read_file/write_file/remind 风险等级；SECURITY_MODEL 未列 ToolResult 源。

**新增发现（1 处）**：delegate 工具（含 verify_hint）未注册进工具循环，当前不可被 LLM 调用。

---

## 五、修复建议（待审批，按波次）

- **A（文档回写，低风险）**：TOOL_CAPABILITY_MODEL 工具表更新为 13 个 + 风险等级修正；SECURITY_MODEL 补 ToolResult 源。
- **B（delete_file 挂门，低风险）**：NativeToolExecutor::execute 分发对 needs_approval 工具统一走 ApprovalGate（与 exec_command 同路径）。
- **C（delegate 注册进工具循环，中风险）**：all_tool_schemas + execute 分发 + capability BUILTIN 补 delegate_to_agent/grant_agent_delegation（risk 定 High/Approval）→ 主链工具面闭环，verify_hint 随之上线生效。
- **D（工具循环接入 PolicyEngine，中风险）**：execute 前统一 check_tool_call_with_caller，CallerTrust 分层真正生效。
- **E（trace 全覆盖，低风险）**：ToolExecutor::execute 统一记录 execute stage。
