//! 数据库 schema：幂等迁移，与 Node 版 `src/db/schema.js` 的最终状态逐表对齐。
//!
//! 兼容性承诺（RUST-ROADMAP.md §4.2）：
//! - 全部表结构与列名与现有 `jarvis.db` 完全一致，用户数据**零迁移**；
//! - 新库直接建出全量列（Node 版由 CREATE + 多轮 ALTER 拼出的最终形态）；
//! - 老库通过 `PRAGMA table_info` 检查缺失列后仅补列（幂等、不碰已有数据）；
//! - `memories_fts` 保持 FTS5 trigram + 外部内容表 + 3 个触发器。
//! - LLM 指标三表（M1 观测层）为 Rust 新增（Node 版无对应表），全部走
//!   `CREATE TABLE IF NOT EXISTS` + 唯一索引，老库零数据改动。

use rusqlite::Connection;
use tracing::info;

use crate::error::Result;

/// 业务表清单（不含 sqlite_sequence 与 memories_fts 的 4 张内部影子表）。
/// 与 Node 版 schema.js 最终状态一致：24 张业务表 + 4 张 FTS 内部表 + sqlite_sequence = 29；
/// M1 新增 3 张 LLM 指标表 → 27 张业务表 + 4 张 FTS 内部表 + sqlite_sequence = 32；
/// P1 新增 turn_state（显式 Turn 状态机）→ 30 张业务表；事项账本 matters → 31 张。
pub const BUSINESS_TABLES: &[&str] = &[
    "conversations",
    "memories",
    "memories_fts",
    "config",
    "entities",
    "user_profiles",
    "action_logs",
    "brain_ui_events",
    "brain_ui_state",
    "reminders",
    "prefetch_tasks",
    "prefetch_cache",
    "ui_signals",
    "media_history",
    "music_library",
    "known_agents",
    "user_identities",
    "focus_stack",
    "threads",
    "commitments",
    "thread_state",
    "wechat_clawbot_tokens",
    "recall_audit",
    "extract_audit",
    "llm_calls",
    "llm_tool_calls",
    "llm_metrics_daily",
    "llm_context_sections",
    "llm_turns",
    "turn_state",
    "matters",
];

/// 检查某表是否已存在指定列。
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 幂等补列：列缺失时执行 `ALTER TABLE {table} ADD COLUMN {ddl}`。
fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    if has_column(conn, table, column)? {
        return Ok(());
    }
    info!("[db schema] 补列: {table}.{column}");
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"))?;
    Ok(())
}

/// 幂等建索引。
fn ensure_index(conn: &Connection, sql: &str) -> Result<()> {
    conn.execute_batch(sql)?;
    Ok(())
}

