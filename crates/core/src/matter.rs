//! 多Agent事项账本业务层（PHILOSOPHY_MULTI_AGENT_MATTER.md 落地）。
//!
//! 七个命题中对齐到本模块的规则：
//! 1. **事项是差距不是物**——创建必须带验收标准（`acceptance_criteria` 非空），
//!    无验收标准的只是愿望，不进入账本（拒绝创建）。
//! 2. **意图漂移可度量**——创建时锚定 `intent_original` 意图原句；收敛报告给出
//!    "原意图 / 我理解 / 做成了"三栏对照，漂移落信号台账（`intent_drift`）。
//! 3. **分解可加性声明**——子事项必须声明与母项验收判据的还原关系
//!    （`all_completed` / `any_completed`），声明缺失或未知拒绝创建；
//!    母项关闭时声明与实际结果对不上 → 记 `additivity_violation` 信号。
//! 4. **发起/执行/验证三主体分离**——验证者不得是执行者（`verify` 强制校验）。
//! 5. **语言承诺 ≠ 世界事实**——提交验证必须带证据（`evidence` 非空）。
//! 6. **决策点委托显式化**——choose/path/execute/verify/terminate 五决策点默认
//!    全归人类（false）；agent 只在显式授权点可自主，越权拒绝；只有发起人能改委托。
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

/// 决策点（命题6）：agent 在哪些点上可以自主行使判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecisionPoint {
    /// 选哪个事项/方案
    Choose,
    /// 选哪条执行路径
    Path,
    /// 执行动作
    Execute,
    /// 验证结果
    Verify,
    /// 终止/关闭事项
    Terminate,
}

impl DecisionPoint {
    /// 落库用的稳定字符串（与 delegation_* 列名一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionPoint::Choose => "choose",
            DecisionPoint::Path => "path",
            DecisionPoint::Execute => "execute",
            DecisionPoint::Verify => "verify",
            DecisionPoint::Terminate => "terminate",
        }
    }
}

/// 委托地图（命题6）：五决策点全 false = 人类保留全部决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DelegationMap {
    pub choose: bool,
    pub path: bool,
    pub execute: bool,
    pub verify: bool,
    pub terminate: bool,
}

/// 从行投影取委托地图（命题6 数据视图）。
pub fn delegation_map(row: &crate::db::repositories::matters::MatterRow) -> DelegationMap {
    DelegationMap {
        choose: row.delegation_choose,
        path: row.delegation_path,
        execute: row.delegation_execute,
        verify: row.delegation_verify,
        terminate: row.delegation_terminate,
    }
}

/// 分解可加性声明（命题3）：子事项验收判据与母项验收判据的还原关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdditivityDecl {
    /// 母项可关闭要求：全部子事项 completed
    AllCompleted,
    /// 母项可关闭要求：至少一个子事项 completed
    AnyCompleted,
}

impl AdditivityDecl {
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s.trim() {
            "all_completed" => Ok(AdditivityDecl::AllCompleted),
            "any_completed" => Ok(AdditivityDecl::AnyCompleted),
            "" => Err("可加性声明不能为空（子事项必须声明与母项验收判据的还原关系）".into()),
            other => Err(format!(
                "未知可加性声明: {other}（仅支持 all_completed | any_completed）"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AdditivityDecl::AllCompleted => "all_completed",
            AdditivityDecl::AnyCompleted => "any_completed",
        }
    }
}

/// 意图漂移对照报告（命题2）。
pub struct DriftVerdict {
    /// 三栏对齐（无理解漂移且无执行漂移）
    pub aligned: bool,
    /// 人类可读对照：原意图 / 我理解 / 做成了
    pub report: String,
}

/// 幽灵事项候选（命题7 兜底）：挂起无主 + 超时无进展 + 无验收判据。
/// 只出信号不自动终止；终止/搁置由人类决策。
#[derive(Debug)]
pub struct GhostCandidate {
    pub id: i64,
    pub title: String,
    pub updated_at: String,
    pub reason: String,
}

/// 创建事项（规则：验收标准非空、验证者≠执行者、子项必须带可加性声明）。返回新事项 id。
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
    intent_original: &str,
    additivity_decl: &str,
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
    // 命题3：父事项必须存在才允许挂子事项；子事项必须声明可加性关系。
    if let Some(pid) = parent_id {
        if crate::db::repositories::matters::get(db, pid)?.is_none() {
            return Err(crate::error::CoreError::Validation(format!(
                "父事项不存在: {pid}"
            )));
        }
        AdditivityDecl::parse(additivity_decl)?;
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
        intent_original,
        additivity_decl,
    )
}

