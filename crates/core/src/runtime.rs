//! runtime 意识主循环骨架（M2 阶段）：消息进入 → injector → 归属 → 落库 → 渲染 `<context>`。
//!
//! 对齐 Node `index.js` runTurn 的线程归属段（756-845）+ 行动者声明（1418-1425）。
//! 本骨架覆盖归属段闭环 + 注入编排闭包 [`run_user_turn`]（process_message → run_injector
//! → format_context_block）；不含 LLM 调用 / 工具循环 / buildMessagesWithContext——
//! 它们是 runTurn 后半段，由后续阶段接入（调用方在注入闭包之后继续完成 LLM 轮）。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::models::Thread;
use crate::db::repositories::conversations::update_user_message_focus_topic;
use crate::db::repositories::threads::{save_state, ThreadState};
use crate::db::Db;
use crate::embedding::Embedder;
use crate::error::Result;
use crate::memory::injector::{
    run_injector, ContextWindowConfig, InjectorContext, InjectorInput, InjectorOutput,
};
use crate::memory::injector_format::{format_context_block, ContextRender};
use crate::memory::messages::{
    build_llm_messages, BuildLlmMessagesArgs, CurrentMessage, LlmMessage,
};
use crate::memory::retrieval::parse_message_input;
use crate::memory::system_prompt::SystemPromptArgs;
use crate::memory::threads::{
    attribute_user_message, build_thread_view, get_foreground_thread, get_thread_by_id,
    init_thread_state, latest_open_commitment, touch_commitment_thread, Attribution,
    AttributionKind,
};

/// 主循环进程内状态（对齐 Node `state` + db.js 的 currentFocusTopic/currentThreadId 写时印章）。
#[derive(Debug, Clone, Default)]
pub struct RuntimeState {
    pub thread_state: ThreadState,
    /// 自启动起的 TICK 轮计数（对齐 `state.tickCounter`；仅在 TICK 轮递增）
    pub tick_counter: i64,
    /// 写时归属印章：focus_topic（对齐 `currentFocusTopic`，缺省注入到后续 insertConversation）
    pub focus_topic: String,
    /// 写时归属印章：thread_id（对齐 `currentThreadId`）
    pub current_thread_id: String,
}

impl RuntimeState {
    /// 当前写时印章 focus_topic（插入对话缺省归属用）。
    pub fn focus_topic(&self) -> &str {
        &self.focus_topic
    }

    /// 当前写时印章 thread_id。
    pub fn current_thread_id(&self) -> &str {
        &self.current_thread_id
    }
}

/// 单轮归属处理结果（对齐 runTurn 归属段输出的关键量）。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    /// 本轮是否为 TICK（自主干活轮）。
    pub is_tick: bool,
    /// 归属判定结果（仅非 TICK；TICK 不走判定，恒为 None）。
    pub attribution: Option<Attribution>,
    /// 印章线程（stampThread）：非 TICK 为前台线索；TICK 为最近开放承诺线索（缺省回前台）。
    pub stamp_thread_id: Option<String>,
    /// 稳定焦点串（`stableFocusTopic(stampThread)`；空串 = 无稳定焦点）。
    pub stamp_focus_topic: String,
    /// 归属事件名（对齐 `threadResult.event`）：`created` / `continued` / `resumed` / `noop`。
    pub event: &'static str,
}

/// 启动恢复主循环状态（对齐 Node 启动时 initThreadState + tickCounter 归零）。
pub fn init(db: &Db) -> Result<RuntimeState> {
    let thread_state = init_thread_state(db, 0)?;
    Ok(RuntimeState {
        thread_state,
        ..Default::default()
    })
}

/// 处理一条进入主循环的消息（对齐 runTurn 归属段 756-845）。
///
/// 消息格式与 `parse_message_input` 约定一致：
/// `[ID:xxxxxx] 2026-04-13 10:00:00 [渠道] 内容` 或 `TICK 2026-04-13-10:00:00`。
/// `channel` 为外部渠道名（对齐 `normalizeChannel(msg.channel)`；无渠道传空串）。
///
/// 职责边界（与 Node 一致）：
/// - 用户消息在进入本函数前已由调用方写库（pushMessage 阶段），本函数只做归属判定与回填，
///   不重复写对话；
/// - TICK 轮：tick_counter 递增、印章到开放承诺线索，不落库（Node 中 TICK 的 threadState
///   持久化发生在工具循环后的 touchCommitmentThread，见 [`touch_open_commitment`]）；
/// - 非 TICK：归属判定 → 事件非 noop 时 save_state 落库 → 回填触发判定的 user 对话行；
/// - 弱信号（`ambiguous_with`）与前台切换（`switched_from`）只记录在结果里，由调用方决定
///   是否触发 LLM 事后仲裁 / 后台摘要（本骨架不含 LLM）。
pub fn process_message(
    db: &Db,
    state: &mut RuntimeState,
    input: &str,
    channel: &str,
) -> Result<TurnOutcome> {
    let parsed = parse_message_input(input);

    // 0) TICK 计数：仅 TICK 轮递增（index.js 688）
    if parsed.is_tick {
        state.tick_counter += 1;
    }

    // 1) 归属判定（仅非 TICK；TICK 恒 noop——TICK 永不参与判定）
    let attribution = if parsed.is_tick {
        None
    } else {
        Some(attribute_user_message(
            &mut state.thread_state,
            input,
            state.tick_counter,
            channel,
        ))
    };

    // 2) 印章线程（index.js 778-783）：非 TICK → 前台；TICK → 最近开放承诺线索，缺省回前台。
    //    Agent 自主干活本身就是注意力事件。
    let stamp_thread = if parsed.is_tick {
        latest_open_commitment(&state.thread_state, channel)
            .and_then(|c| get_thread_by_id(&state.thread_state, &c.thread_id).cloned())
            .or_else(|| get_foreground_thread(&state.thread_state).cloned())
    } else {
        get_foreground_thread(&state.thread_state).cloned()
    };

    let stamp_focus_topic = stable_focus_topic(stamp_thread.as_ref());
    let stamp_thread_id = stamp_thread.as_ref().map(|t| t.id.clone());

    // 3) 写时归属印章（index.js 784-786，对齐 setCurrentFocusTopic / setCurrentThreadId）
    state.focus_topic = stamp_focus_topic.clone();
    state.current_thread_id = stamp_thread_id.clone().unwrap_or_default();

    // 4) 落库
    let mut event = "noop";
    if let Some(a) = attribution.as_ref() {
        event = event_name(&a.kind);
        if a.kind != AttributionKind::Noop {
            save_state(db, &state.thread_state, None)?;
        }
        // 回填触发判定的 user 对话行（index.js 787-789）：pushMessage 阶段写库时 focus_topic
        // 为空，用 (from_id, timestamp) 精确定位回填；失败静默（Node catch{} 吞掉）。
        if let (Some(from_id), Some(ts), Some(tid)) = (
            parsed.sender_id.as_ref(),
            extract_timestamp(input).as_ref(),
            stamp_thread_id.as_ref(),
        ) {
            let _ = update_user_message_focus_topic(db, from_id, ts, &stamp_focus_topic, Some(tid));
        }
    }

    Ok(TurnOutcome {
        is_tick: parsed.is_tick,
        attribution,
        stamp_thread_id,
        stamp_focus_topic,
        event,
    })
}

