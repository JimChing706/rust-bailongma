# -*- coding: utf-8 -*-
"""P2-3 收尾：更新 SUMMARY_RUST_RECOVERY.md、清理临时脚本、提交推送。"""
import io
import os
import subprocess

SUMMARY = "SUMMARY_RUST_RECOVERY.md"

# 1. 更新 SUMMARY_RUST_RECOVERY.md
with io.open(SUMMARY, "r", encoding="utf-8") as f:
    text = f.read()

# 路线行更新（把 R4/P2 标 ✅）
old_line = "- 路线：M1→M1.5→M2→M3→M4 ✅（P0 全部落地）→ P1-1 唤醒闭环 ✅ → **P1-2 幂等修复 ✅** → **R1 sandbox 真实化 ✅ → R2 工具能力层 ✅ → R3 意识闭环 ✅** → R4 文档补齐/封装打包/GitHub 推送 → P2 沙箱 trust 分层 / 参数 schema 校验 → P3 基于周报的缓存友好化 / 模型路由 / token 预算 → P4 自动迭代 + 语料蒸馏。"
new_line = "- 路线：M1→M1.5→M2→M3→M4 ✅（P0 全部落地）→ P1-1 唤醒闭环 ✅ → **P1-2 幂等修复 ✅** → **R1 sandbox 真实化 ✅ → R2 工具能力层 ✅ → R3 意识闭环 ✅ → R4 封装打包 ✅** → **P2 安全强化收官 ✅（P2-1 逃逸套件 / P2-2 trust 分层+schema 校验 / P2-3 打包）** → P3 基于周报的缓存友好化 / 模型路由 / token 预算 → P4 自动迭代 + 语料蒸馏。"
assert old_line in text, "路线行未找到"
text = text.replace(old_line, new_line)

# 追加 P2 收官章节
section = """

## 十四、P2 安全强化收官（P2-1 / P2-2 / P2-3）

- **P2-1 沙箱逃逸测试套件**（commit `7e782d3`，+432/-18）：`crates/sandbox/tests/escape_suite.rs` 端到端 17 条——真实子进程 + stdin/stdout JSON-RPC 打协议层/进程边界攻击面。路径逃逸 6（../ 相对穿越/绝对越界/前缀碰撞兄弟目录/混合分隔符/深嵌套归一化/junction 链接）、命令逃逸 5（shell 链/引号混淆/大小写/白名单外/环境泄露探针）、资源防护 4、协议层 3。
  - 实锤修复两处真实缺陷：①junction 链接逃逸——词法判定不解析链接，测试直接读出 root 外文件 → `resolve_in_root` 加 canonicalize 双保险；②大输出死锁——exec 期间不读管道，输出超 pipe 缓冲父子互等超时 → spawn 后立即读线程排空 stdout/stderr（对齐 delegate.rs 模式）。
- **P2-2 全工具参数 schema 校验 + 信任分层**（commit `4caa6a8` + `5f8158b`，+471/-5）：
  - `tools/validate.rs`：execute 分发前统一校验，未知参数/必填缺失/null/类型不符/enum 越界/数组元素类型错一律 fail-closed；上线即实锤 remind `now` 参数 schema 漏声明缺陷并补上。
  - TrustTier/CallerTrust：工具维度由能力声明自动推导（Trusted/Approval/Denied，未知工具恒拒）；来源维度 System 可放行需确认工具、User/Agent 仍需人工确认；check_tool_call 旧签名兼容委托，13 处既有调用零改动。
- **P2-3 收尾打包**：全量回归 **504 passed / 0 failed**（core 466 + api_e2e 8 + db_compat 1 + sandbox 12 + escape 17）；release 构建 5 个产物（bailongma / serve / bailongma-sandbox / chat / scan_agents）打包至 `_dist/bin` + `_dist/bailongma-rust-20260810.zip`（12.1MB）。
- ✅ 至此 Phase 2 整体收官（提交 `a1ef95a` 起，含 Modify 闭环 fail-closed + trace 可观测性）。
"""

with io.open(SUMMARY, "a", encoding="utf-8") as f:
    f.write(section)
print("SUMMARY updated")

# 2. 清理临时脚本
for tmp in ["_peek_docs.py", "_pack_p23.py"]:
    if os.path.exists(tmp):
        os.remove(tmp)
        print(f"removed {tmp}")

# 3. 提交推送
subprocess.run(["git", "add", "-A"], check=True)
r = subprocess.run(["git", "status", "--short"], capture_output=True, text=True, encoding="utf-8")
print("git status:", r.stdout.strip() or "(clean)")
subprocess.run(["git", "commit", "-m", "P2-3: Phase 2 收尾——release 打包 _dist/bin + zip，SUMMARY 标记 P2 收官"], check=True)
p = subprocess.run(["git", "push"], capture_output=True, text=True, encoding="utf-8")
print("push:", p.stdout.strip(), p.stderr.strip())
