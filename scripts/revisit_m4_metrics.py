# -*- coding: utf-8 -*-
"""M4 接线日回访：真实调用 SQL 检查（DELIBERATION_FINAL_PLAN.md §验收口径）。

对任意 jarvis.db 执行 M4 验收查询，输出回访报告：
  1. llm_calls 明细（行数 / 日期分布 / stage 分布 / finish_reason 分布）
  2. llm_turns turn 级记录（含 is_tick 占比）
  3. llm_tool_calls 工具台账
  4. llm_context_sections 上下文 section 明细（与 llm_calls JOIN 可用性）
  5. llm_metrics_daily 日聚合
  6. 与 conversations 对照：真实对话轮 vs 观测 turn 覆盖率

用法:
  python scripts/revisit_m4_metrics.py [jarvis.db 路径]
  （缺省参数时自动按 resolve_user_dir 规则找库）

退出码: 0 = 回访通过（观测层有真实数据且 JOIN 可用）
         2 = 回访未通过（全空 / JOIN 断裂 / 文件不存在）
"""
import json
import os
import sqlite3
import sys


def resolve_db_path() -> str:
    """与 crates/core/src/config.rs resolve_user_dir 对齐的路径解析。"""
    user_dir = os.environ.get("BAILONGMA_USER_DIR", "").strip()
    if user_dir:
        return os.path.join(user_dir, "data", "jarvis.db")
    portable = os.environ.get("BAILONGMA_PORTABLE_DIR", "").strip()
    if portable:
        return os.path.join(portable, "data", "jarvis.db")
    # 平台用户目录（Windows: %APPDATA%/Bailongma）
    base = os.environ.get("APPDATA") or os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    return os.path.join(base, "Bailongma", "data", "jarvis.db")


def q(conn: sqlite3.Connection, sql: str, *args) -> list:
    return conn.execute(sql, args).fetchall()


def table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = q(conn, "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?", name)
    return row[0][0] > 0


def main() -> int:
    db_path = sys.argv[1] if len(sys.argv) > 1 else resolve_db_path()
    if not os.path.isfile(db_path):
        print(f"[FAIL] 数据库不存在: {db_path}")
        return 2

    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    print(f"数据库: {db_path}\n")

    missing = [t for t in ("llm_calls", "llm_turns", "llm_tool_calls",
                           "llm_context_sections", "llm_metrics_daily")
               if not table_exists(conn, t)]
    if missing:
        print(f"[FAIL] 观测表缺失: {missing} —— LLM 观测层未挂载（M1 未接线）")
        return 2

    # 1) llm_calls 明细
    n_calls = q(conn, "SELECT COUNT(*) FROM llm_calls")[0][0]
    print(f"1) llm_calls 明细: {n_calls} 行")
    if n_calls == 0:
        print("   [FAIL] 空表 —— 无任何真实 LLM 调用落库")
        return 2
    for row in q(conn, "SELECT started_at, stage, finish_reason, COUNT(*) c "
                       "FROM llm_calls GROUP BY 1,2,3 ORDER BY 1 LIMIT 10"):
        print(f"   {row[0]} | stage={row[1] or '(空)'} | {row[2]} | {row[3]} 次")
    print("   stage 分布:", dict(q(conn, "SELECT stage, COUNT(*) FROM llm_calls GROUP BY 1")))
    print("   finish_reason:", dict(q(conn, "SELECT finish_reason, COUNT(*) FROM llm_calls GROUP BY 1")))

    # 2) llm_turns
    n_turns = q(conn, "SELECT COUNT(*) FROM llm_turns")[0][0]
    n_tick = q(conn, "SELECT COUNT(*) FROM llm_turns WHERE is_tick=1")[0][0]
    print(f"2) llm_turns: {n_turns} 行（TICK {n_tick}）")

    # 3) llm_tool_calls
    n_tools = q(conn, "SELECT COUNT(*) FROM llm_tool_calls")[0][0]
    print(f"3) llm_tool_calls: {n_tools} 行")
    if n_tools:
        print("   ", dict(q(conn, "SELECT tool_name, COUNT(*) FROM llm_tool_calls GROUP BY 1")))

    # 4) llm_context_sections + JOIN
    n_sections = q(conn, "SELECT COUNT(*) FROM llm_context_sections")[0][0]
    n_join = q(conn, "SELECT COUNT(*) FROM llm_context_sections s "
                     "JOIN llm_calls c ON c.request_id = s.request_id OR c.request_id LIKE s.request_id || '#%'")[0][0]
    print(f"4) llm_context_sections: {n_sections} 行；与 llm_calls JOIN 命中 {n_join}")
    if n_sections and n_join == 0:
        print("   [FAIL] section 明细与 llm_calls 无法 JOIN —— M3 关联断裂")
        return 2

    # 5) llm_metrics_daily 日聚合
    daily = q(conn, "SELECT day, total_calls, error_count, retry_count, total_tokens "
                    "FROM llm_metrics_daily ORDER BY day")
    print(f"5) llm_metrics_daily: {len(daily)} 天")
    for row in daily[-7:]:
        print(f"   {row[0]} | calls={row[1]} err={row[2]} retry={row[3]} tokens={row[4]}")

    # 6) 对照 conversations：真实对话轮覆盖率
    n_conv = q(conn, "SELECT COUNT(*) FROM conversations WHERE role='user'")[0][0]
    cov = (n_turns / n_conv * 100.0) if n_conv else 0.0
    print(f"6) conversations.user 轮: {n_conv}；llm_turns 覆盖率 {cov:.1f}%")

    # 唤醒成本观测（M4 信号；stage='wakeup' 在 llm_calls）
    n_wakeup = q(conn, "SELECT COUNT(*) FROM llm_calls WHERE stage='wakeup'")[0][0]
    print(f"   唤醒轮观测: {n_wakeup} 次")

    print("\n[PASS] M4 接线日回访通过：观测层真实数据已落库，JOIN 可用。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
