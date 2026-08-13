# -*- coding: utf-8 -*-
"""生成一个模拟真实数据的临时 jarvis.db，用于验证 revisit_m4_metrics.py。"""
import sqlite3
import sys
import os

db_path = sys.argv[1]
if os.path.exists(db_path):
    os.remove(db_path)
conn = sqlite3.connect(db_path)
c = conn.cursor()

# 最小 schema：只建观测层相关表（模拟已迁移的真实库）
c.executescript("""
CREATE TABLE llm_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  request_id TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  stage TEXT NOT NULL DEFAULT '',
  started_at TEXT NOT NULL,
  ttft_ms INTEGER, duration_ms INTEGER,
  total_tokens INTEGER, cached_tokens INTEGER,
  usage_raw TEXT NOT NULL DEFAULT '',
  finish_reason TEXT NOT NULL DEFAULT '',
  error_stage TEXT NOT NULL DEFAULT '', error_class TEXT NOT NULL DEFAULT '',
  http_status INTEGER, had_content INTEGER NOT NULL DEFAULT 0,
  retryable INTEGER NOT NULL DEFAULT 0, attempt INTEGER NOT NULL DEFAULT 1,
  last_error TEXT NOT NULL DEFAULT '', fallback_used INTEGER NOT NULL DEFAULT 0,
  context_bytes INTEGER, created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE UNIQUE INDEX idx_llm_calls_request_id ON llm_calls(request_id);

CREATE TABLE llm_tool_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  request_id TEXT NOT NULL, round INTEGER NOT NULL, attempt INTEGER NOT NULL DEFAULT 1,
  tool_name TEXT NOT NULL, args_json TEXT NOT NULL DEFAULT '{}',
  result_json TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'ok',
  duration_ms INTEGER NOT NULL DEFAULT 0, delegated_from TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(request_id, round, attempt, tool_name)
);

CREATE TABLE llm_metrics_daily (
  day TEXT PRIMARY KEY,
  total_calls INTEGER NOT NULL DEFAULT 0, error_count INTEGER NOT NULL DEFAULT 0,
  retry_count INTEGER NOT NULL DEFAULT 0, fallback_count INTEGER NOT NULL DEFAULT 0,
  aborted_count INTEGER NOT NULL DEFAULT 0,
  total_tokens INTEGER NOT NULL DEFAULT 0, cached_tokens INTEGER NOT NULL DEFAULT 0,
  ttft_sum_ms INTEGER NOT NULL DEFAULT 0, ttft_count INTEGER NOT NULL DEFAULT 0,
  duration_sum_ms INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE llm_context_sections (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  request_id TEXT NOT NULL, section TEXT NOT NULL, bytes INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(request_id, section)
);

CREATE TABLE llm_turns (
  turn_id TEXT PRIMARY KEY,
  started_at TEXT NOT NULL, duration_ms INTEGER,
  attribution TEXT NOT NULL DEFAULT '', is_tick INTEGER NOT NULL DEFAULT 0,
  sections_hit INTEGER, context_bytes INTEGER, calls INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE conversations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  role TEXT NOT NULL, from_id TEXT NOT NULL, to_id TEXT,
  content TEXT NOT NULL, channel TEXT NOT NULL DEFAULT '',
  external_party_id TEXT DEFAULT '', focus_absorbed INTEGER NOT NULL DEFAULT 0,
  focus_topic TEXT DEFAULT '', open_question INTEGER NOT NULL DEFAULT 0,
  thread_id TEXT DEFAULT '', label TEXT DEFAULT '',
  delivery_status TEXT NOT NULL DEFAULT '', timestamp TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
""")

# 模拟 3 天真实数据
rows = [
    ("llm-1", "deepseek", "deepseek-v4-pro", "run_turn", "2026-08-11T09:00:00+08:00", "done", 800, 4200, 512, 128, 0, 1, 1),
    ("llm-2", "deepseek", "deepseek-v4-pro", "run_turn", "2026-08-12T09:00:00+08:00", "done", 700, 3800, 480, 96, 0, 1, 2),
    ("llm-3", "deepseek", "deepseek-v4-pro", "wakeup", "2026-08-12T12:00:00+08:00", "done", 500, 2500, 300, 60, 0, 1, 3),
    ("llm-4", "deepseek", "deepseek-v4-pro", "wakeup", "2026-08-13T08:00:00+08:00", "done", 600, 2800, 320, 64, 0, 1, 4),
]
for r in rows:
    c.execute("INSERT INTO llm_calls (request_id, provider, model, stage, started_at, finish_reason, ttft_ms, duration_ms, total_tokens, cached_tokens, had_content, attempt, context_bytes) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
              (r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8], r[9], r[10], r[11], 1024))

for t in [
    ("llm-1", "2026-08-11T09:00:00+08:00", 4300, "created", 0, 5, 1024, 1),
    ("llm-2", "2026-08-12T09:00:00+08:00", 3900, "created", 0, 5, 1024, 1),
    ("llm-4", "2026-08-13T08:00:00+08:00", 2900, "continued", 1, 3, 768, 1),
]:
    c.execute("INSERT INTO llm_turns (turn_id, started_at, duration_ms, attribution, is_tick, sections_hit, context_bytes, calls) VALUES (?,?,?,?,?,?,?,?)", t)

c.execute("INSERT INTO llm_tool_calls (request_id, round, attempt, tool_name, args_json, result_json, status, duration_ms) VALUES ('llm-2', 0, 1, 'web_search', '{}', 'ok', 'ok', 900)")
c.execute("INSERT INTO llm_tool_calls (request_id, round, attempt, tool_name, args_json, result_json, status, duration_ms) VALUES ('llm-3', 0, 1, 'recall_memory', '{}', 'ok', 'ok', 120)")

for rid, sec, b in [("llm-1", "memories", 512), ("llm-1", "directions", 256), ("llm-1", "tools", 256), ("llm-2", "memories", 512), ("llm-2", "directions", 512)]:
    c.execute("INSERT INTO llm_context_sections (request_id, section, bytes) VALUES (?,?,?)", (rid, sec, b))

c.execute("INSERT INTO llm_metrics_daily (day, total_calls, error_count, retry_count, fallback_count, aborted_count, total_tokens, cached_tokens, ttft_sum_ms, ttft_count, duration_sum_ms) VALUES ('2026-08-11', 1, 0, 0, 0, 0, 512, 128, 800, 1, 4200)")
c.execute("INSERT INTO llm_metrics_daily (day, total_calls, error_count, retry_count, fallback_count, aborted_count, total_tokens, cached_tokens, ttft_sum_ms, ttft_count, duration_sum_ms) VALUES ('2026-08-12', 2, 0, 0, 0, 0, 780, 156, 1200, 2, 6300)")
c.execute("INSERT INTO llm_metrics_daily (day, total_calls, error_count, retry_count, fallback_count, aborted_count, total_tokens, cached_tokens, ttft_sum_ms, ttft_count, duration_sum_ms) VALUES ('2026-08-13', 1, 0, 0, 0, 0, 320, 64, 600, 1, 2800)")

c.execute("INSERT INTO conversations (role, from_id, content, timestamp) VALUES ('user', 'ID:000001', '你好', '2026-08-11T09:00:00+08:00')")
c.execute("INSERT INTO conversations (role, from_id, content, timestamp) VALUES ('user', 'ID:000001', '查一下', '2026-08-12T09:00:00+08:00')")
c.execute("INSERT INTO conversations (role, from_id, content, timestamp) VALUES ('user', 'ID:000001', '继续', '2026-08-13T09:00:00+08:00')")

conn.commit()
conn.close()
print("seeded:", db_path)