/// 行动者声明：TICK 自主干活后触碰开放承诺线索（对齐 index.js 1418-1425）。
///
/// 调用方应在工具调用日志非空时调用；返回本轮是否有承诺状态变化（true 表示已落库）。
pub fn touch_open_commitment(db: &Db, state: &mut RuntimeState) -> Result<bool> {
    let touched = touch_commitment_thread(&mut state.thread_state, state.tick_counter);
    if touched {
        save_state(db, &state.thread_state, None)?;
    }
    Ok(touched)
}

// ── 编排闭包（注入里程碑）：process_message → run_injector → format_context_block ──────

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 从 `run_user_turn` 已有信号投影 `SystemPromptArgs` 并构建 system prompt。
///
/// 对齐 Node index.js 1000-1017 的 buildSystemPrompt 调用面；本骨架尚无法投影的
/// 信号位（persona / birthTime / countryCode / timezone / hasWechatHistory /
/// hasActiveFocus / recentActionsSummary）按默认值处理（Node 端 hasWechatHistory /
/// hasActiveFocus 目前也是写死 false）。调用方若有这些字段，可自行构造
/// [`SystemPromptArgs`] 调用 [`build_system_prompt`] 并作为 `system_prompt` 传入。
#[allow(clippy::too_many_arguments)]
fn build_default_system_prompt(
    agent_name: &str,
    input: &str,
    channel: &str,
    has_active_task: bool,
    task: Option<&str>,
    tools: &[String],
    is_tick: bool,
    available_agents: &[crate::db::models::KnownAgent],
    delegation_allowed: bool,
) -> String {
    crate::memory::system_prompt::build_system_prompt(&SystemPromptArgs {
        agent_name,
        persona: "",
        birth_time: "",
        // 骨架阶段 msg 未传 → input 即当前轮正文（与合成 user 消息保持一致）
        user_message: input,
        current_channel: channel,
        has_wechat_history: false,
        has_active_focus: false,
        has_active_task,
        current_country_code: "",
        current_timezone: "",
        current_tools: tools,
        current_task_text: task.unwrap_or(""),
        recent_actions_summary: "",
        is_voice_turn: channel.eq_ignore_ascii_case("VOICE"),
        is_tick,
        available_agents,
        delegation_allowed,
    })
}

/// 一轮注入闭包的结果：归属 + injector 输出 + `<context>` 块 + 组装好的 LLM 消息。
#[derive(Debug, Clone)]
pub struct InjectedTurn {
    pub outcome: TurnOutcome,
    pub injection: InjectorOutput,
    pub context_block: String,
    /// buildLLMMessages 产物：system → [runtime context] → 会话历史 → 当前轮。
    pub llm_messages: Vec<LlmMessage>,
}

