# Bailongma Rust 版

BaiLongma（白龙马）的 Rust 原生实现——一个本地优先、带「意识循环」的智能体运行时。消息入站 → 归属/注入/记忆 → LLM 工具循环 → 回复落库/广播，全链路真实可用，不依赖任何外部服务即可本地运行；接上 LLM 配置后即具备完整对话与工具调用能力。

## 特性

- **意识循环**：`POST /message` 入站 → 落库拿真实 conversation_id → `run_user_turn`（归属/注入/渲染）→ `call_llm` 工具循环（流式、重试、超时、熔断）→ 回复落库 + `message_out` 事件广播。
- **真实工具层**（R2）：`get_timestamp` / `read_file` / `write_file` / `list_dir` / `exec_command` / `search_memory` / `send_message` / `collect_agents` / `remind` 共 9 个工具，schema 注册进 LLM 工具循环；`exec_command` 默认委托给 sandbox 子进程执行（JSON-RPC、超时、输出截断、参数脱敏）。
- **沙箱子进程**（R1）：`bailongma-sandbox` 从 M5 占位升级为真实执行器——JSON-RPC 协议、能力令牌校验、命令执行（超时/输出截断/参数脱敏）、文件读写、目录列举，全部带测试。
- **可观测性**（M1/M3/M4）：LLM 指标四表（`llm_calls` / `llm_tool_calls` / `llm_metrics_daily` / `llm_context_sections` + `llm_turns`），周报六指标与阈值信号、唤醒成本观测、上下文 section 命中统计。
- **可靠性**（M2/P1-2）：工具台账防重放（UNIQUE(request_id, round, attempt, tool_name)）、round_limit 终态、LLM 重试幂等键（Idempotency-Key）、`DbToolReplayGuard` 工具执行防重放。
- **唤醒闭环**（P1-1）：N 条到期提醒合并为 1 次唤醒 + 周窗口预算闸门，未到期不唤醒、拦截不消费。
- **安全**（M1.5）：delegate CLI 从字符串拼接改为参数数组直启，根除 shell 元字符注入面。
- **UI**：内置 Web 界面（`resources/index.html`），含 Agent 场景面板——实时消费 WS `/scene`（全量快照 + 增量 patch 渲染）、intent 三级视觉、choice 交互回传闭环。

## 工程结构

```
bailongma-rust/
├── crates/
│   ├── core/       # 核心运行时：runtime(意识循环)、memory、llm(调用/工具循环/指标)、
│   │               # tools(9 个真实工具)、api(routes/server/events/scene)、agents、db
│   ├── app/        # 可执行程序：serve(API 服务) / chat(CLI 验证) / scan_agents / sandbox 定位
│   └── sandbox/    # 工具沙箱子进程：JSON-RPC 命令执行/文件读写/目录列举
├── resources/
│   ├── index.html      # 主 Web 界面（含 Agent 场景面板）
│   └── brain-ui.html   # 备选界面
├── scripts/       # 启动/停止脚本（Windows + WSL）
└── .github/workflows/ci.yml
```

## 快速开始

```bash
# 构建（debug）
cargo build --workspace

# 运行 API 服务（默认 http://127.0.0.1:3721/）
cargo run -p bailongma-app --bin serve

# CLI 对话验证（真实 LLM 配置下可用）
cargo run -p bailongma-app --bin chat

# 全量测试
cargo test --workspace

# 发布构建
cargo build --release --workspace
```

配置通过 `BaiLongma` 用户目录（`resolve_user_dir()`）下的配置文件加载；LLM 未配置时服务正常启动，入站消息返回降级回复（不空转、不崩溃）。

## 测试与验证状态

- 全量回归：**397 通过 / 0 失败**（core 380 + app 1 + sandbox 8 + db_compat 8；R3 落地后）。
- 覆盖：意识循环接线、工具层 9 工具、沙箱 JSON-RPC、LLM 指标、幂等防重放、唤醒合并风暴（8→1）、注入面回归（元字符载荷原样传递）。
- 历史基线：M1 351 → M1.5 353 → M2 357 → M3 359 → M4 361 → P1-1 371 → P1-2 374 → UI 368(定向) → R3 397，逐轮递增全绿。

## 增强历史

- **R1 沙箱真实化**：见 `SUMMARY_RUST_RECOVERY.md` 附录「R1」。
- **R2 工具能力层**：`crates/core/src/tools/` 9 个真实工具 + schema 注册。
- **R3 意识闭环**：`crates/app/src/api_host.rs` inbound 闭包由占位 conversation_id=0 升级为真实链路。
- **M1–M4 / P1-1 / P1-2**：详见 `SUMMARY_RUST_RECOVERY.md`。

## 许可

MIT
