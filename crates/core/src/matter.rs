//! 多Agent事项账本业务层（PHILOSOPHY_MULTI_AGENT_MATTER.md 落地）。
//!
//! 七个命题中对齐到本模块的规则：
//! 1. **事项是差距不是物**——创建必须带验收标准（`acceptance_criteria` 非空），
//!    无验收标准的只是愿望，不进入账本（拒绝创建）。
//! 3. **可分性是实用主义的**——子事项必须可独立验证才允许拆分；
//!    父事项关闭前所有子事项必须已终态（`can_close` 判定）。
//! 4. **发起/执行/验证三主体分离**——验证者不得是执行者（`verify` 强制校验）。
//! 5. **语言承诺 ≠ 世界事实**——提交验证必须带证据（`evidence` 非空）。
//! 7. **事项四种死法**——completed / cancelled / shelved / expired，终态登记死因。
//!
//! 数据层在 [`crate::db::repositories::matters`]；本模块负责状态机与规则校验，
//! 与 turn/turn_state 的分工一致。

use std::str::FromStr;

use crate::db::Db;
use crate::error::Result;

/// 事项状态集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatterStatus {
    /// 已登记，未开工
    Open,
    /// 执行中
    InProgress,
    /// 待验证（执行方已提交证据）
    AwaitingVerification,
    /// 正常完成（死法：completed）
    Completed,
    /// 用户/发起方取消（死法：cancelled）
    Cancelled,
    /// 搁置（死法：shelved）
    Shelved,
    /// 过期：长期未动，自动死亡（死法：expired）
    Expired,
}

impl MatterStatus {
    /// 落库用的稳定字符串（与 DB 列值一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            MatterStatus::Open => "open",
            MatterStatus::InProgress => "in_progress",
            MatterStatus::AwaitingVerification => "awaiting_verification",
            MatterStatus::Completed => "completed",
            MatterStatus::Cancelled => "cancelled",
            MatterStatus::Shelved => "shelved",
            MatterStatus::Expired => "expired",
        }
    }

    /// 是否终态（活债扫描与父关闭判定只认非终态）。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            MatterStatus::Completed
                | MatterStatus::Cancelled
                | MatterStatus::Shelved
                | MatterStatus::Expired
        )
    }

    /// 合法转移矩阵（显式白名单；非法转移由调用方拒绝落库）。
    pub fn can_transition_to(&self, to: MatterStatus) -> bool {
        use MatterStatus::*;
        matches!(
            (self, to),
            (Open, InProgress)
                | (Open, Cancelled)
                | (Open, Shelved)
                | (InProgress, AwaitingVerification)
                | (InProgress, Cancelled)
                | (InProgress, Shelved)
                | (AwaitingVerification, Completed)
                | (AwaitingVerification, Cancelled)
                | (AwaitingVerification, Shelved)
                // 过期是"未动"扫描的结果：任何非终态都可被扫成 expired
                | (Open, Expired)
                | (InProgress, Expired)
                | (AwaitingVerification, Expired)
        )
    }
}

impl FromStr for MatterStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "open" => MatterStatus::Open,
            "in_progress" => MatterStatus::InProgress,
            "awaiting_verification" => MatterStatus::AwaitingVerification,
            "completed" => MatterStatus::Completed,
            "cancelled" => MatterStatus::Cancelled,
            "shelved" => MatterStatus::Shelved,
            "expired" => MatterStatus::Expired,
            other => return Err(format!("未知事项状态: {other}")),
        })
    }
}

/// 创建事项（规则：验收标准非空、验证者≠执行者）。返回新事项 id。
#[allow(clippy::too_many_arguments)]
pub fn create(
    db: &Db,
    title: &str,
    expectation: &str,
    current_state: &str,
    gap_desc: &str,
    acceptance_criteria: &str,
    creator_id: &str,
    executor_id: Option<&str>,
    verifier_id: Option<&str>,
    parent_id: Option<i64>,
) -> Result<i64> {
    // 命题1：无验收标准的只是愿望，不进入账本。
    let criteria = acceptance_criteria.trim();
    if criteria.is_empty() {
        return Err(crate::error::CoreError::Validation(
            "事项必须有验收标准（acceptance_criteria），无验收标准的只是愿望".into(),
        ));
    }
    // 命题4：验证者不得是执行者。
    if let (Some(exec), Some(ver)) = (executor_id, verifier_id) {
        if exec == ver {
            return Err(crate::error::CoreError::Validation(
                "验证者不能同时是执行者（三主体分离）".into(),
            ));
        }
    }
    // 命题3：父事项必须存在才允许挂子事项。
    if let Some(pid) = parent_id {
        if crate::db::repositories::matters::get(db, pid)?.is_none() {
            return Err(crate::error::CoreError::Validation(
                format!("父事项不存在: {pid}"),
            ));
        }
    }
    crate::db::repositories::matters::create(
        db,
        title,
        expectation,
        current_state,
        gap_desc,
        criteria,
        creator_id,
        executor_id,
        verifier_id,
        parent_id,
    )
}

