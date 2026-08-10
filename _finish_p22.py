# -*- coding: utf-8 -*-
"""P2-2 收尾：清理临时脚本 + 文档追加 + 提交推送。"""
import io
import os
import subprocess

# 1. 清理临时脚本
for f in ['_patch_p22.py', '_fix_tier_test.py', '_fix_remind_schema.py']:
    if os.path.exists(f):
        os.remove(f)
        print('removed', f)

# 2. TOOL_CAPABILITY_MODEL.md 追加 P2-2 章节
p = 'docs/TOOL_CAPABILITY_MODEL.md'
with io.open(p, 'r', encoding='utf-8') as fh:
    doc = fh.read()
if '信任分层' not in doc:
    append = '''

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
'''
    with io.open(p, 'w', encoding='utf-8', newline='') as fh:
        fh.write(doc + append)
    print('doc updated:', p)
else:
    print('doc already has P2-2, skip')

# 3. git 提交推送
r = subprocess.run(['git', 'add', '-A'], capture_output=True, text=True, encoding='utf-8')
print(r.stdout.strip(), r.stderr.strip())
r = subprocess.run(['git', 'status', '--short'], capture_output=True, text=True, encoding='utf-8')
print('--- status ---')
print(r.stdout)
r = subprocess.run(
    ['git', 'commit', '-m', 'P2-2: 全工具参数 schema 校验 + 信任分层（TrustTier/CallerTrust）',
     '-m', 'execute 分发前统一校验（未知参数/必填/类型/enum/数组 items 一律拒绝）；'
     'capability 新增 trust_tier 推导与 CallerTrust 来源分层，policy 新增 '
     'check_tool_call_with_caller（System 放行需确认工具，User/Agent 仍需人工确认），'
     '旧 check_tool_call 保持兼容委托；remind schema 补 now 参数声明（校验器实锤的 '
     'schema 与实现不一致）。新增测试 11 条。'],
    capture_output=True, text=True, encoding='utf-8')
print(r.stdout.strip(), r.stderr.strip())
r = subprocess.run(['git', 'push'], capture_output=True, text=True, encoding='utf-8')
print('push:', r.stdout.strip(), r.stderr.strip())
r = subprocess.run(['git', 'log', '--oneline', '-3'], capture_output=True, text=True, encoding='utf-8')
print(r.stdout)
