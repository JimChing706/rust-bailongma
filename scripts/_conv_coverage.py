# -*- coding: utf-8 -*-
"""半程报告：conversations 侧覆盖数据（只读，针对真实 jarvis.db）。
观测表(llm_turns/llm_calls)缺失时，覆盖率分子为 0，输出 conversations 全貌供判断基线。"""
import os
import sqlite3

DB = r"E:\BailongmaData\data\jarvis.db"

con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
con.row_factory = sqlite3.Row

# 1) 观测表是否存在（覆盖率分子侧）
obs = {t: con.execute("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?", (t,)).fetchone()[0] > 0
       for t in ("llm_calls", "llm_turns", "llm_tool_calls", "llm_context_sections", "llm_metrics_daily")}
print("观测表存在:", {k: ("有" if v else "缺失") for k, v in obs.items()})

# 2) conversations 全貌
cols = [r[1] for r in con.execute("PRAGMA table_info(conversations)")]
print("conversations 列:", cols)

total = con.execute("SELECT COUNT(*) FROM conversations").fetchone()[0]
print("总行数:", total)

# 角色分布
try:
    roles = dict(con.execute("SELECT role, COUNT(*) FROM conversations GROUP BY 1"))
    print("角色分布:", roles)
except Exception as e:
    print("角色分布: 无 role 列 -", e)

# 时间列探测（取含 time/date/created 的列）
tcol = next((c for c in cols if any(k in c.lower() for k in ("time", "date", "created"))), None)
print("时间列:", tcol)
if tcol:
    rows = con.execute(f"SELECT substr({tcol},1,10) d, COUNT(*) c FROM conversations GROUP BY 1 ORDER BY 1").fetchall()
    print("按日分布(近14天):")
    for r in rows[-14:]:
        print(f"  {r[0]} | {r[1]}")

# 渠道列探测
ccol = next((c for c in cols if "channel" in c.lower()), None)
print("渠道列:", ccol)
if ccol:
    print("渠道分布:", dict(con.execute(f"SELECT {ccol}, COUNT(*) FROM conversations GROUP BY 1")))

# 会话数（若存在 conversation_id）
idcol = next((c for c in cols if c.lower() in ("conversation_id", "session_id", "cid")), None)
print("会话ID列:", idcol)
if idcol:
    n = con.execute(f"SELECT COUNT(DISTINCT {idcol}) FROM conversations").fetchone()[0]
    print("去重会话数:", n)
    print("平均每会话轮数:", round(total / n, 1) if n else 0)

con.close()