/// 开始执行：open → in_progress（登记 started_at）。
pub fn start(db: &Db, id: i64) -> Result<()> {
    transition(db, id, MatterStatus::InProgress, |_| {
        crate::db::repositories::matters::mark_started(db, id)
    })
}

/// 提交验证证据：in_progress → awaiting_verification。
/// 规则（命题5）：证据非空——语言承诺必须落到世界事实。
pub fn submit_evidence(db: &Db, id: i64, evidence: &str) -> Result<()> {
    if evidence.trim().is_empty() {
        return Err(crate::error::CoreError::Validation(
            "提交验证必须附证据（evidence），否则只是语言承诺".into(),
        ));
    }
    transition(db, id, MatterStatus::AwaitingVerification, |_| {
        crate::db::repositories::matters::set_evidence(db, id, evidence)
    })
}

/// 验证通过：awaiting_verification → completed。
/// 规则（命题4）：验证者必须与登记 verifier 一致，且不得是执行者。
pub fn verify(db: &Db, id: i64, verifier_id: &str) -> Result<()> {
    let row = crate::db::repositories::matters::get(db, id)?
        .ok_or_else(|| crate::error::CoreError::NotFound(format!("事项不存在: {id}")))?;

    let registered = row
        .verifier_id
        .ok_or_else(|| crate::error::CoreError::Validation("事项未登记验证者".into()))?;
    if registered != verifier_id {
        return Err(crate::error::CoreError::Validation(format!(
            "验证者必须是登记的 {registered}，实际是 {verifier_id}"
        )));
    }
    if let Some(exec) = &row.executor_id {
        if exec == verifier_id {
            return Err(crate::error::CoreError::Validation(
                "验证者不能同时是执行者（三主体分离）".into(),
            ));
        }
    }
    transition(db, id, MatterStatus::Completed, |_| {
        crate::db::repositories::matters::mark_finished(db, id, "completed", "completed")
    })
}

/// 取消：任意非终态 → cancelled（死因登记）。
pub fn cancel(db: &Db, id: i64) -> Result<()> {
    transition(db, id, MatterStatus::Cancelled, |_| {
        crate::db::repositories::matters::mark_finished(db, id, "cancelled", "cancelled")
    })
}

/// 搁置：任意非终态 → shelved（死因登记）。
pub fn shelve(db: &Db, id: i64) -> Result<()> {
    transition(db, id, MatterStatus::Shelved, |_| {
        crate::db::repositories::matters::mark_finished(db, id, "shelved", "shelved")
    })
}

/// 幽灵检测：把「最后活动早于 stale_before（ISO 字符串比较）」的非终态事项标记为 expired。
/// 返回本次被处死的事项 id 列表。
pub fn expire_stale(db: &Db, stale_before: &str) -> Result<Vec<i64>> {
    let mut dead = Vec::new();
    for row in crate::db::repositories::matters::scan_active(db)? {
        // updated_at 为 SQLite datetime('now')（UTC "YYYY-MM-DD HH:MM:SS"），
        // stale_before 需按同格式传入；字符串比较即时间序比较。
        if row.updated_at < stale_before.to_string() {
            crate::db::repositories::matters::mark_finished(db, row.id, "expired", "expired")?;
            dead.push(row.id);
        }
    }
    Ok(dead)
}

