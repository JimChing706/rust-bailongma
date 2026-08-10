# -*- coding: utf-8 -*-
"""P3-1 收尾：更新 SUMMARY_RUST_RECOVERY.md、清理临时脚本、提交推送。"""
import io, os, subprocess, sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

SUMMARY = "SUMMARY_RUST_RECOVERY.md"
ADD = """
## P3-1 缓存友好化（2026-08-10）

- `injector_format.rs`：`format_context_block` 双区渲染——稳定段（self-evolution/self-perception/constraints/active-policies/person/user-profile/task/thread/threads-background/task-knowledge）前置，变动段（self-snapshot/temporal/memories/directions/extra）后置。prompt 前缀多轮稳定 → provider prompt cache 命中率提升。新增 `NODE_CONTEXT_ORDER`（Node 对齐基线，对照用）与 `CACHE_FRIENDLY_ORDER` 常量。
- `metrics.rs`：新增 `relocate_sections(history)` 数据驱动排序——按「静态稳定级 + 历史字节波动率」重排 section，波动率低者前置；无历史时退化为静态分级；NaN 波动安全处理；未知 section 置尾。
- 新增 4 条测试：顺序集合一致性、渲染顺序稳定段前置、历史波动率排序、NaN 安全。
- 全量回归 469 passed / 0 failed（core 470 running：469 passed + 1 ignored）。
"""

with io.open(SUMMARY, encoding="utf-8") as f:
    content = f.read()
content = content.rstrip() + "\n" + ADD
with io.open(SUMMARY, "w", encoding="utf-8", newline="\n") as f:
    f.write(content)
print("summary updated")

# 清理临时脚本
for name in ["_p3_patch1.py", "_p3_patch2.py", "_p3_fix1.py", "_p3_fix2.py", "_p3_fix3.py", "_p3_fix4.py"]:
    if os.path.exists(name):
        os.remove(name)
        print(f"removed {name}")

# 提交推送
cmds = [
    "git add -A",
    'git commit -m "feat: P3-1 缓存友好化（稳定段前置渲染 + relocate_sections 数据驱动排序）"',
    "git push 2>&1",
    "git status --short",
]
for c in cmds:
    r = subprocess.run(c, shell=True, capture_output=True, text=True, encoding="utf-8", errors="replace")
    out = (r.stdout or "").strip()
    err = (r.stderr or "").strip()
    if out:
        print(out)
    if err and "warning" not in err.lower():
        print("ERR:", err)
    print(f"exit={r.returncode}")
