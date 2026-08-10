# 威胁模型（THREAT MODEL）

> 适用范围：bailongma-rust 核心运行时（`crates/core`）与 API 层（`crates/app`）。
> 对应落地：Phase 1 安全基线（turn 状态机 / policy / capability / 人工确认 / 注入防护）
> 叠加前 5 轮审计修复（R1–R5）。

## 设计原则

1. **Fail-closed**：任何校验缺失、token 缺失、来源不可判定时一律拒绝，不依赖"启动时检查过"的单一信任。
2. **最小权限**：文件操作限定 workspace 根；工具调用按能力声明放行；未知工具直接拒绝。
3. **纵深防御**：同一威胁至少两层防护（例：命令注入 = argv[0] 白名单 + 参数数组直启 + 拒 shell 元字符）。
4. **人工确认兜底**：高风险副作用（命令执行、文件删除、私网访问、记忆写）默认挂起等用户抉择。
5. **审计留痕**：每次策略判定写入审计轨迹，可回溯。

## 威胁清单

| # | 威胁 | 攻击面 / 载体 | 缓解措施 | 代码落点 | 验证 |
|---|------|--------------|---------|---------|------|
| 1 | 未授权访问（LAN 裸读） | 网内任意设备直连 `/message`、`/events/history`（含 LLM 日志）、SSE、静态资源 | `guard_request` 三层校验：origin → access（回环 / Bearer token）→ CORS；token 配置后所有远端请求强制 Bearer | `api/server.rs`、`api/security.rs` | server.rs 集成测试（round 4/5） |
| 2 | Token 缺失暴露态 | 开 LAN 但漏配 `BAILONGMA_API_TOKEN` | 启动 `lan_exposure_check` fail-closed；运行期 token 为空 → 远端一律 403（不信任启动检查单一路径） | `api/security.rs::lan_exposure_check` | 启动检查 + `lan_read_forbidden_without_token_even_when_lan_enabled` |
| 3 | 路径穿越 | `..` 逃逸、绝对路径越界、前缀碰撞 | 组件级 `is_within` 判定（a 等于 b 或在 b 完整组件序列内）；敏感路径 denylist（`.ssh`/`.env`/`credentials`/`.pem` 等）；sandbox 强制 `--root` | `policy/mod.rs`、`api/static_assets.rs`、`main.rs` | policy 12 测 + `security_regression::path_traversal_*` |
| 4 | 命令注入 | prompt 携带 `& \| < > %` 等 shell 元字符、拼接命令链 | `run_command_with_args` 参数数组直启（不经任何 shell）；argv[0] 精确匹配白名单；拒 shell 元字符；超时强杀 | `agents/delegate.rs`、`tools/mod.rs` | delegate 14 测（含 `hi & echo PWNED_7f3a` 载荷） |
| 5 | Prompt/上下文注入 | 外部内容闭合上下文注入 `<system>` 指令、诱导工具调用 | section 安全标签（source_type/trust_level/instruction_allowed/can_trigger_tool）；不可信内容转义；渲染分区隔离（系统/记忆/用户/外部互不越权） | `memory/injector_format.rs` | injector_format 测试 + `security_regression::*_injection*` |
| 6 | 高风险工具滥用 | LLM 幻觉或注入诱导执行破坏性命令 | capability 风险分级；`PolicyEngine` 返回 `RequireApproval`；`ApprovalGate` 全同步挂起，用户四抉择（允许一次/本会话/拒绝/改参），120s 超时按拒绝 | `capability/mod.rs`、`policy/mod.rs`、`approval.rs`、`tools/mod.rs` | approval 9 测 + `security_regression::*_requires_approval` |
| 7 | SSRF / 云元数据 | 请求 `169.254.169.254`、metadata 域名、内网服务 | `NETWORK_DENY_EXACT` 精确拒绝；私网地址段（10/192.168/172.16-31/127/::1/fe80）→ 需人工确认；`host_of` 剥离端口正确判定 | `policy/mod.rs` | `security_regression::cloud_metadata_and_private_net_guarded` |
| 8 | 敏感信息泄露 | 输出含私钥 / AWS key / sk- token / GitHub token | `SECRET_PATTERNS` 命中即脱敏为 `[REDACTED:<kind>]`；超长连续 hex/base64 兜底 | `policy/mod.rs` | `security_regression::secrets_in_output_are_redacted` |
| 9 | 重放 / 幂等破坏 | 同一次工具调用重复执行、LLM 重试双写 | `llm_tool_calls` 台账唯一键（含 attempt 维度）；`Idempotency-Key` 头共享 `request_id`；成功终态不覆盖错误 | `db/schema.rs`、`llm/replay.rs`、`llm/caller.rs` | P1-2 回归 |
| 10 | 洪泛 / DoS | `/message` 高频调用、auth 探测 | 来源级限流（默认 30 次 / 10s，auth 探测也占槽位） | `api/security.rs::RateLimiter` | round 2 API 实测 |
| 11 | 审计缺失 | 恶意行为无痕 | `audit_trail` 环形缓冲（上限 10k）记录每次决策摘要 | `policy/mod.rs` | `security_regression::every_check_leaves_audit_trail` |
| 12 | Turn 悬挂 | 崩溃/重启后状态机停留在 running | `turn_state` 表持久化；启动扫描未终态按 `recover_policy` 恢复 | `turn/mod.rs`、`db/repositories/turn_state.rs` | turn 4 测 + turn_state 3 测 |

## 已知边界（明确不做 / 留待后续）

- **DNS 解析级 SSRF**：Phase 1 只做静态 host 判定，域名解析到内网地址的绕过留待 Phase 4 网络策略细化。
- **LLM 输出不可完全信任**：人工确认门降低但不消除幻觉风险；exec 类工具默认全量审批。
- **审计仅内存环形缓冲**：后续接线落库（DB 持久化）后再支持跨重启追查。