/// 执行完整幂等迁移（对齐 Node 版 `initializeSchema`）。
///
/// 打开任意数据库（新库或老库）后调用。老库只会补缺失列/索引，不改动已有数据；
/// 唯一的数据改写是 `migration_canonical_user_v1` 一次性历史迁移（与 Node 版相同，
/// 由 config 表 flag 守卫，只执行一次）。
pub fn initialize(conn: &Connection) -> Result<()> {
    // ── 基础表（CREATE 时即含全部迁移后列） ──
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS conversations (
          id          INTEGER PRIMARY KEY AUTOINCREMENT,
          role        TEXT    NOT NULL,
          from_id     TEXT    NOT NULL,
          to_id       TEXT,
          content     TEXT    NOT NULL,
          channel     TEXT    NOT NULL DEFAULT '',
          external_party_id TEXT DEFAULT '',
          focus_absorbed INTEGER NOT NULL DEFAULT 0,
          focus_topic TEXT DEFAULT '',
          open_question INTEGER NOT NULL DEFAULT 0,
          thread_id   TEXT DEFAULT '',
          label       TEXT DEFAULT '',
          delivery_status TEXT NOT NULL DEFAULT '',
          timestamp   TEXT    NOT NULL,
          created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS memories (
          id          INTEGER PRIMARY KEY AUTOINCREMENT,
          event_type  TEXT    NOT NULL,
          content     TEXT    NOT NULL,
          detail      TEXT    NOT NULL,
          title       TEXT    DEFAULT '',
          mem_id      TEXT,
          entities    TEXT    DEFAULT '[]',
          concepts    TEXT    DEFAULT '[]',
          tags        TEXT    DEFAULT '[]',
          links       TEXT    DEFAULT '[]',
          salience    INTEGER DEFAULT 3,
          source_ref  TEXT,
          timestamp   TEXT    NOT NULL,
          parent_id   INTEGER REFERENCES memories(id),
          embedding   BLOB,
          created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
          visibility  INTEGER NOT NULL DEFAULT 1,
          hidden_at   TEXT,
          merged_into TEXT,
          embedding_dim INTEGER,
          embedding_model TEXT
        );

        CREATE TABLE IF NOT EXISTS config (
          key         TEXT    PRIMARY KEY,
          value       TEXT    NOT NULL,
          updated_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS entities (
          id          TEXT    PRIMARY KEY,
          label       TEXT,
          last_seen   TEXT    NOT NULL,
          created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS user_profiles (
          user_id                  TEXT PRIMARY KEY,
          summary                  TEXT NOT NULL DEFAULT '',
          roles_json               TEXT NOT NULL DEFAULT '[]',
          domains_json             TEXT NOT NULL DEFAULT '[]',
          expertise_json           TEXT NOT NULL DEFAULT '[]',
          projects_json            TEXT NOT NULL DEFAULT '[]',
          preferences_json         TEXT NOT NULL DEFAULT '[]',
          communication_style_json TEXT NOT NULL DEFAULT '[]',
          evidence_json            TEXT NOT NULL DEFAULT '[]',
          confidence               REAL NOT NULL DEFAULT 0,
          updated_at               TEXT NOT NULL
        );
        "#,
    )?;

    // ── 老库兜底补列（CREATE IF NOT EXISTS 对已存在的表不生效） ──
    ensure_column(conn, "conversations", "channel", "channel TEXT DEFAULT ''")?;
    ensure_column(
        conn,
        "conversations",
        "external_party_id",
        "external_party_id TEXT DEFAULT ''",
    )?;
    ensure_column(
        conn,
        "conversations",
        "delivery_status",
        "delivery_status TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(
        conn,
        "conversations",
        "focus_absorbed",
        "focus_absorbed INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "conversations",
        "focus_topic",
        "focus_topic TEXT DEFAULT ''",
    )?;
    ensure_column(
        conn,
        "conversations",
        "open_question",
        "open_question INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "conversations",
        "thread_id",
        "thread_id TEXT DEFAULT ''",
    )?;
    // 老库兜底：to_id（新库 CREATE 自带；旧库缺列时 migrate_canonical_user 会报 no such column）
    ensure_column(conn, "conversations", "to_id", "to_id TEXT")?;
    // M1 语料积累（评审修订 #9）：预留 label 列，蒸馏 P4 启动前 conversations 自然积累
    ensure_column(conn, "conversations", "label", "label TEXT DEFAULT ''")?;

    ensure_column(conn, "memories", "title", "title TEXT DEFAULT ''")?;
    ensure_column(conn, "memories", "mem_id", "mem_id TEXT")?;
    ensure_column(conn, "memories", "links", "links TEXT DEFAULT '[]'")?;
    ensure_column(conn, "memories", "salience", "salience INTEGER DEFAULT 3")?;
    ensure_column(
        conn,
        "memories",
        "parent_id",
        "parent_id INTEGER REFERENCES memories(id)",
    )?;
    ensure_column(
        conn,
        "memories",
        "visibility",
        "visibility INTEGER NOT NULL DEFAULT 1",
    )?;
    ensure_column(conn, "memories", "hidden_at", "hidden_at TEXT")?;
    ensure_column(conn, "memories", "merged_into", "merged_into TEXT")?;
    ensure_column(conn, "memories", "embedding", "embedding BLOB")?;
    ensure_column(conn, "memories", "embedding_dim", "embedding_dim INTEGER")?;
    ensure_column(conn, "memories", "embedding_model", "embedding_model TEXT")?;

    // ── conversations 索引 ──
    ensure_index(
        conn,
        r#"
        CREATE INDEX IF NOT EXISTS idx_conv_timestamp ON conversations(timestamp);
        CREATE INDEX IF NOT EXISTS idx_conv_from_id   ON conversations(from_id);
        CREATE INDEX IF NOT EXISTS idx_conv_delivery_status ON conversations(delivery_status);
        CREATE INDEX IF NOT EXISTS idx_conv_focus_absorbed ON conversations(focus_absorbed);
        CREATE INDEX IF NOT EXISTS idx_conv_thread_id ON conversations(thread_id);
        "#,
    )?;

    // ── memories 索引 ──
    ensure_index(
        conn,
        r#"
        CREATE INDEX IF NOT EXISTS idx_memories_timestamp  ON memories(timestamp);
        CREATE INDEX IF NOT EXISTS idx_memories_event_type ON memories(event_type);
        CREATE INDEX IF NOT EXISTS idx_memories_parent_id  ON memories(parent_id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_mem_id ON memories(mem_id) WHERE mem_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_memories_visibility ON memories(visibility);
        "#,
    )?;

    // ── memories_fts：FTS5 trigram 外部内容表 + 3 触发器 ──
    conn.execute_batch(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
          title, mem_id, content, detail, entities, concepts, tags,
          content='memories', content_rowid='id',
          tokenize='trigram'
        );

        CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
          INSERT INTO memories_fts(rowid, title, mem_id, content, detail, entities, concepts, tags)
          VALUES (new.id, new.title, new.mem_id, new.content, new.detail, new.entities, new.concepts, new.tags);
        END;

        CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
          INSERT INTO memories_fts(memories_fts, rowid, title, mem_id, content, detail, entities, concepts, tags)
          VALUES ('delete', old.id, old.title, old.mem_id, old.content, old.detail, old.entities, old.concepts, old.tags);
        END;

        CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
          INSERT INTO memories_fts(memories_fts, rowid, title, mem_id, content, detail, entities, concepts, tags)
          VALUES ('delete', old.id, old.title, old.mem_id, old.content, old.detail, old.entities, old.concepts, old.tags);
          INSERT INTO memories_fts(rowid, title, mem_id, content, detail, entities, concepts, tags)
          VALUES (new.id, new.title, new.mem_id, new.content, new.detail, new.entities, new.concepts, new.tags);
        END;
        "#,
    )?;

    // ── 观测/工具/社交等其余表 ──
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS action_logs (
          id        INTEGER PRIMARY KEY AUTOINCREMENT,
          timestamp TEXT    NOT NULL,
          tool      TEXT    NOT NULL,
          summary   TEXT    NOT NULL,
          detail    TEXT    NOT NULL DEFAULT '',
          status    TEXT NOT NULL DEFAULT 'ok',
          risk      TEXT NOT NULL DEFAULT 'medium',
          args_json TEXT NOT NULL DEFAULT '{}',
          result_preview TEXT NOT NULL DEFAULT '',
          error     TEXT NOT NULL DEFAULT '',
          duration_ms INTEGER NOT NULL DEFAULT 0,
          source    TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS brain_ui_events (
          id           INTEGER PRIMARY KEY AUTOINCREMENT,
          timestamp    TEXT    NOT NULL,
          path         TEXT    NOT NULL DEFAULT 'l2',
          event_type   TEXT    NOT NULL,
          payload_json TEXT    NOT NULL DEFAULT '{}'
        );

        CREATE TABLE IF NOT EXISTS brain_ui_state (
          key        TEXT PRIMARY KEY,
          value      TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS reminders (
          id                INTEGER PRIMARY KEY AUTOINCREMENT,
          user_id           TEXT    NOT NULL,
          due_at            TEXT    NOT NULL,
          task              TEXT    NOT NULL,
          system_message    TEXT    NOT NULL,
          status            TEXT    NOT NULL DEFAULT 'pending',
          created_at        TEXT    NOT NULL DEFAULT (datetime('now')),
          fired_at          TEXT,
          cancelled_at      TEXT,
          source            TEXT    DEFAULT '',
          recurrence_type   TEXT,
          recurrence_config TEXT
        );

        CREATE TABLE IF NOT EXISTS prefetch_tasks (
          id          INTEGER PRIMARY KEY AUTOINCREMENT,
          source      TEXT    NOT NULL UNIQUE,
          label       TEXT    NOT NULL,
          url         TEXT    NOT NULL,
          ttl_minutes INTEGER NOT NULL DEFAULT 60,
          tags        TEXT    DEFAULT '[]',
          enabled     INTEGER NOT NULL DEFAULT 1,
          created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
          updated_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS prefetch_cache (
          id         INTEGER PRIMARY KEY AUTOINCREMENT,
          source     TEXT    NOT NULL,
          content    TEXT    NOT NULL,
          fetched_at TEXT    NOT NULL,
          expires_at TEXT    NOT NULL,
          tags       TEXT    DEFAULT '[]',
          created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS ui_signals (
          id         INTEGER PRIMARY KEY AUTOINCREMENT,
          type       TEXT    NOT NULL,
          target     TEXT,
          payload    TEXT    NOT NULL DEFAULT '{}',
          ts         INTEGER NOT NULL,
          consumed   INTEGER NOT NULL DEFAULT 0,
          created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS media_history (
          id         INTEGER PRIMARY KEY AUTOINCREMENT,
          kind       TEXT    NOT NULL,
          url        TEXT    NOT NULL,
          title      TEXT    NOT NULL DEFAULT '',
          video_id   TEXT,
          platform   TEXT,
          played_at  TEXT    NOT NULL DEFAULT (datetime('now')),
          created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS music_library (
          id         INTEGER PRIMARY KEY AUTOINCREMENT,
          title      TEXT    NOT NULL DEFAULT '',
          artist     TEXT    NOT NULL DEFAULT '',
          album      TEXT    NOT NULL DEFAULT '',
          file_path  TEXT    NOT NULL UNIQUE,
          duration   INTEGER NOT NULL DEFAULT 0,
          lrc        TEXT    NOT NULL DEFAULT '',
          cover      TEXT    NOT NULL DEFAULT '',
          source_url TEXT    NOT NULL DEFAULT '',
          added_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS known_agents (
          id                TEXT PRIMARY KEY,
          name              TEXT NOT NULL,
          description       TEXT NOT NULL DEFAULT '',
          available         INTEGER NOT NULL DEFAULT 0,
          version           TEXT,
          invoke_type       TEXT,
          invoke_cmd        TEXT,
          invoke_args       TEXT NOT NULL DEFAULT '[]',
          notes             TEXT NOT NULL DEFAULT '',
          docs_url          TEXT,
          docs_search_query TEXT,
          detected_at       TEXT NOT NULL,
          updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS user_identities (
          canonical_id TEXT NOT NULL,
          channel      TEXT NOT NULL,
          external_id  TEXT NOT NULL,
          alias        TEXT DEFAULT '',
          bound_at     TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (channel, external_id)
        );

        CREATE TABLE IF NOT EXISTS focus_stack (
          depth         INTEGER PRIMARY KEY,
          topic         TEXT    NOT NULL,
          started_at    TEXT    NOT NULL,
          started_at_tick INTEGER NOT NULL,
          last_seen_tick INTEGER NOT NULL,
          hit_count     INTEGER NOT NULL DEFAULT 1,
          conclusions   TEXT    NOT NULL DEFAULT '[]',
          updated_at    TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS threads (
          id              TEXT PRIMARY KEY,
          topic           TEXT NOT NULL DEFAULT '[]',
          signature       TEXT NOT NULL DEFAULT '[]',
          label           TEXT NOT NULL DEFAULT '',
          summary         TEXT NOT NULL DEFAULT '',
          conclusions     TEXT NOT NULL DEFAULT '[]',
          status          TEXT NOT NULL DEFAULT 'open',
          created_at      TEXT NOT NULL,
          last_event_at   TEXT NOT NULL,
          last_event_tick INTEGER NOT NULL DEFAULT 0,
          hit_count       INTEGER NOT NULL DEFAULT 1,
          last_summary_at TEXT NOT NULL DEFAULT '',
          updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS commitments (
          id          TEXT PRIMARY KEY,
          thread_id   TEXT NOT NULL,
          text        TEXT NOT NULL DEFAULT '',
          status      TEXT NOT NULL DEFAULT 'open',
          channel     TEXT NOT NULL DEFAULT '',
          created_at  TEXT NOT NULL,
          closed_at   TEXT
        );

        CREATE TABLE IF NOT EXISTS thread_state (
          key   TEXT PRIMARY KEY,
          value TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS wechat_clawbot_tokens (
          from_user_id  TEXT    PRIMARY KEY,
          context_token TEXT    NOT NULL,
          updated_at    TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS recall_audit (
          id              INTEGER PRIMARY KEY AUTOINCREMENT,
          created_at      TEXT    NOT NULL DEFAULT (datetime('now')),
          turn_label      TEXT,
          from_id         TEXT,
          channel         TEXT,
          query_text      TEXT,
          matched_mem_ids TEXT    NOT NULL DEFAULT '[]',
          matched_count   INTEGER NOT NULL DEFAULT 0,
          chosen_count    INTEGER NOT NULL DEFAULT 0,
          event_type_dist TEXT    NOT NULL DEFAULT '{}',
          latency_ms      INTEGER,
          source          TEXT
        );

        CREATE TABLE IF NOT EXISTS extract_audit (
          id                INTEGER PRIMARY KEY AUTOINCREMENT,
          created_at        TEXT    NOT NULL DEFAULT (datetime('now')),
          turn_label        TEXT,
          from_id           TEXT,
          channel           TEXT,
          turn_summary      TEXT,
          extracted_mem_ids TEXT    NOT NULL DEFAULT '[]',
          extracted_count   INTEGER NOT NULL DEFAULT 0,
          event_type_dist   TEXT    NOT NULL DEFAULT '{}',
          latency_ms        INTEGER,
          skipped           INTEGER NOT NULL DEFAULT 0,
          skip_reason       TEXT
        );
        "#,
    )?;

    // ── 老库兜底补列（action_logs / reminders / known_agents） ──
    ensure_column(
        conn,
        "action_logs",
        "status",
        "status TEXT NOT NULL DEFAULT 'ok'",
    )?;
    ensure_column(
        conn,
        "action_logs",
        "risk",
        "risk TEXT NOT NULL DEFAULT 'medium'",
    )?;
    ensure_column(
        conn,
        "action_logs",
        "args_json",
        "args_json TEXT NOT NULL DEFAULT '{}'",
    )?;
    ensure_column(
        conn,
        "action_logs",
        "result_preview",
        "result_preview TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(
        conn,
        "action_logs",
        "error",
        "error TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(
        conn,
        "action_logs",
        "duration_ms",
        "duration_ms INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "action_logs",
        "source",
        "source TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(conn, "reminders", "recurrence_type", "recurrence_type TEXT")?;
    ensure_column(
        conn,
        "reminders",
        "recurrence_config",
        "recurrence_config TEXT",
    )?;
    ensure_column(conn, "known_agents", "docs_url", "docs_url TEXT")?;
    ensure_column(
        conn,
        "known_agents",
        "docs_search_query",
        "docs_search_query TEXT",
    )?;

    // ── 其余索引 ──
    ensure_index(
        conn,
        r#"
        CREATE INDEX IF NOT EXISTS idx_action_logs_timestamp ON action_logs(timestamp);
        CREATE INDEX IF NOT EXISTS idx_action_logs_status ON action_logs(status);
        CREATE INDEX IF NOT EXISTS idx_action_logs_risk ON action_logs(risk);
        CREATE INDEX IF NOT EXISTS idx_brain_ui_events_path_id ON brain_ui_events(path, id);
        CREATE INDEX IF NOT EXISTS idx_reminders_due_at ON reminders(status, due_at);
        CREATE INDEX IF NOT EXISTS idx_prefetch_tasks_enabled ON prefetch_tasks(enabled);
        CREATE INDEX IF NOT EXISTS idx_prefetch_expires ON prefetch_cache(expires_at);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_prefetch_source ON prefetch_cache(source);
        CREATE INDEX IF NOT EXISTS idx_ui_signals_unconsumed ON ui_signals(consumed, ts);
        CREATE INDEX IF NOT EXISTS idx_media_history_played_at ON media_history(played_at);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_media_history_url ON media_history(url);
        CREATE INDEX IF NOT EXISTS idx_music_title  ON music_library(title);
        CREATE INDEX IF NOT EXISTS idx_music_artist ON music_library(artist);
        CREATE INDEX IF NOT EXISTS idx_music_added  ON music_library(added_at);
        CREATE INDEX IF NOT EXISTS idx_identity_canonical ON user_identities(canonical_id);
        CREATE INDEX IF NOT EXISTS idx_commitments_status ON commitments(status);
        CREATE INDEX IF NOT EXISTS idx_recall_audit_created_at ON recall_audit(created_at);
        CREATE INDEX IF NOT EXISTS idx_recall_audit_from_id    ON recall_audit(from_id);
        CREATE INDEX IF NOT EXISTS idx_extract_audit_created_at ON extract_audit(created_at);
        CREATE INDEX IF NOT EXISTS idx_extract_audit_from_id    ON extract_audit(from_id);
        "#,
    )?;

    // ── LLM 指标表（P0 观测层，M1；幂等建表，老库零改动） ──
    // 评审修订：llm_calls 终态语义 = UPSERT（成功覆盖错误、attempt 取 MAX，见 §5.2 #1）；
    // llm_tool_calls 唯一键含 attempt 维度（重试路径不误伤）+ delegated_from（协作信任账本）。
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS llm_calls (
          id             INTEGER PRIMARY KEY AUTOINCREMENT,
          request_id     TEXT    NOT NULL,
          provider       TEXT    NOT NULL,
          model          TEXT    NOT NULL,
          stage          TEXT    NOT NULL DEFAULT '',
          started_at     TEXT    NOT NULL,
          ttft_ms        INTEGER,
          duration_ms    INTEGER,
          total_tokens   INTEGER,
          cached_tokens  INTEGER,
          usage_raw      TEXT    NOT NULL DEFAULT '',
          finish_reason  TEXT    NOT NULL DEFAULT '',
          error_stage    TEXT    NOT NULL DEFAULT '',
          error_class    TEXT    NOT NULL DEFAULT '',
          http_status    INTEGER,
          had_content    INTEGER NOT NULL DEFAULT 0,
          retryable      INTEGER NOT NULL DEFAULT 0,
          attempt        INTEGER NOT NULL DEFAULT 1,
          last_error     TEXT    NOT NULL DEFAULT '',
          fallback_used  INTEGER NOT NULL DEFAULT 0,
          context_bytes  INTEGER,
          created_at     TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_llm_calls_request_id ON llm_calls(request_id);

        CREATE TABLE IF NOT EXISTS llm_tool_calls (
          id            INTEGER PRIMARY KEY AUTOINCREMENT,
          request_id    TEXT    NOT NULL,
          round         INTEGER NOT NULL,
          attempt       INTEGER NOT NULL DEFAULT 1,
          tool_name     TEXT    NOT NULL,
          args_json     TEXT    NOT NULL DEFAULT '{}',
          result_json   TEXT    NOT NULL DEFAULT '',
          status        TEXT    NOT NULL DEFAULT 'ok',
          duration_ms   INTEGER NOT NULL DEFAULT 0,
          delegated_from TEXT   NOT NULL DEFAULT '',
          created_at    TEXT    NOT NULL DEFAULT (datetime('now')),
          UNIQUE(request_id, round, attempt, tool_name)
        );

        CREATE TABLE IF NOT EXISTS llm_metrics_daily (
          day             TEXT    PRIMARY KEY,
          total_calls     INTEGER NOT NULL DEFAULT 0,
          error_count     INTEGER NOT NULL DEFAULT 0,
          retry_count     INTEGER NOT NULL DEFAULT 0,
          fallback_count  INTEGER NOT NULL DEFAULT 0,
          aborted_count   INTEGER NOT NULL DEFAULT 0,
          total_tokens    INTEGER NOT NULL DEFAULT 0,
          cached_tokens   INTEGER NOT NULL DEFAULT 0,
          ttft_sum_ms     INTEGER NOT NULL DEFAULT 0,
          ttft_count      INTEGER NOT NULL DEFAULT 0,
          duration_sum_ms INTEGER NOT NULL DEFAULT 0,
          updated_at      TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS llm_context_sections (
          id         INTEGER PRIMARY KEY AUTOINCREMENT,
          request_id TEXT    NOT NULL,
          section    TEXT    NOT NULL,
          bytes      INTEGER NOT NULL DEFAULT 0,
          created_at TEXT    NOT NULL DEFAULT (datetime('now')),
          UNIQUE(request_id, section)
        );

        CREATE TABLE IF NOT EXISTS llm_turns (
          turn_id       TEXT    PRIMARY KEY,
          started_at    TEXT    NOT NULL,
          duration_ms   INTEGER,
          attribution   TEXT    NOT NULL DEFAULT '',
          is_tick       INTEGER NOT NULL DEFAULT 0,
          sections_hit  INTEGER,
          context_bytes INTEGER,
          calls         INTEGER NOT NULL DEFAULT 0,
          created_at    TEXT    NOT NULL DEFAULT (datetime('now'))
        );

        -- Phase 1：显式 Turn 状态机（turn_state 表）。
        -- 每个 user/tick turn 占一行，状态全程落库；启动时扫描未终态按 recover_policy 恢复。
        -- idempotency_key 部分唯一：同一逻辑轮重试复用同一行，防重复执行。
        CREATE TABLE IF NOT EXISTS turn_state (
          turn_id         INTEGER PRIMARY KEY AUTOINCREMENT,
          state           TEXT    NOT NULL DEFAULT 'received',
          round           INTEGER NOT NULL DEFAULT 0,
          attempt         INTEGER NOT NULL DEFAULT 1,
          idempotency_key TEXT    NOT NULL DEFAULT '',
          conversation_id INTEGER,
          channel         TEXT    NOT NULL DEFAULT '',
          from_id         TEXT    NOT NULL DEFAULT '',
          input_snapshot  TEXT    NOT NULL DEFAULT '',
          trace_id        TEXT    NOT NULL DEFAULT '',
          last_error      TEXT    NOT NULL DEFAULT '',
          recover_policy  TEXT    NOT NULL DEFAULT 'retry',
          started_at      TEXT    NOT NULL,
          updated_at      TEXT    NOT NULL DEFAULT (datetime('now')),
          finished_at     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_turn_state_state ON turn_state(state);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_turn_state_idem
          ON turn_state(idempotency_key) WHERE idempotency_key != '';

        -- 多Agent事项账本（PHILOSOPHY_MULTI_AGENT_MATTER.md 落地）：
        -- 事项=差距（期望态 vs 当前态）+ 验收标准（verifiable，无验收只是愿望）；
        -- 发起/执行/验证三主体分离（verifier_id != executor_id）；四种死法登记 death_reason；
        -- parent_id 支持分解（子事项可独立验证才可拆）。
        CREATE TABLE IF NOT EXISTS matters (
          id           INTEGER PRIMARY KEY AUTOINCREMENT,
          title        TEXT    NOT NULL,
          expectation  TEXT    NOT NULL,
          current_state TEXT   NOT NULL DEFAULT '',
          gap_desc     TEXT    NOT NULL DEFAULT '',
          acceptance_criteria TEXT NOT NULL DEFAULT '',
          status       TEXT    NOT NULL DEFAULT 'open',
          creator_id   TEXT    NOT NULL DEFAULT '',
          executor_id  TEXT,
          verifier_id  TEXT,
          parent_id    INTEGER,
          -- 命题6 决策点委托（默认全 0：人类保留全部决策；agent 仅在显式授权点可自主）
          delegation_choose    INTEGER NOT NULL DEFAULT 0,
          delegation_path      INTEGER NOT NULL DEFAULT 0,
          delegation_execute   INTEGER NOT NULL DEFAULT 0,
          delegation_verify    INTEGER NOT NULL DEFAULT 0,
          delegation_terminate INTEGER NOT NULL DEFAULT 0,
          -- 命题2 意图原句锚点（收敛对照"我理解为X做成了Y"）
          intent_original TEXT NOT NULL DEFAULT '',
          -- 命题3 分解可加性声明（子事项必填：all_completed | any_completed）
          additivity_decl TEXT NOT NULL DEFAULT '',
          -- 命题2/3 信号台账（JSON 数组：[{ts,kind,detail}]）
          signals TEXT NOT NULL DEFAULT '[]',
          evidence     TEXT    NOT NULL DEFAULT '',
          death_reason TEXT    NOT NULL DEFAULT '',
          started_at   TEXT,
          finished_at  TEXT,
          created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
          updated_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_matters_status ON matters(status);
        CREATE INDEX IF NOT EXISTS idx_matters_parent ON matters(parent_id);
        "#,
    )?;

    // ── M3：老库补 llm_calls.stage（幂等；新库 CREATE 已带） ──
    ensure_column(conn, "llm_calls", "stage", "stage TEXT NOT NULL DEFAULT ''")?;

    // ── M4：matter ledger 三缺口补列（命题2/3/6；幂等；新库 CREATE 已带） ──
    ensure_column(conn, "matters", "delegation_choose", "delegation_choose INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "matters", "delegation_path", "delegation_path INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "matters", "delegation_execute", "delegation_execute INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "matters", "delegation_verify", "delegation_verify INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "matters", "delegation_terminate", "delegation_terminate INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "matters", "intent_original", "intent_original TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "matters", "additivity_decl", "additivity_decl TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "matters", "signals", "signals TEXT NOT NULL DEFAULT '[]'")?;

    // ── 一次性历史迁移：外部渠道前缀 ID 统一为 canonical 用户（与 Node 版相同，flag 守卫） ──
    migrate_canonical_user(conn)?;

    // ── 重建 FTS 索引（覆盖已有历史数据；对老库是幂等全量重建） ──
    conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')")?;

    info!("[db schema] 迁移完成 ({} 张业务表)", BUSINESS_TABLES.len());
    Ok(())
}

/// 一次性迁移：`wechat:`/`discord:`/`feishu:`/`wecom:` 前缀的外部 ID 统一为 `ID:000001`，
/// 原值保留到 `external_party_id`。与 Node 版 schema.js 行为一致，用 config flag 防重入。
fn migrate_canonical_user(conn: &Connection) -> Result<()> {
    let already_done: bool = conn
        .prepare("SELECT COUNT(*) FROM config WHERE key = 'migration_canonical_user_v1'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|n| n > 0)?;
    if already_done {
        return Ok(());
    }

    let affected: i64 = conn
        .prepare(
            r#"
        SELECT COUNT(*) FROM conversations
        WHERE from_id LIKE 'wechat:%' OR from_id LIKE 'discord:%'
           OR from_id LIKE 'feishu:%' OR from_id LIKE 'wecom:%'
           OR to_id   LIKE 'wechat:%' OR to_id   LIKE 'discord:%'
           OR to_id   LIKE 'feishu:%' OR to_id   LIKE 'wecom:%'
        "#,
        )?
        .query_row([], |row| row.get(0))?;

    if affected > 0 {
        info!("[db schema] canonicalize {affected} 条外部渠道会话记录 → ID:000001");
        conn.execute_batch(
            r#"
            UPDATE conversations
              SET external_party_id = CASE WHEN external_party_id = '' OR external_party_id IS NULL THEN from_id ELSE external_party_id END,
                  from_id = 'ID:000001'
              WHERE from_id LIKE 'wechat:%' OR from_id LIKE 'discord:%'
                 OR from_id LIKE 'feishu:%' OR from_id LIKE 'wecom:%';
            UPDATE conversations
              SET external_party_id = CASE WHEN external_party_id = '' OR external_party_id IS NULL THEN to_id ELSE external_party_id END,
                  to_id = 'ID:000001'
              WHERE to_id LIKE 'wechat:%' OR to_id LIKE 'discord:%'
                 OR to_id LIKE 'feishu:%' OR to_id LIKE 'wecom:%';
            "#,
        )?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO config (key, value, updated_at) VALUES (?1, ?2, datetime('now'))",
        (
            "migration_canonical_user_v1",
            chrono::Utc::now().to_rfc3339(),
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_database_gets_full_schema() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();

        for table in BUSINESS_TABLES {
            let count: i64 = conn
                .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?1")
                .unwrap()
                .query_row([table], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 1, "业务表缺失: {table}");
        }

        // conversations 全量列（含 M1 语料 label 列）
        for col in [
            "id",
            "role",
            "from_id",
            "to_id",
            "content",
            "channel",
            "external_party_id",
            "focus_absorbed",
            "focus_topic",
            "open_question",
            "thread_id",
            "label",
            "delivery_status",
            "timestamp",
            "created_at",
        ] {
            assert!(
                has_column(&conn, "conversations", col).unwrap(),
                "缺列 conversations.{col}"
            );
        }

        // memories 全量列（含 embedding 与软隐藏三件套）
        for col in [
            "id",
            "event_type",
            "content",
            "detail",
            "title",
            "mem_id",
            "entities",
            "concepts",
            "tags",
            "links",
            "salience",
            "source_ref",
            "timestamp",
            "parent_id",
            "embedding",
            "created_at",
            "visibility",
            "hidden_at",
            "merged_into",
            "embedding_dim",
            "embedding_model",
        ] {
            assert!(
                has_column(&conn, "memories", col).unwrap(),
                "缺列 memories.{col}"
            );
        }

        // LLM 指标表关键结构（M1）
        for col in ["request_id", "attempt", "finish_reason", "context_bytes", "stage"] {
            assert!(
                has_column(&conn, "llm_calls", col).unwrap(),
                "缺列 llm_calls.{col}"
            );
        }
        for col in ["request_id", "round", "attempt", "tool_name", "delegated_from"] {
            assert!(
                has_column(&conn, "llm_tool_calls", col).unwrap(),
                "缺列 llm_tool_calls.{col}"
            );
        }
        // llm_calls.request_id 唯一索引（UPSERT 依赖）
        let idx_count: i64 = conn
            .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_llm_calls_request_id'")
            .unwrap()
            .query_row([], |row| row.get(0))
            .unwrap();
        assert_eq!(idx_count, 1, "llm_calls.request_id 唯一索引缺失");

        // Phase 1：turn_state 关键列 + 状态索引
        for col in [
            "turn_id",
            "state",
            "round",
            "attempt",
            "idempotency_key",
            "recover_policy",
            "started_at",
            "finished_at",
        ] {
            assert!(
                has_column(&conn, "turn_state", col).unwrap(),
                "缺列 turn_state.{col}"
            );
        }
        let state_idx: i64 = conn
            .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_turn_state_state'")
            .unwrap()
            .query_row([], |row| row.get(0))
            .unwrap();
        assert_eq!(state_idx, 1, "turn_state.state 索引缺失");

        // 触发器存在
        for trig in ["memories_ai", "memories_au", "memories_ad"] {
            let count: i64 = conn
                .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name = ?1")
                .unwrap()
                .query_row([trig], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 1, "触发器缺失: {trig}");
        }
    }

    #[test]
    fn schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap(); // 重复执行必须 no-op 不报错
        let count: i64 = conn
            .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'memories_fts_%'")
            .unwrap()
            .query_row([], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 32); // 31 业务表 + sqlite_sequence（事项账本 matters 加入）
    }

    #[test]
    fn old_database_gets_label_column_backfilled() {
        // 模拟老库：conversations 无 label 列 → initialize 后自动补列
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               role TEXT NOT NULL, from_id TEXT NOT NULL, content TEXT NOT NULL,
               timestamp TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations (role, from_id, content, timestamp) VALUES ('user','ID:000001','老数据','2026-08-01T00:00:00Z')",
            [],
        )
        .unwrap();
        initialize(&conn).unwrap();
        assert!(has_column(&conn, "conversations", "label").unwrap());
        // 老数据未被触碰
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn fts5_trigram_indexes_chinese() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO memories (event_type, content, detail, timestamp) VALUES (?1, ?2, ?2, ?3)",
            (
                "fact",
                "用户喜欢喝咖啡，偏好少糖。",
                "2026-08-08T00:00:00.000Z",
            ),
        )
        .unwrap();

        // trigram：查询 ≥3 字符可命中中文子串（外部内容表需 JOIN memories 过滤可见性）
        let hit: i64 = conn
            .prepare(
                "SELECT COUNT(*) FROM memories_fts
                 JOIN memories ON memories.id = memories_fts.rowid
                 WHERE memories_fts MATCH ?1 AND memories.visibility = 1",
            )
            .unwrap()
            .query_row(["\"喜欢喝咖啡\""], |row| row.get(0))
            .unwrap();
        assert_eq!(hit, 1, "FTS5 trigram 中文搜索未命中");

        // 无 MATCH 时不应崩溃（语法错误返回非零行数也算行为正确：Node 版会 catch）
        let _ = conn
            .prepare("SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH ?1")
            .map(|mut s| s.query_row(["喜欢喝咖啡"], |r| r.get::<_, i64>(0)));
    }
}