/// 一轮完整注入闭包（对齐 Node runTurn：749 runInjector → 756 归属 → 1073 buildContextBlock
/// → 1089 buildMessagesWithContext/buildLLMMessages）。
///
/// 时序说明：Node 里 runInjector 先于归属判定执行，但两者解耦——run_injector 只读 DB，
/// process_message 只改 `state.thread_state` 内存态 + 落库，互不冲突；这里先跑归属
/// （对齐 Node pushMessage 先落库的时序），再跑 injector，最后渲染 `<context>` 并组装消息。
///
/// `system_prompt` 由调用方渲染；传空串时按 [`build_default_system_prompt`] 用
/// `buildSystemPrompt` 信号投影自动生成（Node index.js 1000-1017 对齐）；
/// `msg` 为当前轮 user 行（pushMessage 已落库时的行投影；None 时按 input 合成 user 消息）；
/// `has_active_task` / `task` 由调用方从 `state.task` 投影（本骨架不含任务管理器）；
/// person / task-knowledge / constraints / policies 等外部子系统结果经 `InjectorContext` 注入。
/// 返回后调用方只需把 `llm_messages` 交给 LLM 并处理工具循环。
/// 参数个数对齐 Node runTurn 的调用面（上下文注入项较多），故豁免 too_many_arguments。
#[allow(clippy::too_many_arguments)]
pub async fn run_user_turn(
    db: &Db,
    embedder: &dyn Embedder,
    state: &mut RuntimeState,
    input: &str,
    channel: &str,
    input_hint: &str,
    ctx: &InjectorContext,
    window: &ContextWindowConfig,
    agent_name: &str,
    has_active_task: bool,
    task: Option<&str>,
    system_prompt: &str,
    msg: Option<CurrentMessage>,
) -> Result<InjectedTurn> {
    // 1) 归属判定 + 写时印章 + 落库 + 回填
    let outcome = process_message(db, state, input, channel)?;

    // 2) injector 编排（async；内部 best-effort，不返回 Err）
    let mut injection = run_injector(
        db,
        embedder,
        &InjectorInput {
            message: input.to_string(),
            hint: input_hint.to_string(),
            current_channel: channel.to_string(),
            task: task.map(str::to_string),
            prev_recall: None,
            confidence_hint: None,
            last_tool_result: None,
        },
        ctx,
        window,
        agent_name,
    )
    .await;

    // 2.5) TICK 轮注入一次性本地 Agent 发现（对齐 Node index.js 858：
    //      directions.unshift(buildAutonomousTickDirections({ delegationDiscovery:
    //      buildDelegationDiscoveryContext() || '', ... })) —— tick-policy 其余文本
    //      尚未迁移，此处仅注入发现块；delegation_discovery 内部负责 mark
    //      `agent_delegation_asked`，首个 TICK 注入后不再重复出现。
    //      发现文本并入 <directions>（insert(0) 对齐 Node 的 unshift 顺序）。
    if outcome.is_tick {
        if let Some(discovery) = crate::agents::delegation_discovery(db) {
            injection.directions.insert(0, discovery);
        }
    }

    // 2.6) 能力预喂（weather）：对齐 Node runCapabilityPrefeed → weatherContextText。
    //      Node 对每个有 prefeed 的能力跑 buildXxxRuntimeContext；Rust 已迁 weather，
    //      其余能力数据源未迁。weather 内部自带关键词门 + 位置解析 + 30min 缓存，
    //      抓取失败返回空串（不注入、不阻断本轮）。
    if ctx.enable_weather_prefeed && injection.weather_runtime_text.is_none() {
        let text = crate::memory::weather::build_weather_runtime_context(input, db).await;
        if !text.is_empty() {
            injection.weather_runtime_text = Some(text);
        }
    }

    // 2.7) 自我进化：把本轮检索到的策略记忆沉淀进 self_evolution 状态
    //      （对齐 Node index.js:292 recordSelfEvolutionFromMemories；best-effort）。
    {
        let recall: Vec<crate::db::models::Memory> = injection
            .memories
            .iter()
            .map(|s| s.memory.clone())
            .collect();
        if let Err(err) =
            crate::memory::self_evolution::record_self_evolution_from_memories(db, &recall)
        {
            tracing::warn!(%err, "self-evolution record failed (skipped)");
        }
    }
    // 2.8) 自我进化上下文文本（对齐 injector.js:269 formatSelfEvolutionForPrompt：
    //      TICK 轮 3 条、用户轮 5 条）；输入侧已提供文本时不重复计算。
    if injection.self_evolution.trim().is_empty() {
        let max_recent = if outcome.is_tick { 3 } else { 5 };
        injection.self_evolution =
            crate::memory::self_evolution::format_self_evolution_for_prompt(db, max_recent);
    }

    // 3) 渲染 <context> 块（线程视图 + injection；对齐 buildContextBlock）
    let thread_view = build_thread_view(&state.thread_state, now_ms());
    let context_block = format_context_block(&ContextRender {
        thread_view: Some(&thread_view),
        injection: &injection,
        has_active_task,
        task,
    });

    // 4) 组装 LLM 消息（对齐 buildMessagesWithContext → buildLLMMessages）；
    //    骨架阶段 recent_actions / task_steps / battery 未移植 → 空；lastToolResult 留工具循环接线。
    let system_prompt = if system_prompt.trim().is_empty() {
        // 信号位投影 + AI Collaborators 块所需的 Agent 数据（对齐 Node buildSystemPrompt
        // 内部 buildAgentContextBlock 查库；本骨架在编排闭包内查一次）
        let available_agents = crate::db::repositories::agents::get_available_agents(db)?;
        let delegation_allowed = crate::db::repositories::agents::is_delegation_allowed(db)?;
        build_default_system_prompt(
            agent_name,
            input,
            channel,
            has_active_task,
            task,
            &injection.tools,
            outcome.is_tick,
            &available_agents,
            delegation_allowed,
        )
    } else {
        system_prompt.to_string()
    };
    let llm_messages = build_llm_messages(BuildLlmMessagesArgs {
        system_prompt,
        context_block: context_block.clone(),
        conversation_window: injection.conversation_window.clone(),
        input: input.to_string(),
        msg,
        recent_actions: Vec::new(),
        action_log: injection.action_log.clone(),
        last_tool_result: None,
        task_steps: Vec::new(),
        battery_block: String::new(),
        current_topic: outcome.stamp_focus_topic.clone(),
        is_tick: outcome.is_tick,
    });

    Ok(InjectedTurn {
        outcome,
        injection,
        context_block,
        llm_messages,
    })
}

/// 稳定焦点串（对齐 stableFocusTopic，index.js 511-517）：
/// topic 为空 → ''；hit_count < 2 且无结论 → ''；否则 topic 前 3 个 join ','。
fn stable_focus_topic(thread: Option<&Thread>) -> String {
    let Some(t) = thread else {
        return String::new();
    };
    if t.topic.is_empty() {
        return String::new();
    }
    if t.hit_count < 2 && t.conclusions.is_empty() {
        return String::new();
    }
    t.topic
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(",")
}

/// 归属事件名（对齐 Node `threadResult.event` 字符串）。
fn event_name(kind: &AttributionKind) -> &'static str {
    match kind {
        AttributionKind::Created => "created",
        AttributionKind::Continued => "continued",
        AttributionKind::Resumed => "resumed",
        AttributionKind::Noop => "noop",
    }
}

