//! M1 关键承诺验证：现有 `jarvis.db` 打开数据完好、仅新增观测表。
//!
//! 使用 SQLite backup API 将真实库复制到临时目录后打开（只读源库，
//! 不修改用户数据）。开发机存在真实 DB 时全量断言；CI/其他环境
//! 无此文件则自动跳过。
//!
//! 契约（幂等迁移）：打开真实库后，仅新增"源库中尚不存在"的观测表
//! ——M1 的 3 张 LLM 指标表（llm_calls / llm_tool_calls / llm_metrics_daily）、
//! M3 的 2 张观测表（llm_context_sections / llm_turns）、P1 的 turn_state、
//! 事项账本 matters / matter_events。若源库已被此前运行迁移过（8 张观测表
//! 已存在），增量应为 0；绝不少表、不动用户数据——这正是"观测层不碰
//! 用户数据"的承诺边界。

use std::collections::HashSet;

use bailongma_core::db::open_database;

/// 候选真实用户库路径（Windows 开发机）：
/// 优先 BAILONGMA_USER_DIR=E:\BailongmaData 下的活跃库（serve 实际读写），
/// 回退到 AppData 默认路径（旧版/未设用户目录时）。两个都无则跳过。
const REAL_DB_CANDIDATES: &[&str] = &[
    r"E:\BailongmaData\data\jarvis.db",
    r"C:\Users\ADMIN\AppData\Roaming\Bailongma\data\jarvis.db",
];

/// M1 新增的 3 张 LLM 指标表（老库打开后应恰好新增这 3 张）。
const NEW_M1_TABLES: &[&str] = &["llm_calls", "llm_tool_calls", "llm_metrics_daily"];

/// M3 新增的 2 张观测表（上下文 section 明细 + turn 级记录）。
const NEW_M3_TABLES: &[&str] = &["llm_context_sections", "llm_turns"];

/// P1 新增的 1 张状态机表（显式 Turn 状态机数据层）。
const NEW_P1_TABLES: &[&str] = &["turn_state"];

/// 事项账本（多Agent事项哲学落地：差距/验收/三主体分离/四种死法）。
const NEW_MATTERS_TABLES: &[&str] = &["matters", "matter_events"];

fn row_count(conn: &rusqlite::Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap();
    n == 1
}

#[test]
fn real_db_copy_opens_without_data_loss() {
    // 1. 定位真实库：取第一个存在的候选路径
    let real = REAL_DB_CANDIDATES
        .iter()
        .map(std::path::Path::new)
        .find(|p| p.exists());
    let Some(real) = real else {
        eprintln!("SKIP: 真实 jarvis.db 不存在（非开发机/CI），跳过零迁移验证");
        return;
    };

    // 2. 备份：只读打开源库 → backup 到临时文件（含 WAL 一致性快照）
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("jarvis_copy.db");
    let src_convs;
    let src_mems;
    let src_tables: i64;
    let src_missing_new: usize; // 源库中缺失的观测表数（迁移前快照）
    {
        let src = rusqlite::Connection::open(real).unwrap();
        src_convs = row_count(&src, "conversations");
        src_mems = row_count(&src, "memories");
        src_tables = src
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let all_new = NEW_M1_TABLES
            .iter()
            .chain(NEW_M3_TABLES.iter())
            .chain(NEW_P1_TABLES.iter())
            .chain(NEW_MATTERS_TABLES.iter())
            .copied()
            .collect::<Vec<_>>();
        src_missing_new = all_new.iter().filter(|t| !table_exists(&src, t)).count();
        src.backup(rusqlite::DatabaseName::Main, &copy, None)
            .unwrap();
    }

    // 3. 用我们的 Db 打开副本（触发幂等迁移）
    let db = open_database(&copy).unwrap();
    let conn = db.conn();

    // 4. 核心表行数零变化
    assert_eq!(
        row_count(&conn, "conversations"),
        src_convs,
        "conversations 行数被改动"
    );
    assert_eq!(
        row_count(&conn, "memories"),
        src_mems,
        "memories 行数被改动"
    );

    // 5. 表结构变化仅限观测层，且严格等于"源库缺失的观测表数"（幂等：
    //    源库已被迁移过则增量为 0，绝不多建、绝不少建、绝不动用户数据）
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        tables,
        src_tables + src_missing_new as i64,
        "表数量变化：应仅新增源库缺失的观测表（幂等增量，实际 src={src_tables} 缺={src_missing_new}）"
    );
    for name in NEW_M1_TABLES {
        assert!(table_exists(&conn, name), "M1 新增表 {name} 应存在");
    }
    for name in NEW_M3_TABLES {
        assert!(table_exists(&conn, name), "M3 新增表 {name} 应存在");
    }
    for name in NEW_P1_TABLES {
        assert!(table_exists(&conn, name), "P1 新增表 {name} 应存在");
    }
    for name in NEW_MATTERS_TABLES {
        assert!(table_exists(&conn, name), "事项账本新增表 {name} 应存在");
    }

    // 5.5 M3：llm_calls.stage 补列幂等
    let stage_col: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('llm_calls') WHERE name = 'stage'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stage_col, 1, "老库补列 llm_calls.stage 应幂等生效");

    // 5.6 P1：turn_state 关键列 + 恢复扫描索引齐备
    let state_col: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('turn_state') WHERE name = 'state'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state_col, 1, "老库 turn_state.state 列应存在");
    let idem_idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_turn_state_idem'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idem_idx, 1, "老库 turn_state 幂等键部分唯一索引应存在");

    // 6. 关键迁移列存在且历史数据可读（仅当库里确有 embedding 历史）
    let dims: Vec<(i64, String)> = conn
        .prepare("SELECT embedding_dim, embedding_model FROM memories WHERE embedding IS NOT NULL LIMIT 3")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for (dim, model) in dims {
        assert!(dim > 0, "历史 embedding_dim 应为正数");
        assert!(!model.is_empty(), "历史 embedding_model 不应为空");
    }

    // 7. FTS5 trigram 在历史数据上真实可搜（库内有可见记忆时才断言）
    let vis_mems: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories WHERE visibility = 1", [], |r| r.get(0))
        .unwrap();
    if vis_mems > 0 {
        let hit: i64 = conn
            .prepare(
                "SELECT COUNT(*) FROM memories_fts
                 JOIN memories ON memories.id = memories_fts.rowid
                 WHERE memories_fts MATCH ?1 AND memories.visibility = 1",
            )
            .unwrap()
            .query_row(["\u{22}strategy_project\u{22}"], |r| r.get(0))
            .unwrap();
        assert!(hit > 0, "FTS5 应在历史数据上命中（strategy_project）");
    }

    // 8. canonical 迁移 flag 已落库（一次性迁移只跑一次的证据）
    let flag: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM config WHERE key = 'migration_canonical_user_v1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(flag, 1, "一次性迁移 flag 应已写入 config");

    eprintln!(
        "✓ 真实库幂等迁移验证通过: conversations={src_convs}, memories={src_mems}, tables={src_tables}->{tables} (新增观测表 {src_missing_new})"
    );
}
