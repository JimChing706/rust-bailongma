# -*- coding: utf-8 -*-
"""生成一个只含观测表、无任何数据的空库，用于验证回访脚本的空库 FAIL 分支。"""
import os
import sqlite3
import sys

db_path = sys.argv[1]
if os.path.exists(db_path):
    os.remove(db_path)
conn = sqlite3.connect(db_path)
conn.executescript("""
CREATE TABLE llm_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT NOT NULL,
  provider TEXT NOT NULL, model TEXT NOT NULL, stage TEXT NOT NULL DEFAULT '',
  started_at TEXT NOT NULL, ttft_ms INTEGER, duration_ms INTEGER,
  total_tokens INTEGER, cached_tokens INTEGER,
  usage_raw TEXT NOT NULL DEFAULT '', finish_reason TEXT NOT NULL DEFAULT '',
  error_stage TEXT NOT NULL DEFAULT '', error_class TEXT NOT NULL DEFAULT '',
  http_status INTEGER, had_content INTEGER NOT NULL DEFAULT 0,
  retryable INTEGER NOT NULL DEFAULT 0, attempt INTEGER NOT NULL DEFAULT 1,
  last_error TEXT NOT NULL DEFAULT '', fallback_used INTEGER NOT NULL DEFAULT 0,
  context_bytes INTEGER, created_at TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE llm_turns (
  turn_id TEXT PRIMARY KEY, started_at TEXT NOT NULL, duration_ms INTEGER,
  attribution TEXT NOT NULL DEFAULT '', is_tick INTEGER NOT NULL DEFAULT 0,
  sections_hit INTEGER, context_bytes INTEGER, calls INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE llm_tool_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT NOT NULL,
  round INTEGER NOT NULL, attempt INTEGER NOT NULL DEFAULT 1,
  tool_name TEXT NOT NULL, args_json TEXT NOT NULL DEFAULT '{}',
  result_json TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'ok',
  duration_ms INTEGER NOT NULL DEFAULT 0, delegated_from TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE llm_context_sections (
  id INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT NOT NULL,
  section TEXT NOT NULL, bytes INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE llm_metrics_daily (
  day TEXT PRIMARY KEY, total_calls INTEGER NOT NULL DEFAULT 0,
  error_count INTEGER NOT NULL DEFAULT 0, retry_count INTEGER NOT NULL DEFAULT 0,
  fallback_count INTEGER NOT NULL DEFAULT 0, aborted_count INTEGER NOT NULL DEFAULT 0,
  total_tokens INTEGER NOT NULL DEFAULT 0, cached_tokens INTEGER NOT NULL DEFAULT 0,
  ttft_sum_ms INTEGER NOT NULL DEFAULT 0, ttft_count INTEGER NOT NULL DEFAULT 0,
  duration_sum_ms INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE conversations (
  id INTEGER PRIMARY KEY AUTOINCREMENT, role TEXT NOT NULL,
  from_id TEXT NOT NULL, to_id TEXT, content TEXT NOT NULL,
  channel TEXT NOT NULL DEFAULT '', external_party_id TEXT DEFAULT '',
  focus_absorbed INTEGER NOT NULL DEFAULT 0, focus_topic TEXT DEFAULT '',
  open_question INTEGER NOT NULL DEFAULT 0, thread_id TEXT DEFAULT '',
  label TEXT DEFAULT '', delivery_status TEXT NOT NULL DEFAULT '',
  timestamp TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (datetime('now')));
""")
conn.commit()
conn.close()
print("empty db ready:", db_path)