/// 从消息 envelope 提取时间戳（`[ID:xxx] 2026-04-13 10:00:00 [渠道] 内容` → `2026-04-13 10:00:00`）。
/// 对齐 Node `msg.timestamp`：回填 UPDATE 用等值匹配，提取值必须与写库时一致（由上层保证）。
fn extract_timestamp(input: &str) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // 与 insertConversation 的 timestamp（time.js nowTimestamp 格式，
        // 如 `2026-08-09T10:00:00+08:00`）保持一致，才能命中回填 UPDATE。
        regex::Regex::new(r"(?s)^\[[^\]]+\]\s*([\d\-T:+]+)").expect("static regex")
    });
    re.captures(input.trim())
        .and_then(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{Memory, Thread};
    use crate::db::open_database;
    use crate::db::repositories::conversations::{insert_conversation, recent_by_from};
    use crate::db::repositories::threads::Commitment;
    use crate::memory::messages::LlmRole;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        open_database(path).unwrap()
    }

    /// 构造测试记忆（其余字段取空默认；与 injector_format 测试的 mem() 同形）。
    fn mem(event_type: &str, content: &str, entities: Vec<&str>) -> Memory {
        Memory {
            id: 0,
            event_type: event_type.into(),
            content: content.into(),
            detail: String::new(),
            title: String::new(),
            mem_id: None,
            entities: entities.into_iter().map(str::to_string).collect(),
            concepts: Vec::new(),
            tags: Vec::new(),
            links: Vec::new(),
            salience: 8,
            source_ref: None,
            timestamp: "2026-08-09T10:00:00.000Z".into(),
            parent_id: None,
            embedding: None,
            visibility: true,
            hidden_at: None,
            merged_into: None,
            embedding_dim: None,
            embedding_model: None,
            created_at: "2026-08-09T10:00:00.000Z".into(),
        }
    }

    /// 写入一条用户会话行（模拟 pushMessage 落库；run_user_turn 本身不写对话）。
    fn push_user_message(db: &Db, from_id: &str, content: &str, ts: &str) {
        insert_conversation(
            db,
            &crate::db::models::NewConversation {
                role: "user".into(),
                from_id: from_id.into(),
                to_id: None,
                content: content.into(),
                timestamp: ts.into(),
                channel: "TUI".into(),
                external_party_id: String::new(),
                focus_topic: String::new(),
                open_question: false,
                thread_id: String::new(),
                delivery_status: "delivered".into(),
            },
        )
        .unwrap();
    }

    fn make_thread(id: &str) -> Thread {
        Thread {
            id: id.into(),
            topic: vec!["前端轮子".into()],
            signature: vec!["重构".into()],
            label: String::new(),
            summary: String::new(),
            conclusions: Vec::new(),
            status: "open".into(),
            created_at: "2026-08-09 10:00:00".into(),
            last_event_at: "2026-08-09 10:00:00".into(),
            last_event_tick: 0,
            hit_count: 2,
            last_summary_at: String::new(),
            updated_at: "2026-08-09 10:00:00".into(),
        }
    }

    fn open_commitment(thread_id: &str) -> Commitment {
        Commitment {
            id: "c1".into(),
            thread_id: thread_id.into(),
            text: "把前端轮子重做一遍".into(),
            status: "open".into(),
            channel: "TUI".into(),
            created_at: "2026-08-09 10:00:00".into(),
            closed_at: None,
        }
    }

    #[test]
    fn init_on_empty_db_produces_default_state() {
        let db = test_db();
        let state = init(&db).unwrap();
        assert_eq!(state.tick_counter, 0);
        assert_eq!(state.focus_topic, "");
        assert_eq!(state.current_thread_id, "");
        assert!(state.thread_state.threads.is_empty());
        assert!(state.thread_state.foreground_id.is_none());
    }

    #[test]
    fn user_message_creates_thread_and_persists() {
        let db = test_db();
        let mut state = init(&db).unwrap();
        let input =
            "[ID:000001] 2026-08-09T10:00:00+08:00 [TUI] 帮我把前端轮子重做一遍，要支持多端复用";
        let out = process_message(&db, &mut state, input, "TUI").unwrap();

        assert!(!out.is_tick);
        assert_eq!(out.event, "created");
        let stamp_id = out.stamp_thread_id.expect("created 应有印章线程");
        assert!(!stamp_id.is_empty());
        // hit_count=1 且无结论 → 稳定焦点为空（对齐 stableFocusTopic）
        assert_eq!(out.stamp_focus_topic, "");
        assert_eq!(state.current_thread_id(), stamp_id);
        assert_eq!(state.focus_topic(), "");

        // 事件非 noop → 已落库：重启恢复后前台线索仍在
        let restored = init(&db).unwrap();
        assert_eq!(
            restored.thread_state.foreground_id.as_deref(),
            Some(stamp_id.as_str())
        );
        assert_eq!(restored.thread_state.threads.len(), 1);
    }

    #[test]
    fn short_leaf_message_is_noop_and_not_persisted() {
        let db = test_db();
        let mut state = init(&db).unwrap();
        let out = process_message(
            &db,
            &mut state,
            "[ID:000001] 2026-08-09T10:00:00+08:00 [TUI] 好",
            "TUI",
        )
        .unwrap();
        assert_eq!(out.event, "noop");
        assert_eq!(out.stamp_thread_id, None);
        assert_eq!(out.stamp_focus_topic, "");
        // 不落库：重启后无任何线程
        let restored = init(&db).unwrap();
        assert!(restored.thread_state.threads.is_empty());
    }

    #[test]
    fn tick_round_increments_counter_and_stamps_commitment_thread() {
        let db = test_db();
        let mut state = init(&db).unwrap();
        // 预置一条开放承诺线索（模拟上次用户轮的产出）
        state.thread_state.threads.push(make_thread("th_commit"));
        state.thread_state.foreground_id = Some("th_commit".into());
        state
            .thread_state
            .commitments
            .push(open_commitment("th_commit"));

        let out = process_message(&db, &mut state, "TICK 2026-08-09-10:30:00", "").unwrap();
        assert!(out.is_tick);
        assert_eq!(out.attribution, None);
        assert_eq!(out.event, "noop");
        assert_eq!(out.stamp_thread_id.as_deref(), Some("th_commit"));
        // TICK 轮 tick_counter 递增
        assert_eq!(state.tick_counter, 1);

        // TICK 不落库（Node 中由 touch_open_commitment 承担）：此时库里仍无线程
        let restored = init(&db).unwrap();
        assert!(restored.thread_state.threads.is_empty());

        // 无承诺时的 TICK：印章回退到前台线索
        let out2 = process_message(&db, &mut state, "TICK 2026-08-09-10:31:00", "").unwrap();
        assert_eq!(out2.stamp_thread_id.as_deref(), Some("th_commit"));
        assert_eq!(state.tick_counter, 2);
    }

    #[test]
    fn user_message_backfills_focus_and_thread_on_stored_row() {
        let db = test_db();
        let mut state = init(&db).unwrap();
        // pushMessage 阶段先写库（focus_topic 为空、thread_id 为空）——timestamp 与 envelope 一致
        insert_conversation(
            &db,
            &crate::db::models::NewConversation {
                role: "user".into(),
                from_id: "ID:000001".into(),
                to_id: None,
                content: "帮我把前端轮子重做一遍，要支持多端复用".into(),
                timestamp: "2026-08-09T10:00:00+08:00".into(),
                channel: "TUI".into(),
                external_party_id: String::new(),
                focus_topic: String::new(),
                open_question: false,
                thread_id: String::new(),
                delivery_status: String::new(),
            },
        )
        .unwrap();

        let out = process_message(
            &db,
            &mut state,
            "[ID:000001] 2026-08-09T10:00:00+08:00 [TUI] 帮我把前端轮子重做一遍，要支持多端复用",
            "TUI",
        )
        .unwrap();

        let rows = recent_by_from(&db, "ID:000001", 10).unwrap();
        assert_eq!(rows.len(), 1);
        // 回填：thread_id 盖章；focus_topic 因 hit_count=1 无结论为空串
        assert_eq!(rows[0].thread_id, out.stamp_thread_id.unwrap());
        assert_eq!(rows[0].focus_topic, "");
    }

    #[test]
    fn continued_message_accumulates_hit_and_stable_focus() {
        let db = test_db();
        let mut state = init(&db).unwrap();
        let input =
            "[ID:000001] 2026-08-09T10:00:00+08:00 [TUI] 帮我把前端轮子重做一遍，要支持多端复用";
        let first = process_message(&db, &mut state, input, "TUI").unwrap();
        let th_id = first.stamp_thread_id.unwrap();
        // hit_count=1 且无结论 → 无稳定焦点
        assert_eq!(first.stamp_focus_topic, "");
        assert_eq!(state.focus_topic(), "");

        // 第二条同类消息 → continued；hit_count 达到 2 → 稳定焦点出现（内容为提取词表 join）
        let second = process_message(
            &db,
            &mut state,
            "[ID:000001] 2026-08-09T10:05:00+08:00 [TUI] 前端轮子重构记得保留原来的接口",
            "TUI",
        )
        .unwrap();
        assert_eq!(second.event, "continued");
        assert_eq!(second.stamp_thread_id.as_deref(), Some(th_id.as_str()));
        assert!(!second.stamp_focus_topic.is_empty());
        assert_eq!(state.focus_topic(), second.stamp_focus_topic);
        assert_eq!(state.current_thread_id(), th_id);

        // 线程仍在前台、hit_count 累计到 2（stableFocusTopic 门限）
        let thread = get_foreground_thread(&state.thread_state).unwrap();
        assert_eq!(thread.id, th_id);
        assert!(thread.hit_count >= 2);
        // 落库：重启后前台恢复、hit_count 保留
        let restored = init(&db).unwrap();
        let rt = get_foreground_thread(&restored.thread_state).unwrap();
        assert_eq!(rt.id, th_id);
        assert!(rt.hit_count >= 2);
    }

    #[test]
    fn touch_open_commitment_persists_when_state_changed() {
        let db = test_db();
        let mut state = init(&db).unwrap();
        // 无承诺 → false，不落库
        assert!(!touch_open_commitment(&db, &mut state).unwrap());

        // 有开放承诺 → true 并落库
        state.thread_state.threads.push(make_thread("th_commit"));
        state.thread_state.foreground_id = Some("th_commit".into());
        state
            .thread_state
            .commitments
            .push(open_commitment("th_commit"));
        assert!(touch_open_commitment(&db, &mut state).unwrap());

        let restored = init(&db).unwrap();
        assert_eq!(restored.thread_state.threads.len(), 1);
        assert_eq!(restored.thread_state.commitments.len(), 1);
    }

    #[tokio::test]
    async fn run_user_turn_closure_injects_and_renders_context() {
        let db = test_db();
        let mut state = init(&db).unwrap();
        let embedder = crate::embedding::NoopEmbedder;
        let ctx = InjectorContext::default();
        let window = ContextWindowConfig::default();
        let input =
            "[ID:000001] 2026-08-09T10:00:00+08:00 [TUI] 帮我把前端轮子重做一遍，要支持多端复用";

        let turn = run_user_turn(
            &db,
            &embedder,
            &mut state,
            input,
            "TUI",
            "",
            &ctx,
            &window,
            "白马",
            false,
            None,
            "你是白马，一只注重实效的 AI 助理。",
            None,
        )
        .await
        .unwrap();

        // 归属段照常：created + 印章
        assert_eq!(turn.outcome.event, "created");
        assert!(turn.outcome.stamp_thread_id.is_some());
        assert!(!turn.outcome.is_tick);
        // injector 输出存在（空库时 memories 为空）
        assert!(turn.injection.memories.is_empty());
        // 渲染块：<context> 包装 + 线程段 + 无任务占位；空库无 <memories>
        assert!(turn.context_block.starts_with("<context>\n"));
        assert!(turn.context_block.contains("<thread topic=\""));
        assert!(turn.context_block.contains("<task active=\"false\">"));
        assert!(!turn.context_block.contains("<memories>"));
        // LLM 消息：system + [runtime context] + 合成 user
        assert_eq!(turn.llm_messages.len(), 3);
        assert_eq!(turn.llm_messages[0].role.as_str(), "system");
        assert_eq!(
            turn.llm_messages[0].content,
            "你是白马，一只注重实效的 AI 助理。"
        );
        assert_eq!(turn.llm_messages[1].role.as_str(), "user");
        assert!(turn.llm_messages[1]
            .content
            .starts_with("[runtime context]"));
        assert_eq!(turn.llm_messages[2].role.as_str(), "user");
        assert_eq!(turn.llm_messages[2].content, input);
        // 落库：重启恢复前台
        let restored = init(&db).unwrap();
        assert_eq!(restored.thread_state.threads.len(), 1);
    }

    #[tokio::test]
    async fn run_user_turn_renders_recalled_memories_and_active_task() {
        let db = test_db();
        // 预置一条带实体记忆，让 injector 的 sender_memories 命中
        crate::db::repositories::memories::insert_simple(
            &db,
            "fact",
            "用户偏好冷萃咖啡（ID:000001 提到过）",
        )
        .unwrap();
        let mut state = init(&db).unwrap();
        let embedder = crate::embedding::NoopEmbedder;
        let ctx = InjectorContext::default();
        let window = ContextWindowConfig::default();
        let input = "[ID:000001] 2026-08-09T10:00:00+08:00 [TUI] 前端轮子重构记得保留原来的接口";

        let turn = run_user_turn(
            &db,
            &embedder,
            &mut state,
            input,
            "TUI",
            "",
            &ctx,
            &window,
            "白马",
            true,
            Some("重构前端轮子，支持多端复用"),
            "你是白马，一只注重实效的 AI 助理。",
            None,
        )
        .await
        .unwrap();

        // 有任务 → <task active="true"> + 任务文本
        assert!(turn.context_block.contains("<task active=\"true\">"));
        assert!(turn.context_block.contains("重构前端轮子，支持多端复用"));
        // 记忆检索跑过（即使未命中注入，链路已通）：memory search 不 panic
        assert!(turn.context_block.starts_with("<context>\n"));
    }

    #[tokio::test]
    async fn run_user_turn_generates_system_prompt_when_empty() {
        let db = test_db();
        let mut state = init(&db).unwrap();
        let embedder = crate::embedding::NoopEmbedder;
        let ctx = InjectorContext::default();
        let window = ContextWindowConfig::default();
        // 空 system_prompt → 走 build_default_system_prompt（buildSystemPrompt 投影）
        let turn = run_user_turn(
            &db,
            &embedder,
            &mut state,
            "[ID:000001] 2026-08-09T10:00:00+08:00 [VOICE] 放首歌",
            "VOICE",
            "",
            &ctx,
            &window,
            "白马",
            false,
            None,
            "",
            None,
        )
        .await
        .unwrap();

        // STABLE 核心渲染 + 出生/存在天数占位（birth_time 空 → unknown）
        let sys = &turn.llm_messages[0];
        assert_eq!(sys.role.as_str(), "system");
        assert!(sys.content.contains("## Relationship Posture"));
        assert!(sys.content.contains(
            "## Current Name\nYour current display name and self-reference name is: 白马"
        ));
        assert!(sys.content.contains("birth date is unknown"));
        // 语音渠道 → Voice Input + Voice Orb 段注入
        assert!(sys.content.contains("## Voice Input: Spoken Brevity"));
        assert!(sys.content.contains("## Voice Orb (floating voice ball)"));
        // 音乐关键词 gate 命中 → Music Mode 块
        assert!(sys.content.contains("## Music Mode: Highest Priority"));
        // 三条消息结构不变
        assert_eq!(turn.llm_messages.len(), 3);
    }

    #[tokio::test]
    async fn run_user_turn_injects_agent_block_from_db() {
        let db = test_db();
        // 预置：委托已授权 + 一个可用 claude-code agent
        crate::db::repositories::agents::grant_delegation(&db).unwrap();
        crate::db::repositories::agents::upsert_agents(
            &db,
            &[crate::db::models::NewKnownAgent {
                id: "claude-code".into(),
                name: "Claude Code".into(),
                description: "本地 CLI 编程 agent".into(),
                available: true,
                version: None,
                invoke_type: Some("cli".into()),
                invoke_cmd: Some("claude".into()),
                invoke_args: Vec::new(),
                notes: String::new(),
                docs_url: None,
                docs_search_query: None,
                detected_at: None,
            }],
        )
        .unwrap();

        let mut state = init(&db).unwrap();
        let embedder = crate::embedding::NoopEmbedder;
        let ctx = InjectorContext::default();
        let window = ContextWindowConfig::default();
        // 空 system_prompt + 关键词命中 → 编排闭包内查库注入 AI Collaborators 块
        let turn = run_user_turn(
            &db,
            &embedder,
            &mut state,
            "[ID:000001] 2026-08-09T10:00:00+08:00 [TUI] 让 claude code 帮我写个脚本",
            "TUI",
            "",
            &ctx,
            &window,
            "白马",
            false,
            None,
            "",
            None,
        )
        .await
        .unwrap();

        let sys = &turn.llm_messages[0];
        assert!(sys
            .content
            .contains("## AI Collaborators You Can Work With"));
        assert!(sys.content.contains("**Claude Code** (claude-code)"));
        assert!(sys.content.contains("exec_command(\"claude ...\")"));
        // 无授权时不注入：撤销后重跑
        crate::db::repositories::agents::revoke_delegation(&db).unwrap();
        let turn2 = run_user_turn(
            &db,
            &embedder,
            &mut state,
            "[ID:000001] 2026-08-09T10:00:00+08:00 [TUI] 让 claude code 帮我写个脚本",
            "TUI",
            "",
            &ctx,
            &window,
            "白马",
            false,
            None,
            "",
            None,
        )
        .await
        .unwrap();
        assert!(!turn2.llm_messages[0]
            .content
            .contains("## AI Collaborators You Can Work With"));
    }

    #[tokio::test]
    async fn tick_injects_delegation_discovery_once() {
        let db = test_db();
        // 预置：委托已授权 + 一个可用 claude-code agent
        crate::db::repositories::agents::grant_delegation(&db).unwrap();
        crate::db::repositories::agents::upsert_agents(
            &db,
            &[crate::db::models::NewKnownAgent {
                id: "claude-code".into(),
                name: "Claude Code".into(),
                description: "本地 CLI 编程 agent".into(),
                available: true,
                version: None,
                invoke_type: Some("cli".into()),
                invoke_cmd: Some("claude".into()),
                invoke_args: Vec::new(),
                notes: String::new(),
                docs_url: None,
                docs_search_query: None,
                detected_at: None,
            }],
        )
        .unwrap();

        let mut state = init(&db).unwrap();
        let embedder = crate::embedding::NoopEmbedder;
        let ctx = InjectorContext::default();
        let window = ContextWindowConfig::default();

        // 首个 TICK：发现文本进入 <directions>（对齐 Node unshift 语义）
        let tick1 = run_user_turn(
            &db,
            &embedder,
            &mut state,
            "TICK 2026-08-09-11:00:00",
            "SYSTEM",
            "",
            &ctx,
            &window,
            "白马",
            true,
            Some("重构部署脚本"),
            "",
            None,
        )
        .await
        .unwrap();
        assert!(tick1.outcome.is_tick);
        assert!(
            tick1
                .context_block
                .contains("[One-time environment discovery]"),
            "首个 TICK 应注入一次性发现文本"
        );
        assert!(tick1.context_block.contains("Claude Code"));
        // mark 已落库
        assert!(crate::db::repositories::agents::has_delegation_been_asked(&db).unwrap());

        // 第二个 TICK：已 mark → 不再重复注入
        let tick2 = run_user_turn(
            &db,
            &embedder,
            &mut state,
            "TICK 2026-08-09-12:00:00",
            "SYSTEM",
            "",
            &ctx,
            &window,
            "白马",
            true,
            Some("重构部署脚本"),
            "",
            None,
        )
        .await
        .unwrap();
        assert!(
            !tick2
                .context_block
                .contains("[One-time environment discovery]"),
            "一次性发现只注入一次"
        );
    }

    /// 全链路 e2e：归属（created/continued）+ 全信号位注入 → 渲染 → 消息组装 → TICK 轮。
    /// 对齐 Node runTurn 的用户轮 + 心跳轮闭环；不依赖 FTS 命中的断言一律用透传字段，
    /// 记忆命中用 LIKE 兜底可确定的字面串验证。
    #[tokio::test]
    async fn run_user_turn_full_pipeline_multi_turn_e2e() {
        // ── 预置 DB ──
        let db = test_db();
        // 授权 + claude-code agent → system prompt 自动生成时注入 AI Collaborators 块
        crate::db::repositories::agents::grant_delegation(&db).unwrap();
        crate::db::repositories::agents::upsert_agents(
            &db,
            &[crate::db::models::NewKnownAgent {
                id: "claude-code".into(),
                name: "Claude Code".into(),
                description: "本地 CLI 编程 agent".into(),
                available: true,
                version: None,
                invoke_type: Some("cli".into()),
                invoke_cmd: Some("claude".into()),
                invoke_args: Vec::new(),
                notes: String::new(),
                docs_url: None,
                docs_search_query: None,
                detected_at: None,
            }],
        )
        .unwrap();
        // 记忆：与消息共享字面串「部署脚本」（LIKE 兜底必中，不赌 FTS 中文分词）
        crate::db::repositories::memories::insert_simple(
            &db,
            "experience",
            "上次部署脚本踩过 nginx 配置的坑",
        )
        .unwrap();
        // 预写一条同 sender 历史会话（pushMessage 落库）→ conversation_window
        push_user_message(
            &db,
            "ID:000001",
            "帮我看下部署脚本的问题",
            "2026-08-09T09:00:00+08:00",
        );

        let mut state = init(&db).unwrap();
        let embedder = crate::embedding::NoopEmbedder;
        let window = ContextWindowConfig::default();

        // ── T1：用户首轮（归属 created + 全信号位） ──
        let ctx = InjectorContext {
            person_memory: Some(mem("person", "阿杰是用户的昵称，喝美式咖啡", vec!["阿杰"])),
            user_profile: Some("后端工程师，常在 TUI 渠道干活".into()),
            task_knowledge: vec![mem("task_knowledge", "部署脚本骨架已生成", vec![])],
            action_log: Vec::new(),
            prefetched_items: vec!["天气卡预取：上海 今天 32°C".into()],
            constraints: vec!["不要替用户做决定".into()],
            tools: vec!["web_read".into(), "web_read".into()], // 去重验证
            active_policies: vec![crate::db::repositories::memories::ScoredMemory {
                memory: mem("policy", "部署前先跑一遍 lint", vec![]),
                fts_score: None,
                vec_score: None,
            }],
            self_perception: Some("本轮有 2 个能力在场".into()),
            self_snapshot: Some("处于工作时段".into()),
            self_evolution: "偏好变化：写脚本前先确认平台".into(),
            browser_runtime_text: Some("浏览器已打开目标页面".into()),
            weather_runtime_text: None,
            enable_weather_prefeed: false,
        };
        let t1_input =
            "[ID:000001] 2026-08-09T10:00:00+08:00 [TUI] 让 claude code 帮我写个部署脚本，顺便看下今天上海天气";
        let t1 = run_user_turn(
            &db,
            &embedder,
            &mut state,
            t1_input,
            "TUI",
            "",
            &ctx,
            &window,
            "白马",
            true,
            Some("重构部署脚本"),
            "",
            None,
        )
        .await
        .unwrap();

        // 归属：created + 印章线程
        assert!(!t1.outcome.is_tick);
        assert_eq!(t1.outcome.event, "created");
        assert!(t1.outcome.stamp_thread_id.is_some());
        // 消息结构：system → [runtime context] → 历史行（预置 1 条）→ 合成当前轮
        assert_eq!(t1.llm_messages.len(), 4);
        assert_eq!(t1.llm_messages[0].role.as_str(), "system");
        assert_eq!(t1.llm_messages[1].role.as_str(), "user");
        assert_eq!(t1.llm_messages[3].role.as_str(), "user");

        // system prompt（空串自动生成）：STABLE 核心 + 能力块 + agent 块
        let sys = &t1.llm_messages[0].content;
        assert!(sys.contains("## Relationship Posture"));
        assert!(sys.contains(
            "## Current Name\nYour current display name and self-reference name is: 白马"
        ));
        assert!(
            sys.contains("### Weather Surface Rules"),
            "天气关键词 → 能力块"
        );
        assert!(
            sys.contains("## AI Collaborators You Can Work With"),
            "claude code + 授权 → agent 块"
        );

        // [runtime context] 全信号位 section 透传渲染
        let block = &t1.llm_messages[1].content;
        assert!(
            block.contains("<context>\n"),
            "runtime context 含 context 块"
        );
        assert!(block.contains("<self-snapshot>\n处于工作时段"));
        assert!(block.contains("<self-evolution>\n偏好变化"));
        assert!(block.contains("<self-perception>\n本轮有 2 个能力在场"));
        assert!(block.contains("<constraints>\n- 不要替用户做决定"));
        assert!(block.contains("<active-policies>\n(These policies are active"));
        assert!(block.contains("部署前先跑一遍 lint"));
        assert!(block.contains("<person>\nAbout 阿杰"));
        assert!(block.contains("<user-profile>\n后端工程师"));
        assert!(block.contains("<task active=\"true\">"));
        assert!(block.contains("重构部署脚本"));
        assert!(block.contains("<thread topic="));
        assert!(block.contains("<task-knowledge>"));
        assert!(block.contains("部署脚本骨架已生成"));
        assert!(block.contains("<memories>"));
        assert!(
            block.contains("上次部署脚本踩过 nginx 配置的坑"),
            "记忆 LIKE 命中"
        );
        assert!(block.contains("<directions>"));
        assert!(block.contains("浏览器已打开目标页面"));
        assert!(block.contains("<extra>"));
        assert!(block.contains("Prefetched (low latency, likely asked soon)"));
        assert!(block.contains("天气卡预取：上海 今天 32°C"));
        // 会话历史轮：预置行出现在 LLM 消息里
        assert!(t1.llm_messages[2]
            .content
            .contains("帮我看下部署脚本的问题"));
        // 合成当前轮 = 消息正文
        assert!(t1.llm_messages[3]
            .content
            .contains("让 claude code 帮我写个部署脚本"));
        // 工具去重 + 透传
        assert_eq!(t1.injection.tools, vec!["web_read"]);
        // 记忆检索跑过：person/task_knowledge 透传无污染
        assert_eq!(
            t1.injection.person_memory.as_ref().unwrap().content,
            "阿杰是用户的昵称，喝美式咖啡"
        );

        // 模拟 T1 落库（pushMessage），供 T2 会话窗口引用
        push_user_message(
            &db,
            "ID:000001",
            "让 claude code 帮我写个部署脚本，顺便看下今天上海天气",
            "2026-08-09T10:00:00+08:00",
        );

        // ── T2：同主题延续（归属 continued + 历史轮进入 LLM 消息） ──
        let t2 = run_user_turn(
            &db,
            &embedder,
            &mut state,
            "[ID:000001] 2026-08-09T10:05:00+08:00 [TUI] 天气先放放，写个部署脚本的测试",
            "TUI",
            "",
            &ctx,
            &window,
            "白马",
            true,
            Some("重构部署脚本"),
            "",
            None,
        )
        .await
        .unwrap();

        assert_eq!(t2.outcome.event, "continued");
        // 历史轮（预置 1 + T1 落库 1）+ 当前轮合成 → 5 条
        assert_eq!(t2.llm_messages.len(), 5);
        let user_rows: Vec<&str> = t2
            .llm_messages
            .iter()
            .filter(|m| m.role == LlmRole::User)
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            user_rows
                .iter()
                .any(|c| c.contains("帮我看下部署脚本的问题")),
            "预置历史行应在 LLM 消息中"
        );
        assert!(
            user_rows.iter().any(|c| c.contains("顺便看下今天上海天气")),
            "T1 落库行应在 LLM 消息中"
        );
        assert!(
            user_rows.iter().any(|c| c.contains("写个部署脚本的测试")),
            "当前轮合成消息应在 LLM 消息中"
        );

        // ── T3：TICK 心跳轮（无合成 user；system 走 heartbeat 包装 + TICK 段） ──
        let tick = run_user_turn(
            &db,
            &embedder,
            &mut state,
            "TICK 2026-08-09-11:00:00",
            "SYSTEM",
            "",
            &ctx,
            &window,
            "白马",
            true,
            Some("重构部署脚本"),
            "",
            None,
        )
        .await
        .unwrap();

        assert!(tick.outcome.is_tick);
        assert_eq!(state.tick_counter, 1, "仅 TICK 轮递增计数");
        assert_eq!(tick.llm_messages[0].role.as_str(), "system");
        assert!(
            tick.llm_messages[0].content.contains("[heartbeat tick"),
            "TICK 轮 system 走 heartbeat 包装"
        );
        assert_eq!(
            tick.llm_messages[1].role,
            LlmRole::System,
            "TICK 轮 [runtime context] 用 System role"
        );
        assert!(
            !tick
                .llm_messages
                .iter()
                .any(|m| m.content.trim() == "TICK 2026-08-09-11:00:00"),
            "TICK 轮不应合成 user 消息（信号在 system 里）"
        );
    }
}