/// 授权/收回某个决策点（命题6）。只有发起人（人类）能改委托地图。
pub fn delegate(
    db: &Db,
    id: i64,
    actor: &str,
    point: DecisionPoint,
    allowed: bool,
) -> Result<()> {
    let row = crate::db::repositories::matters::get(db, id)?
        .ok_or_else(|| crate::error::CoreError::NotFound(format!("事项不存在: {id}")))?;
    if row.creator_id != actor {
        return Err(crate::error::CoreError::Validation(format!(
            "只有发起人（{}）能授权决策点，实际是 {actor}",
            row.creator_id
        )));
    }
    crate::db::repositories::matters::set_delegation(db, id, point.as_str(), allowed)
}

/// 某 actor 在某决策点是否被授权（命题6）。发起人（人类）始终放行；
/// agent 必须同时满足"角色匹配（执行者/验证者）+ 对应委托位为 true"。
pub fn decision_allowed(db: &Db, id: i64, actor: &str, point: DecisionPoint) -> Result<bool> {
    let row = crate::db::repositories::matters::get(db, id)?
        .ok_or_else(|| crate::error::CoreError::NotFound(format!("事项不存在: {id}")))?;
    if row.creator_id == actor {
        return Ok(true);
    }
    let dm = delegation_map(&row);
    let is_exec = row.executor_id.as_deref() == Some(actor);
    let is_ver = row.verifier_id.as_deref() == Some(actor);
    let ok = match point {
        DecisionPoint::Choose => is_exec && dm.choose,
        DecisionPoint::Path => is_exec && dm.path,
        DecisionPoint::Execute => is_exec && dm.execute,
        DecisionPoint::Verify => {
            if is_ver && dm.verify {
                true
            } else if is_exec && dm.verify && row.verifier_id.is_none() {
                // 命题4/7：未登记独立验证者时，verify 委托给执行者 = 允许自证完成（降级）
                true
            } else {
                false
            }
        }
        DecisionPoint::Terminate => (is_exec || is_ver) && dm.terminate,
    };
    Ok(ok)
}

fn require_allowed(db: &Db, id: i64, actor: &str, point: DecisionPoint) -> Result<()> {
    if !decision_allowed(db, id, actor, point)? {
        return Err(crate::error::CoreError::Validation(format!(
            "{actor} 在 {} 决策点未获授权（委托地图默认人类保留，需发起人显式授权）",
            point.as_str()
        )));
    }
    Ok(())
}

/// 开始执行：open → in_progress（登记 started_at）。需 execute 决策点授权。
pub fn start(db: &Db, id: i64, actor: &str) -> Result<()> {
    transition(db, id, MatterStatus::InProgress, actor, |_| {
        require_allowed(db, id, actor, DecisionPoint::Execute)?;
        crate::db::repositories::matters::mark_started(db, id)
    })
}

/// 提交验证证据：in_progress → awaiting_verification。需 execute 决策点授权。
/// 规则（命题5）：证据非空——语言承诺必须落到世界事实。
pub fn submit_evidence(db: &Db, id: i64, actor: &str, evidence: &str) -> Result<()> {
    if evidence.trim().is_empty() {
        return Err(crate::error::CoreError::Validation(
            "提交验证必须附证据（evidence），否则只是语言承诺".into(),
        ));
    }
    transition(db, id, MatterStatus::AwaitingVerification, actor, |_| {
        require_allowed(db, id, actor, DecisionPoint::Execute)?;
        crate::db::repositories::matters::set_evidence(db, id, evidence)
    })
}