/// 父事项能否关闭：所有子事项都已终态（分解可加性；无子事项时视为可关闭）。
pub fn can_close(db: &Db, parent_id: i64) -> Result<bool> {
    for child in crate::db::repositories::matters::list_children(db, parent_id)? {
        let status: MatterStatus = child
            .status
            .parse()
            .map_err(|e: String| crate::error::CoreError::Validation(e))?;
        if !status.is_terminal() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 活债清单（全部非终态事项），供注入/审计用。
pub fn list_active(db: &Db) -> Result<Vec<crate::db::repositories::matters::MatterRow>> {
    crate::db::repositories::matters::scan_active(db)
}

// ── 内部：校验转移合法性后执行动作 ──────────────────────────────

fn transition(
    db: &Db,
    id: i64,
    to: MatterStatus,
    action: impl FnOnce(i64) -> Result<()>,
) -> Result<()> {
    let row = crate::db::repositories::matters::get(db, id)?
        .ok_or_else(|| crate::error::CoreError::NotFound(format!("事项不存在: {id}")))?;
    let from: MatterStatus = row
        .status
        .parse()
        .map_err(|e: String| crate::error::CoreError::Validation(e))?;
    if !from.can_transition_to(to) {
        return Err(crate::error::CoreError::Validation(format!(
            "非法状态转移: {} → {}",
            from.as_str(),
            to.as_str()
        )));
    }
    action(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_database;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        open_database(dir.path().join("t.db")).unwrap()
    }

    fn seed(db: &Db) -> i64 {
        create(
            db,
            "接入协作者",
            "协作者能独立跑工具循环",
            "仅骨架",
            "协作者没有 LLM 轮",
            "协作者在测试环境完成一次真实工具调用并留痕",
            "ID:000001",
            Some("codex"),
            Some("jarvis"),
            None,
        )
        .unwrap()
    }

    #[test]
    fn create_rejects_wish_without_criteria() {
        let db = test_db();
        let err = create(
            &db,
            "愿望",
            "期望态",
            "当前态",
            "",
            "  ",
            "ID:000001",
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("验收标准"));
    }

    #[test]
    fn create_rejects_self_verification() {
        let db = test_db();
        let err = create(
            &db,
            "x",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            Some("codex"),
            Some("codex"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("三主体分离"));
    }

    #[test]
    fn happy_path_lifecycle() {
        let db = test_db();
        let id = seed(&db);

        start(&db, id).unwrap();
        assert!(start(&db, id).unwrap_err().to_string().contains("非法状态转移"));

        submit_evidence(&db, id, "工具调用 trace 已落库").unwrap();
        assert!(
            submit_evidence(&db, id, "再次提交")
                .unwrap_err()
                .to_string()
                .contains("非法状态转移")
        );

        // 非登记验证者被拒
        let err = verify(&db, id, "codex").unwrap_err();
        assert!(err.to_string().contains("验证者必须是登记的 jarvis"));

        verify(&db, id, "jarvis").unwrap();
        let row = crate::db::repositories::matters::get(&db, id).unwrap().unwrap();
        assert_eq!(row.status, "completed");
        assert_eq!(row.death_reason, "completed");
        assert!(row.finished_at.is_some());
    }

    #[test]
    fn verify_rejects_executor_as_verifier() {
        let db = test_db();
        // 登记执行者 codex、验证者 jarvis；但代码层面防呆：直接以执行者身份验证
        let id = seed(&db);
        start(&db, id).unwrap();
        submit_evidence(&db, id, "ev").unwrap();
        let err = verify(&db, id, "codex").unwrap_err();
        assert!(err.to_string().contains("验证者必须是登记的 jarvis"));
    }

    #[test]
    fn cancel_and_shelve_register_death() {
        let db = test_db();
        let a = seed(&db);
        cancel(&db, a).unwrap();
        let row = crate::db::repositories::matters::get(&db, a).unwrap().unwrap();
        assert_eq!(row.status, "cancelled");
        assert_eq!(row.death_reason, "cancelled");

        let b = seed(&db);
        start(&db, b).unwrap();
        shelve(&db, b).unwrap();
        let row = crate::db::repositories::matters::get(&db, b).unwrap().unwrap();
        assert_eq!(row.status, "shelved");
    }

    #[test]
    fn expired_is_terminal_and_excluded_from_active() {
        let db = test_db();
        let id = seed(&db);
        // 手动把 updated_at 拨回过去，模拟长期未动
        {
            let conn = db.conn();
            conn.execute(
                "UPDATE matters SET updated_at = '2026-01-01 00:00:00' WHERE id = ?1",
                [id],
            )
            .unwrap();
        } // 块作用域：提前释放 conn 锁，否则 expire_stale 再次上锁死锁

        let dead = expire_stale(&db, "2026-06-01 00:00:00").unwrap();
        assert_eq!(dead, vec![id]);
        let row = crate::db::repositories::matters::get(&db, id).unwrap().unwrap();
        assert_eq!(row.status, "expired");
        assert_eq!(row.death_reason, "expired");
        assert!(list_active(&db).unwrap().is_empty());
    }

    #[test]
    fn parent_cannot_close_with_open_children() {
        let db = test_db();
        let parent = seed(&db);
        create(
            &db,
            "子事项",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            Some("codex"),
            Some("jarvis"),
            Some(parent),
        )
        .unwrap();
        assert!(!can_close(&db, parent).unwrap());

        // 子事项全部终态后父可关闭
        let kids = crate::db::repositories::matters::list_children(&db, parent).unwrap();
        for k in &kids {
            cancel(&db, k.id).unwrap();
        }
        assert!(can_close(&db, parent).unwrap());
    }

    #[test]
    fn child_must_reference_existing_parent() {
        let db = test_db();
        let err = create(
            &db,
            "孤儿",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            None,
            None,
            Some(9999),
        )
        .unwrap_err();
        assert!(err.to_string().contains("父事项不存在"));
    }
}