/// 验证通过：awaiting_verification → completed。需 verify 决策点授权。
/// 规则（命题4）：验证者必须与登记 verifier 一致，且不得是执行者。
/// 母项完成时执行命题3 收口：子项可加性声明与实际结果对不上 → 记信号。
pub fn verify(db: &Db, id: i64, actor: &str) -> Result<()> {
    let row = crate::db::repositories::matters::get(db, id)?
        .ok_or_else(|| crate::error::CoreError::NotFound(format!("事项不存在: {id}")))?;

    match &row.verifier_id {
        // 命题4/7：未登记独立验证者 → verifier 缺省=执行者，执行者自证完成（可信等级降级）
        None => {
            let is_exec = row.executor_id.as_deref() == Some(actor);
            if !is_exec {
                return Err(crate::error::CoreError::Validation(format!(
                    "事项未登记验证者，仅执行者 {:?} 可自证完成；实际是 {actor}",
                    row.executor_id
                )));
            }
            transition(db, id, MatterStatus::Completed, actor, |_| {
                require_allowed(db, id, actor, DecisionPoint::Verify)?;
                crate::db::repositories::matters::set_self_verified(db, id, true)?;
                crate::db::repositories::matters::mark_finished(db, id, "completed", "completed")
            })?;
            record_signal(
                db,
                id,
                "self_verified",
                "验证者缺省为执行者：执行者自证完成，可信等级降级（不再全信）".into(),
            )?;
        }
        // 命题4：独立验证者路径（注册验证者 ≠ 执行者）
        Some(registered) => {
            if registered != actor {
                return Err(crate::error::CoreError::Validation(format!(
                    "验证者必须是登记的 {registered}，实际是 {actor}"
                )));
            }
            if let Some(exec) = &row.executor_id {
                if exec == actor {
                    return Err(crate::error::CoreError::Validation(
                        "验证者不能同时是执行者（三主体分离）".into(),
                    ));
                }
            }
            transition(db, id, MatterStatus::Completed, actor, |_| {
                require_allowed(db, id, actor, DecisionPoint::Verify)?;
                crate::db::repositories::matters::set_self_verified(db, id, false)?;
                crate::db::repositories::matters::mark_finished(db, id, "completed", "completed")
            })?;
        }
    }
    check_additivity_on_complete(db, id)?;
    Ok(())
}

/// 取消：任意非终态 → cancelled（死因登记）。需 terminate 决策点授权。
pub fn cancel(db: &Db, id: i64, actor: &str) -> Result<()> {
    transition(db, id, MatterStatus::Cancelled, actor, |_| {
        require_allowed(db, id, actor, DecisionPoint::Terminate)?;
        crate::db::repositories::matters::mark_finished(db, id, "cancelled", "cancelled")
    })
}

/// 搁置：任意非终态 → shelved（死因登记）。需 terminate 决策点授权。
pub fn shelve(db: &Db, id: i64, actor: &str) -> Result<()> {
    transition(db, id, MatterStatus::Shelved, actor, |_| {
        require_allowed(db, id, actor, DecisionPoint::Terminate)?;
        crate::db::repositories::matters::mark_finished(db, id, "shelved", "shelved")
    })
}

/// 幽灵检测：把「最后活动早于 stale_before（ISO 字符串比较）」的非终态事项标记为 expired。
/// 返回本次被处死的事项 id 列表。
pub fn expire_stale(db: &Db, stale_before: &str) -> Result<Vec<i64>> {
    let mut dead = Vec::new();
    for row in crate::db::repositories::matters::scan_active(db)? {
        // updated_at 为 SQLite datetime('now')（UTC "YYYY-MM-DD HH:MM:SS"），
        // stale_before 需按同样格式传入；字符串比较即时序比较。
        if row.updated_at < stale_before.to_string() {
            crate::db::repositories::matters::mark_finished(db, row.id, "expired", "expired")?;
            crate::db::repositories::matters::insert_event(
                db,
                row.id,
                "expired",
                &row.status,
                "expired",
                &("stale_before=".to_owned() + stale_before),
                "system",
            )?;
            dead.push(row.id);
        }
    }
    Ok(dead)
}

/// 幽灵事项检测（命题4/7 兜底）：挂起无主（executor 空）+ 超过 stale_before 无进展
/// + 无验收判据（老数据可能为空）→ 候选清单；只出 ghost_candidate 信号，
/// 不自动终止，终止/搁置由人类决策。
pub fn detect_ghosts(db: &Db, stale_before: &str) -> Result<Vec<GhostCandidate>> {
    let mut ghosts = Vec::new();
    for row in crate::db::repositories::matters::scan_active(db)? {
        let orphan = row.executor_id.is_none();
        let idle = row.updated_at < stale_before.to_string();
        let no_criteria = row.acceptance_criteria.trim().is_empty();
        if orphan && idle && no_criteria {
            record_signal(
                db,
                row.id,
                "ghost_candidate",
                format!("幽灵事项：挂起无主且 {stale_before} 前无进展且无验收判据，建议终止或显式搁置"),
            )?;
            ghosts.push(GhostCandidate {
                id: row.id,
                title: row.title,
                updated_at: row.updated_at,
                reason: "挂起无主 / 超时无进展 / 无验收判据".into(),
            });
        }
    }
    Ok(ghosts)
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

/// 命题2 收敛对照：给出"原意图 / 我理解 / 做成了"三栏报告；
/// 理解或执行发生漂移时落 `intent_drift` 信号，并返回对齐判定。
pub fn intent_drift_report(
    db: &Db,
    id: i64,
    understood_as: &str,
    done_as: &str,
) -> Result<DriftVerdict> {
    let row = crate::db::repositories::matters::get(db, id)?
        .ok_or_else(|| crate::error::CoreError::NotFound(format!("事项不存在: {id}")))?;
    let original = row.intent_original.trim();
    let understood = understood_as.trim();
    let done = done_as.trim();

    let mut drift = Vec::new();
    if !original.is_empty() && original != understood {
        drift.push(format!("理解漂移（原:{original} ≠ 理解:{understood}）"));
        record_signal(
            db,
            id,
            "intent_drift",
            format!("理解漂移: 原={original} 理解={understood}"),
        )?;
    }
    if understood != done {
        drift.push(format!("执行漂移（理解:{understood} ≠ 做成:{done}）"));
        record_signal(
            db,
            id,
            "intent_drift",
            format!("执行漂移: 理解={understood} 做成={done}"),
        )?;
    }

    let mut report = format!("原意图: {original}\n我理解: {understood}\n做成了: {done}");
    if drift.is_empty() {
        report.push_str("\n结论: 对齐");
    } else {
        report.push_str(&format!("\n结论: {}", drift.join("; ")));
    }
    Ok(DriftVerdict {
        aligned: drift.is_empty(),
        report,
    })
}

/// 信号台账追加（命题2/3）：kind 如 intent_drift / additivity_violation。
pub fn record_signal(db: &Db, id: i64, kind: &str, detail: String) -> Result<()> {
    let row = crate::db::repositories::matters::get(db, id)?
        .ok_or_else(|| crate::error::CoreError::NotFound(format!("事项不存在: {id}")))?;
    let mut signals: Vec<serde_json::Value> =
        serde_json::from_str(&row.signals).unwrap_or_default();
    signals.push(serde_json::json!({
        "ts": crate::db::repositories::matters::now_utc(db)?,
        "kind": kind,
        "detail": detail,
    }));
    crate::db::repositories::matters::set_signals(db, id, &serde_json::to_string(&signals)?)
}

// ── 内部：校验转移合法性后执行动作 ──────────────────────────────

fn transition(
    db: &Db,
    id: i64,
    to: MatterStatus,
    actor: &str,
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
    action(id)?;
    // 命题4/7：状态转移全部留痕（open→in_progress / →awaiting_verification / 四态死亡）
    crate::db::repositories::matters::insert_event(
        db,
        id,
        to.as_str(),
        from.as_str(),
        to.as_str(),
        "",
        actor,
    )?;
    Ok(())
}

/// 命题3 收口：母项 completed 时，子项声明的可加性关系必须与实际结果吻合，否则记信号。
fn check_additivity_on_complete(db: &Db, parent_id: i64) -> Result<()> {
    let children = crate::db::repositories::matters::list_children(db, parent_id)?;
    if children.is_empty() {
        return Ok(());
    }
    let mut any_completed = false;
    let mut has_any_decl = false;
    for child in &children {
        if child.status == "completed" {
            any_completed = true;
        }
        match AdditivityDecl::parse(&child.additivity_decl) {
            Ok(AdditivityDecl::AllCompleted) => {
                if child.status != "completed" {
                    record_signal(
                        db,
                        parent_id,
                        "additivity_violation",
                        format!(
                            "子事项 {} 声明 all_completed 但实际 {}",
                            child.id, child.status
                        ),
                    )?;
                }
            }
            Ok(AdditivityDecl::AnyCompleted) => {
                has_any_decl = true;
            }
            // 老数据/空声明：不判定（兼容旧库，缺声明不追加信号）
            Err(_) => {}
        }
    }
    if has_any_decl && !any_completed {
        record_signal(
            db,
            parent_id,
            "additivity_violation",
            "存在 any_completed 声明的子项，但没有任何子项 completed".into(),
        )?;
    }
    Ok(())
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
        let id = create(
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
            "让协作者能独立跑工具循环",
            "",
        )
        .unwrap();
        // 显式授权执行/验证/终止三个决策点给对应 agent（choose/path 保持人类保留）
        delegate(db, id, "ID:000001", DecisionPoint::Execute, true).unwrap();
        delegate(db, id, "ID:000001", DecisionPoint::Verify, true).unwrap();
        delegate(db, id, "ID:000001", DecisionPoint::Terminate, true).unwrap();
        id
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
            "",
            "",
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
            "",
            "",
        )
        .unwrap_err();
        assert!(err.to_string().contains("三主体分离"));
    }

    #[test]
    fn delegation_defaults_all_false() {
        let db = test_db();
        let id = create(
            &db,
            "x",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            Some("codex"),
            Some("jarvis"),
            None,
            "意图",
            "",
        )
        .unwrap();
        let row = crate::db::repositories::matters::get(&db, id).unwrap().unwrap();
        assert_eq!(delegation_map(&row), DelegationMap::default());
        // 人类发起人始终放行
        assert!(decision_allowed(&db, id, "ID:000001", DecisionPoint::Execute).unwrap());
    }

    #[test]
    fn unauthorized_agent_rejected_on_all_decision_points() {
        let db = test_db();
        let id = create(
            &db,
            "x",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            Some("codex"),
            Some("jarvis"),
            None,
            "",
            "",
        )
        .unwrap();
        // 未授权 execute：执行者不能开工
        assert!(start(&db, id, "codex").unwrap_err().to_string().contains("未获授权"));

        // 授权 execute 后走到 awaiting_verification，再分别验证 verify/terminate 门禁
        delegate(&db, id, "ID:000001", DecisionPoint::Execute, true).unwrap();
        start(&db, id, "codex").unwrap();
        submit_evidence(&db, id, "codex", "ev").unwrap();

        // 未授权 verify：验证者不能通过（状态合法，被委托门禁拦下）
        let err = verify(&db, id, "jarvis").unwrap_err();
        assert!(err.to_string().contains("未获授权"), "got: {err}");

        // 未授权 terminate：执行者不能取消
        let err2 = cancel(&db, id, "codex").unwrap_err();
        assert!(err2.to_string().contains("未获授权"), "got: {err2}");
    }

    #[test]
    fn delegate_requires_creator_and_grants_flow() {
        let db = test_db();
        let id = create(
            &db,
            "x",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            Some("codex"),
            Some("jarvis"),
            None,
            "",
            "",
        )
        .unwrap();
        // 非发起人不能改委托地图
        let err = delegate(&db, id, "codex", DecisionPoint::Execute, true).unwrap_err();
        assert!(err.to_string().contains("只有发起人"));
        // 发起人授权后，执行者放行
        delegate(&db, id, "ID:000001", DecisionPoint::Execute, true).unwrap();
        start(&db, id, "codex").unwrap();
        // 收回后再次拒绝
        delegate(&db, id, "ID:000001", DecisionPoint::Execute, false).unwrap();
        assert!(start(&db, id, "codex").unwrap_err().to_string().contains("非法状态转移"));
    }

    #[test]
    fn happy_path_lifecycle() {
        let db = test_db();
        let id = seed(&db);

        start(&db, id, "codex").unwrap();
        assert!(start(&db, id, "codex").unwrap_err().to_string().contains("非法状态转移"));

        submit_evidence(&db, id, "codex", "工具调用 trace 已落库").unwrap();
        assert!(
            submit_evidence(&db, id, "codex", "再次提交")
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
        start(&db, id, "codex").unwrap();
        submit_evidence(&db, id, "codex", "ev").unwrap();
        let err = verify(&db, id, "codex").unwrap_err();
        assert!(err.to_string().contains("验证者必须是登记的 jarvis"));
    }

    #[test]
    fn cancel_and_shelve_register_death() {
        let db = test_db();
        let a = seed(&db);
        cancel(&db, a, "codex").unwrap();
        let row = crate::db::repositories::matters::get(&db, a).unwrap().unwrap();
        assert_eq!(row.status, "cancelled");
        assert_eq!(row.death_reason, "cancelled");

        let b = seed(&db);
        start(&db, b, "codex").unwrap();
        shelve(&db, b, "codex").unwrap();
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
            "",
            "all_completed",
        )
        .unwrap();
        assert!(!can_close(&db, parent).unwrap());

        // 子事项全部终态后父可关闭
        let kids = crate::db::repositories::matters::list_children(&db, parent).unwrap();
        for k in &kids {
            cancel(&db, k.id, "ID:000001").unwrap();
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
            "",
            "",
        )
        .unwrap_err();
        assert!(err.to_string().contains("父事项不存在"));
    }

    #[test]
    fn child_requires_additivity_decl() {
        let db = test_db();
        let parent = seed(&db);
        // 声明缺失 → 拒绝
        let err = create(
            &db,
            "子",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            None,
            None,
            Some(parent),
            "",
            "  ",
        )
        .unwrap_err();
        assert!(err.to_string().contains("可加性声明"));
        // 未知声明值 → 拒绝
        let err2 = create(
            &db,
            "子",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            None,
            None,
            Some(parent),
            "",
            "half_completed",
        )
        .unwrap_err();
        assert!(err2.to_string().contains("未知可加性声明"));
    }

    #[test]
    fn intent_original_persisted_and_drift_reported() {
        let db = test_db();
        let id = create(
            &db,
            "接入协作者",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            Some("codex"),
            Some("jarvis"),
            None,
            "让协作者独立跑通工具循环",
            "",
        )
        .unwrap();
        let row = crate::db::repositories::matters::get(&db, id).unwrap().unwrap();
        assert_eq!(row.intent_original, "让协作者独立跑通工具循环");

        // 理解漂移：原=做A 理解=做B → 未对齐，落信号
        let v = intent_drift_report(&db, id, "让协作者独立跑通消息回复", "让协作者独立跑通消息回复").unwrap();
        assert!(!v.aligned);
        assert!(v.report.contains("理解漂移"));
        let row = crate::db::repositories::matters::get(&db, id).unwrap().unwrap();
        assert!(row.signals.contains("intent_drift"), "signals: {}", row.signals);

        // 完全对齐 → aligned
        let v2 = intent_drift_report(&db, id, "让协作者独立跑通工具循环", "让协作者独立跑通工具循环").unwrap();
        assert!(v2.aligned);
    }

    #[test]
    fn additivity_violation_signals_on_parent_complete() {
        let db = test_db();
        let parent = seed(&db);
        // 子1：声明 all_completed 且正常完成
        let child = create(
            &db,
            "子1",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            Some("codex"),
            Some("jarvis"),
            Some(parent),
            "",
            "all_completed",
        )
        .unwrap();
        delegate(&db, child, "ID:000001", DecisionPoint::Execute, true).unwrap();
        delegate(&db, child, "ID:000001", DecisionPoint::Verify, true).unwrap();
        start(&db, child, "codex").unwrap();
        submit_evidence(&db, child, "codex", "ev").unwrap();
        verify(&db, child, "jarvis").unwrap();

        // 子2：声明 all_completed 但被取消 → 母项关闭时应记违反信号
        let bad = create(
            &db,
            "子2",
            "e",
            "c",
            "g",
            "crit",
            "ID:000001",
            Some("codex"),
            Some("jarvis"),
            Some(parent),
            "",
            "all_completed",
        )
        .unwrap();
        cancel(&db, bad, "ID:000001").unwrap();

        start(&db, parent, "codex").unwrap();
        submit_evidence(&db, parent, "codex", "全部子项已处理").unwrap();
        verify(&db, parent, "jarvis").unwrap();

        let prow = crate::db::repositories::matters::get(&db, parent).unwrap().unwrap();
        assert!(
            prow.signals.contains("additivity_violation"),
            "signals: {}",
            prow.signals
        );
    }

    // ── M5：验证者分离 + 事件留痕 + 幽灵检测（命题4/7） ──

    #[test]
    fn verifier_defaults_to_executor_self_verified() {
        let db = test_db();
        let id = create(
            &db, "自证事项", "e", "c", "g", "crit", "ID:000001", Some("codex"), None, None, "", "",
        )
        .unwrap();
        delegate(&db, id, "ID:000001", DecisionPoint::Execute, true).unwrap();
        delegate(&db, id, "ID:000001", DecisionPoint::Verify, true).unwrap();
        start(&db, id, "codex").unwrap();
        submit_evidence(&db, id, "codex", "证据").unwrap();
        verify(&db, id, "codex").unwrap();

        let row = crate::db::repositories::matters::get(&db, id).unwrap().unwrap();
        assert_eq!(row.status, "completed");
        assert!(row.self_verified, "执行者自证应落 self_verified=true");
        assert!(row.signals.contains("self_verified"), "signals: {}", row.signals);
        let events = crate::db::repositories::matters::list_events(&db, id).unwrap();
        assert!(events.iter().any(|e| e.event_type == "completed"), "应记录 completed 事件");
    }

    #[test]
    fn explicit_verifier_not_self_verified() {
        let db = test_db();
        let id = create(
            &db, "独立验证", "e", "c", "g", "crit", "ID:000001", Some("codex"), Some("jarvis"), None, "", "",
        )
        .unwrap();
        delegate(&db, id, "ID:000001", DecisionPoint::Execute, true).unwrap();
        delegate(&db, id, "ID:000001", DecisionPoint::Verify, true).unwrap();
        start(&db, id, "codex").unwrap();
        submit_evidence(&db, id, "codex", "证据").unwrap();
        verify(&db, id, "jarvis").unwrap();

        let row = crate::db::repositories::matters::get(&db, id).unwrap().unwrap();
        assert_eq!(row.status, "completed");
        assert!(!row.self_verified, "独立验证者完成不应标记 self_verified");
    }

    #[test]
    fn self_verify_requires_executor() {
        let db = test_db();
        let id = create(
            &db, "自证越权", "e", "c", "g", "crit", "ID:000001", Some("codex"), None, None, "", "",
        )
        .unwrap();
        delegate(&db, id, "ID:000001", DecisionPoint::Verify, true).unwrap();
        let err = verify(&db, id, "stranger").unwrap_err();
        assert!(err.to_string().contains("自证"), "err: {err}");
    }

    #[test]
    fn death_events_recorded_for_all_terminal_states() {
        let db = test_db();
        let c1 = seed(&db);
        cancel(&db, c1, "ID:000001").unwrap();
        let c2 = seed(&db);
        shelve(&db, c2, "ID:000001").unwrap();

        let ev1 = crate::db::repositories::matters::list_events(&db, c1).unwrap();
        assert!(ev1.iter().any(|e| e.event_type == "cancelled"), "缺 cancelled 事件");
        let ev2 = crate::db::repositories::matters::list_events(&db, c2).unwrap();
        assert!(ev2.iter().any(|e| e.event_type == "shelved"), "缺 shelved 事件");
    }

    #[test]
    fn expire_records_event_with_system_actor() {
        let db = test_db();
        let id = seed(&db);
        db.conn()
            .execute(
                "UPDATE matters SET updated_at = '2020-01-01 00:00:00' WHERE id = ?1",
                (id,),
            )
            .unwrap();
        let dead = expire_stale(&db, "2021-01-01 00:00:00").unwrap();
        assert!(dead.contains(&id), "应过期事项 {id}");
        let events = crate::db::repositories::matters::list_events(&db, id).unwrap();
        let ev = events
            .iter()
            .find(|e| e.event_type == "expired")
            .expect("缺 expired 事件");
        assert_eq!(ev.actor, "system");
    }

    #[test]
    fn ghost_detected_when_orphan_idle_no_criteria() {
        let db = test_db();
        let id = create(
            &db, "幽灵", "e", "c", "g", "crit", "ID:000001", None, None, None, "", "",
        )
        .unwrap();
        // 模拟老数据：无验收判据 + 长时间未动
        db.conn()
            .execute(
                "UPDATE matters SET acceptance_criteria = '', updated_at = '2020-01-01 00:00:00' WHERE id = ?1",
                (id,),
            )
            .unwrap();
        let ghosts = detect_ghosts(&db, "2021-01-01 00:00:00").unwrap();
        assert_eq!(ghosts.len(), 1, "应识别 1 个幽灵事项: {ghosts:?}");
        assert_eq!(ghosts[0].id, id);
        let row = crate::db::repositories::matters::get(&db, id).unwrap().unwrap();
        assert!(row.signals.contains("ghost_candidate"), "signals: {}", row.signals);
    }

    #[test]
    fn ghost_excludes_owned_fresh_or_with_criteria() {
        let db = test_db();
        // 有主 + 老 + 无判据 → 不算幽灵（有主）
        let owned = create(
            &db, "有主", "e", "c", "g", "crit", "ID:000001", Some("codex"), None, None, "", "",
        )
        .unwrap();
        db.conn()
            .execute(
                "UPDATE matters SET acceptance_criteria = '', updated_at = '2020-01-01 00:00:00' WHERE id = ?1",
                (owned,),
            )
            .unwrap();
        // 无主 + 新 + 无判据 → 不算幽灵（有进展）
        let fresh = create(
            &db, "活跃", "e", "c", "g", "crit", "ID:000001", None, None, None, "", "",
        )
        .unwrap();
        db.conn()
            .execute(
                "UPDATE matters SET acceptance_criteria = '' WHERE id = ?1",
                (fresh,),
            )
            .unwrap();
        // 无主 + 老 + 有判据 → 不算幽灵（有验收判据）
        let crit = create(
            &db, "有判据", "e", "c", "g", "crit", "ID:000001", None, None, None, "", "",
        )
        .unwrap();
        db.conn()
            .execute("UPDATE matters SET updated_at = '2020-01-01 00:00:00' WHERE id = ?1", (crit,))
            .unwrap();

        let ghosts = detect_ghosts(&db, "2021-01-01 00:00:00").unwrap();
        assert!(ghosts.is_empty(), "不应识别任何幽灵: {ghosts:?}");
    }
}
