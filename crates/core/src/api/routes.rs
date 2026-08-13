//! API 路由处理器（对齐 Node 版 `src/api/routes/message.js` / `events.js` / `memory.js`）。
//!
//! 采用 axum handler 风格，由 `server.rs` 挂载：
//! - `POST /message`         入站消息（去重 → 入队 → 广播 message_in）
//! - `GET  /events/history`  brain-ui 观测历史（L1/L2）
//! - `GET  /status`          状态快照（memory_count / running / 扩展字段）
//! - `GET  /events`          SSE 实时流（在 server.rs 用 `sse_stream` 挂载）
//! - `GET  /metrics/weekly`  M4 周报（六指标 + 阈值信号 + 唤醒成本，波3·片3）
//!
//! 意识循环尚未迁移的部分（pushMessage / 记忆 / 控制开关）通过注入的
//! 回调对接，路由层与循环层解耦，可独立测试。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::events::{iso_now, EventBus};
use super::security::RateLimiter;
use crate::db::repositories::{brain_ui_events, llm_metrics};
use crate::db::Db;
use crate::scene::SceneStore;

// ─────────────────────────────────────────────────────────────
// 入站消息（对齐 message.js 的 pushMessage 入参）
// ─────────────────────────────────────────────────────────────

/// 一条入站消息（由意识循环消费）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub from_id: String,
    pub content: String,
    pub channel: String,
    /// 附加元数据（strictEvaluation / forbiddenTools / attachments 等）
    pub meta: Value,
}

/// 入队结果（对齐 pushMessage 返回值中使用的 conversationId）。
#[derive(Debug, Clone, Default)]
pub struct InboundQueued {
    pub conversation_id: i64,
}

/// 入队回调（server 层注入真实实现；测试注入 stub）。
pub type InboundHandler = Arc<dyn Fn(InboundMessage) -> Option<InboundQueued> + Send + Sync>;
/// 代理名回调（默认 小白龙）。
pub type AgentNameFn = Arc<dyn Fn() -> String + Send + Sync>;
/// 状态快照回调（/status 的扩展字段，server 层注入）。
pub type StatusFn = Arc<dyn Fn() -> Value + Send + Sync>;

// ─────────────────────────────────────────────────────────────
// 请求 / 响应体
// ─────────────────────────────────────────────────────────────

/// POST /message 查询参数：`wait=true` 时同步等待该会话的 LLM 回复并直接返回。
/// （微信网桥用：Node 侧转发微信消息后无需解析 SSE 即可拿到回复文本）
#[derive(Debug, Default, Deserialize)]
pub struct MessageQuery {
    #[serde(default)]
    pub wait: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct InboundBody {
    #[serde(default = "default_from_id")]
    pub from_id: String,
    #[serde(default)]
    pub content: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    // 客户端消息 ID（去重用，显式 ID 版）
    #[serde(alias = "clientMessageId", default)]
    pub client_message_id: Option<String>,
    // 严格评估（对齐 body.strict_evaluation ?? body.strictEvaluation）
    #[serde(alias = "strictEvaluation", default)]
    pub strict_evaluation: Option<bool>,
    // 评估模式字符串（"strict" → 严格评估）
    #[serde(alias = "evaluationMode", default)]
    pub evaluation_mode: Option<String>,
    // 禁用工具（对齐 body.forbidden_tools ?? body.forbiddenTools）
    #[serde(alias = "forbiddenTools", default)]
    pub forbidden_tools: Option<Vec<String>>,
    // 附加字段（图片、附件等；基础版原样透传）
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

fn default_from_id() -> String {
    "ID:000001".into()
}
fn default_channel() -> String {
    "API".into()
}

#[derive(Debug, Serialize)]
pub struct InboundResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate: Option<bool>,
    pub agent_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<i64>,
}

impl InboundResponse {
    fn into_json(self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({ "ok": false }))
    }
}

// ─────────────────────────────────────────────────────────────
// 去重（对齐 claimInboundMessage）
// ─────────────────────────────────────────────────────────────

const DEDUPE_TTL_EXPLICIT: Duration = Duration::from_millis(10_000);
const DEDUPE_TTL_FALLBACK: Duration = Duration::from_millis(1_500);

/// 显式客户端消息 ID 格式（与 Node 正则一致）
fn normalize_client_message_id(value: &str) -> Option<String> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    let ok = text.len() >= 8
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    ok.then(|| text.to_string())
}

struct Deduper {
    entries: HashMap<String, Instant>,
}

impl Deduper {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// 尝试认领消息；重复 → false（对齐 claimInboundMessage 返回 { claimed }）。
    fn claim(&mut self, key: String, ttl: Duration) -> bool {
        let now = Instant::now();
        self.entries.retain(|_, at| now.duration_since(*at) < ttl);
        if let Some(at) = self.entries.get(&key) {
            if now.duration_since(*at) <= ttl {
                return false;
            }
        }
        self.entries.insert(key, now);
        true
    }

    fn release(&mut self, key: &str) {
        self.entries.remove(key);
    }

    fn make_key(
        &self,
        client_message_id: &Option<String>,
        from_id: &str,
        channel: &str,
        content: &str,
    ) -> (String, Duration) {
        match client_message_id
            .as_deref()
            .and_then(normalize_client_message_id)
        {
            Some(id) => (format!("id:{id}"), DEDUPE_TTL_EXPLICIT),
            None => {
                let body = serde_json::json!([from_id, channel, content]).to_string();
                (format!("body:{body}"), DEDUPE_TTL_FALLBACK)
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────
// API 状态
// ─────────────────────────────────────────────────────────────

/// 路由共享状态（server 层组装）。
#[derive(Clone)]
pub struct ApiState {
    pub db: Db,
    pub bus: EventBus,
    pub inbound: InboundHandler,
    pub agent_name: AgentNameFn,
    pub status: StatusFn,
    /// Scene 场景状态机（Agent 驱动 UI 真相源，WS /scene 与工具层共享）
    pub scene: SceneStore,
    pub guard: Guard,
    deduper: Arc<Mutex<Deduper>>,
}

/// 安全守卫配置（请求中间件 / WS 授权共享）。
#[derive(Clone, Debug)]
pub struct Guard {
    pub lan_enabled: bool,
    pub token: String,
    /// `POST /message` 限流器（按来源地址计数；防刷接口烧额度 / 撑爆 DB）
    pub message_rate: Arc<RateLimiter>,
}

impl Default for Guard {
    fn default() -> Self {
        Self {
            lan_enabled: false,
            token: String::new(),
            message_rate: Arc::new(RateLimiter::default()),
        }
    }
}

impl ApiState {
    pub fn new(
        db: Db,
        bus: EventBus,
        inbound: InboundHandler,
        agent_name: AgentNameFn,
        status: StatusFn,
    ) -> Self {
        Self {
            db,
            bus,
            inbound,
            agent_name,
            status,
            scene: SceneStore::new(),
            guard: Guard::default(),
            deduper: Arc::new(Mutex::new(Deduper::new())),
        }
    }

    pub fn with_guard(mut self, guard: Guard) -> Self {
        self.guard = guard;
        self
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<super::events::EventMsg> {
        self.bus.subscribe()
    }
}

// ─────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────

/// POST /message —— 入站消息（对齐 handleMessageRoutes）。
/// POST /approval —— 人工确认抉择回传（Phase 1 安全基线）。
/// 执行端（ApprovalGate）挂起高风险工具调用，用户在此回传抉择：
/// allow_once / allow_session / deny（改参 Phase 2 落地）。
#[derive(Deserialize)]
pub struct ApprovalBody {
    pub id: String,
    #[serde(default)]
    pub decision: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TraceQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub tool: Option<String>,
}

/// GET /trace —— 工具调用轨迹查询（Phase 2 可观测性）。
pub async fn get_trace(
    Query(q): Query<TraceQuery>,
) -> (StatusCode, Json<Value>) {
    let limit = q.limit.unwrap_or(50).min(500);
    let tool = q.tool.unwrap_or_default();
    let traces = crate::trace::global().recent(limit, &tool);
    let items: Vec<Value> = traces
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "ts_ms": t.ts_ms,
                "tool": t.tool,
                "stage": t.stage,
                "decision": t.decision,
                "detail": t.detail,
                "duration_ms": t.duration_ms,
                "ok": t.ok,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "count": items.len(), "traces": items })),
    )
}

pub async fn post_approval(
    State(state): State<ApiState>,
    Json(body): Json<ApprovalBody>,
) -> (StatusCode, Json<Value>) {
    if body.id.is_empty() || body.decision.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "id 与 decision 必填" })),
        );
    }
    match crate::approval::global().submit(&body.id, &body.decision) {
        Ok(choice) => {
            // 移除场景卡片
            let _ = state.scene.set(&format!("approval:{}", body.id), None);
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "decision": choice.as_str() })),
            )
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

pub async fn post_message(
    State(state): State<ApiState>,
    Query(query): Query<MessageQuery>,
    Json(body): Json<InboundBody>,
) -> (StatusCode, Json<Value>) {
    let content = body.content.trim().to_string();
    if content.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "content or image required" })),
        );
    }

    // 去重认领（失败则释放；对齐 catch 中 recentInboundMessages.delete(claim.key)）
    let (key, ttl) = {
        let deduper = state.deduper.lock().unwrap();
        deduper.make_key(
            &body.client_message_id,
            &body.from_id,
            &body.channel,
            &content,
        )
    };
    let claimed = state.deduper.lock().unwrap().claim(key.clone(), ttl);
    if !claimed {
        return (
            StatusCode::OK,
            Json(
                InboundResponse {
                    ok: true,
                    duplicate: Some(true),
                    agent_name: (state.agent_name)(),
                    conversation_id: None,
                }
                .into_json(),
            ),
        );
    }

    // 组装 meta（对齐 message.js 的 meta 组装）
    let mut meta = serde_json::Map::new();
    let strict = body.strict_evaluation.or_else(|| {
        body.evaluation_mode
            .as_deref()
            .map(|m| m.trim().eq_ignore_ascii_case("strict"))
    });
    if let Some(v) = strict {
        meta.insert("strictEvaluation".into(), json!(v));
    }
    if let Some(tools) = &body.forbidden_tools {
        meta.insert("forbiddenTools".into(), json!(tools));
    }
    for (k, v) in body.extra {
        if matches!(k.as_str(), "attachments" | "image" | "image_url") {
            meta.insert(k, v);
        }
    }

    let inbound = InboundMessage {
        from_id: body.from_id.clone(),
        content: content.clone(),
        channel: body.channel.clone(),
        meta: Value::Object(meta.clone()),
    };

    // wait=true：先建立订阅（确保不丢后续 message_out），再入队并广播 message_in。
    // 注意：订阅必须先于 inbound——inbound 会 spawn 意识轮，若 LLM 快速回复（或未激活
    // 立即降级），message_out 可能在订阅建立前发出，导致 wait 模式丢失回复、干等超时。
    let wait_rx = if query.wait.unwrap_or(false) {
        Some(state.subscribe())
    } else {
        None
    };

    let queued = (state.inbound)(inbound);
    let conversation_id = queued.as_ref().map(|q| q.conversation_id);
    if conversation_id.is_none() {
        // 入队失败：释放去重槽位，避免误吞后续消息
        state.deduper.lock().unwrap().release(&key);
    }

    // 入队失败（无 conversation_id）则无可等消息，丢弃订阅
    let wait_rx = if conversation_id.is_some() { wait_rx } else { None };

    // 广播 message_in（对齐 emitEvent）
    let attachments = meta.get("attachments").cloned().unwrap_or(Value::Null);
    state.bus.emit(
        "message_in",
        json!({
            "from_id": body.from_id,
            "content": content,
            "channel": body.channel,
            "timestamp": iso_now(),
            "conversation_id": conversation_id.unwrap_or(0),
            "attachments": attachments,
        }),
    );

    // wait=true：同步等待该会话的 LLM 回复（按 conversation_id 匹配，300s 超时）
    if let Some(mut rx) = wait_rx {
        let cid = conversation_id.unwrap_or(0);
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return (
                    StatusCode::OK,
                    Json(json!({ "ok": true, "reply": null, "timed_out": true })),
                );
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev))
                    if ev.r#type == "message_out"
                        && ev.data["conversation_id"].as_i64() == Some(cid) =>
                {
                    let reply = ev.data["content"].as_str().unwrap_or("").to_string();
                    return (
                        StatusCode::OK,
                        Json(json!({ "ok": true, "reply": reply, "timed_out": false })),
                    );
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => {
                    return (
                        StatusCode::OK,
                        Json(json!({ "ok": true, "reply": null, "timed_out": true })),
                    );
                }
            }
        }
    }

    (
        StatusCode::OK,
        Json(
            InboundResponse {
                ok: true,
                duplicate: None,
                agent_name: (state.agent_name)(),
                conversation_id,
            }
            .into_json(),
        ),
    )
}

/// GET /events/history —— brain-ui 观测历史（对齐 handleEventRoutes）。
#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub path: Option<String>,
    pub limit: Option<usize>,
}

pub async fn get_events_history(
    State(state): State<ApiState>,
    Query(q): Query<HistoryQuery>,
) -> Json<Value> {
    let path = match q.path.as_deref() {
        Some("all") => "all",
        Some("l1") => "l1",
        _ => "l2",
    };
    // Node: clamp(1, 400)；默认 160
    let limit = q.limit.unwrap_or(160).clamp(1, 400);
    match brain_ui_events::get_brain_ui_event_history(&state.db, path, limit) {
        Ok(history) => {
            let events: Vec<Value> = history
                .events
                .iter()
                .map(|e| {
                    json!({
                        "id": e.id,
                        "path": e.path,
                        "type": e.event_type,
                        "data": e.payload,
                        "ts": e.timestamp,
                    })
                })
                .collect();
            Json(json!({ "ok": true, "events": events }))
        }
        Err(e) => {
            tracing::warn!("[events/history] query failed: {e}");
            Json(json!({ "ok": false, "error": e.to_string() }))
        }
    }
}

/// GET /health —— 轻量健康检查（Node 桥接 / 监控探针 / 运维自检）。
/// 与 /status 的区别：只回最小固定字段，不触发扩展状态计算。
pub async fn get_health(State(state): State<ApiState>) -> Json<Value> {
    let memory_count = state
        .db
        .conn()
        .query_row("SELECT COUNT(*) FROM memories", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0);
    Json(json!({
        "ok": true,
        "running": true,
        "memory_count": memory_count,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// GET /conversations —— 最近会话（微信桥接观测面板数据源）。
#[derive(Debug, Deserialize)]
pub struct ConversationsQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

pub async fn get_conversations(
    State(state): State<ApiState>,
    Query(q): Query<ConversationsQuery>,
) -> (StatusCode, Json<Value>) {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    match crate::db::repositories::conversations::recent(&state.db, limit) {
        Ok(rows) => {
            let items: Vec<Value> = rows
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "role": c.role,
                        "from_id": c.from_id,
                        "to_id": c.to_id,
                        "content": c.content,
                        "timestamp": c.timestamp,
                        "channel": c.channel,
                        "external_party_id": c.external_party_id,
                        "delivery_status": c.delivery_status,
                        "focus_topic": c.focus_topic,
                        "open_question": c.open_question,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "count": items.len(), "conversations": items })),
            )
        }
        Err(e) => {
            tracing::warn!("[conversations] query failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "error": e.to_string() })),
            )
        }
    }
}

/// GET /status —— 状态快照（对齐 memory.js /status + 扩展字段）。
pub async fn get_status(State(state): State<ApiState>) -> Json<Value> {
    // 基础字段：memories 数量（表不存在时静默为 0）
    let memory_count = state
        .db
        .conn()
        .query_row("SELECT COUNT(*) FROM memories", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0);
    let mut extra = (state.status)();
    if let Some(obj) = extra.as_object_mut() {
        obj.insert("ok".into(), json!(true));
        obj.insert("memory_count".into(), json!(memory_count));
    }
    Json(extra)
}

/// GET /metrics/weekly —— M4 周报（波3·片3 对外入口）。
/// 六指标 + 派生率 + 阈值信号 + 唤醒成本；`days` 窗口默认 7（clamp 1..=90）。
/// 数据源：llm_metrics_daily 日聚合（长期趋势）+ llm_calls stage='wakeup' 明细。
#[derive(Debug, Deserialize)]
pub struct WeeklyQuery {
    #[serde(default)]
    pub days: Option<i64>,
}

pub async fn get_metrics_weekly(
    State(state): State<ApiState>,
    Query(q): Query<WeeklyQuery>,
) -> (StatusCode, Json<Value>) {
    let days = q.days.unwrap_or(7).clamp(1, 90);
    match llm_metrics::weekly_report(&state.db, days) {
        Ok(r) => {
            let text = r.render();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "days": days,
                    "total_calls": r.total_calls,
                    "error_count": r.error_count,
                    "retry_count": r.retry_count,
                    "fallback_count": r.fallback_count,
                    "aborted_count": r.aborted_count,
                    "total_tokens": r.total_tokens,
                    "cached_tokens": r.cached_tokens,
                    "cache_rate_pct": r.cache_rate_pct(),
                    "error_rate_pct": r.error_rate_pct(),
                    "avg_ttft_ms": r.avg_ttft_ms(),
                    "avg_duration_ms": r.avg_duration_ms(),
                    "avg_tokens_per_call": r.avg_tokens_per_call(),
                    "wakeup": {
                        "calls": r.wakeup.calls,
                        "total_tokens": r.wakeup.total_tokens,
                        "cached_tokens": r.wakeup.cached_tokens,
                        "share_pct": r.wakeup.share_of_total(r.total_tokens),
                    },
                    "signals": r.signals,
                    "text": text,
                })),
            )
        }
        Err(e) => {
            tracing::warn!("[metrics/weekly] query failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "error": e.to_string() })),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试 ApiState（stub inbound / agent_name / status）。
    fn test_state() -> ApiState {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("test.db")).unwrap();
        let bus = EventBus::new(Arc::new(|_, _, _, _| {}));
        ApiState::new(
            db,
            bus,
            Arc::new(|m| {
                Some(InboundQueued {
                    conversation_id: m.content.len() as i64,
                })
            }),
            Arc::new(|| "小白龙".into()),
            Arc::new(|| json!({ "running": true })),
        )
    }

    #[test]
    fn inbound_body_accepts_camel_case_aliases() {
        // camelCase 客户端字段（Node 前端契约）：strictEvaluation / forbiddenTools
        let body: InboundBody = serde_json::from_value(json!({
            "from_id": "ID:000001",
            "content": "分析一下",
            "channel": "API",
            "strictEvaluation": true,
            "forbiddenTools": ["exec_command"],
            "evaluationMode": "strict",
        }))
        .unwrap();
        assert_eq!(body.strict_evaluation, Some(true));
        assert_eq!(body.forbidden_tools, Some(vec!["exec_command".to_string()]));
        assert_eq!(body.evaluation_mode.as_deref(), Some("strict"));
        // 未进 flatten extra（未被静默丢弃）
        assert!(body.extra.is_empty(), "camelCase 字段不应落进 extra");

        // snake_case 照常
        let body: InboundBody = serde_json::from_value(json!({
            "content": "x",
            "strict_evaluation": false,
            "forbidden_tools": ["web_read"],
        }))
        .unwrap();
        assert_eq!(body.strict_evaluation, Some(false));
        assert_eq!(body.forbidden_tools, Some(vec!["web_read".to_string()]));
        // 未知字段仍进 extra（透传语义不变）
        let body: InboundBody = serde_json::from_value(json!({
            "content": "x",
            "custom_flag": 1,
        }))
        .unwrap();
        assert_eq!(
            body.extra.get("custom_flag").and_then(Value::as_i64),
            Some(1)
        );
    }

    #[tokio::test]
    async fn post_message_queues_and_broadcasts() {
        let state = test_state();
        let mut rx = state.subscribe();
        let resp = post_message(
            State(state.clone()),
            Query(MessageQuery::default()),
            Json(InboundBody {
                from_id: "ID:000001".into(),
                content: "  你好  ".into(),
                channel: "API".into(),
                client_message_id: None,
                strict_evaluation: None,
                evaluation_mode: None,
                forbidden_tools: None,
                extra: Default::default(),
            }),
        )
        .await;
        let (status, Json(body)) = resp;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["agent_name"], "小白龙");
        assert_eq!(body["conversation_id"], 6); // "你好" UTF-8 字节数 = 6
                                                // 广播 message_in
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.r#type, "message_in");
        assert_eq!(msg.data["content"], "你好");
    }

    #[tokio::test]
    async fn empty_content_returns_400() {
        let state = test_state();
        let resp = post_message(
            State(state),
            Query(MessageQuery::default()),
            Json(InboundBody {
                from_id: "ID:000001".into(),
                content: "   ".into(),
                channel: "API".into(),
                client_message_id: None,
                strict_evaluation: None,
                evaluation_mode: None,
                forbidden_tools: None,
                extra: Default::default(),
            }),
        )
        .await;
        assert_eq!(resp.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn duplicate_explicit_id_returns_duplicate() {
        let state = test_state();
        let body = |id: Option<String>| InboundBody {
            from_id: "ID:000001".into(),
            content: "hello".into(),
            channel: "API".into(),
            client_message_id: id,
            strict_evaluation: None,
            evaluation_mode: None,
            forbidden_tools: None,
            extra: Default::default(),
        };
        let first = post_message(
            State(state.clone()),
            Query(MessageQuery::default()),
            Json(body(Some("msg_12345678".into()))),
        )
        .await;
        assert_eq!(first.0, StatusCode::OK);
        let second = post_message(
            State(state.clone()),
            Query(MessageQuery::default()),
            Json(body(Some("msg_12345678".into()))),
        )
        .await;
        let (_, Json(b)) = second;
        assert_eq!(b["duplicate"], true);
        assert_eq!(b["conversation_id"], Value::Null);
    }

    #[tokio::test]
    async fn wait_mode_returns_reply_from_message_out() {
        // wait=true：订阅在入队后建立 → 等 message_in 确认订阅就绪 → 发射 message_out → 直接返回 reply
        let state = test_state();
        let state2 = state.clone();
        let handle = tokio::spawn(async move {
            post_message(
                State(state2),
                Query(MessageQuery { wait: Some(true) }),
                Json(InboundBody {
                    from_id: "ID:000001".into(),
                    content: "hello".into(),
                    channel: "WECHAT".into(),
                    client_message_id: None,
                    strict_evaluation: None,
                    evaluation_mode: None,
                    forbidden_tools: None,
                    extra: Default::default(),
                }),
            )
            .await
        });
        // 等 stub inbound 入队 + message_in 广播（wait 订阅已建立），再发射回复事件
        let mut rx = state.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while let Ok(ev) = rx.recv().await {
                if ev.r#type == "message_in" {
                    break;
                }
            }
        })
        .await;
        state.bus.emit(
            "message_out",
            json!({
                "from_id": "ID:000001",
                "content": "你好，收到",
                "channel": "WECHAT",
                "timestamp": iso_now(),
                "conversation_id": 5, // "hello" 字节数 = 5
            }),
        );
        let (status, Json(body)) = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["reply"], "你好，收到");
        assert_eq!(body["timed_out"], false);
    }

    #[test]
    fn normalize_client_id_validates_format() {
        assert_eq!(
            normalize_client_message_id("abc_123456"),
            Some("abc_123456".into())
        );
        assert_eq!(normalize_client_message_id("short"), None); // <8
        assert_eq!(normalize_client_message_id(&"a".repeat(200)), None); // >128
        assert_eq!(normalize_client_message_id("abc def!"), None); // 非法字符
        assert_eq!(normalize_client_message_id("  "), None);
    }

    #[test]
    fn body_fallback_ttl_is_short() {
        let mut d = Deduper::new();
        let (key, ttl) = d.make_key(&None, "A", "API", "hello");
        assert!(key.starts_with("body:"));
        assert_eq!(ttl, DEDUPE_TTL_FALLBACK);
        assert!(d.claim(key.clone(), ttl));
        // 立刻重复 → 拒绝
        assert!(!d.claim(key, ttl));
    }

    #[tokio::test]
    async fn history_returns_ok() {
        let state = test_state();
        let resp = get_events_history(
            State(state),
            Query(HistoryQuery {
                path: None,
                limit: None,
            }),
        )
        .await;
        let v = resp.0;
        assert_eq!(v["ok"], true);
        assert!(v["events"].is_array());
        assert!(v["events"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_includes_memory_count_and_extra() {
        let state = test_state();
        let resp = get_status(State(state)).await;
        assert_eq!(resp.0["running"], true);
        assert_eq!(resp.0["ok"], true);
        assert_eq!(resp.0["memory_count"], 0);
    }

    #[tokio::test]
    async fn metrics_weekly_empty_db_returns_signal() {
        // M4 验收：空库 → ok=true + 「无 LLM 调用」信号 + 六指标全 0
        let state = test_state();
        let resp = get_metrics_weekly(State(state), Query(WeeklyQuery { days: None })).await;
        let (status, Json(body)) = resp;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["days"], 7, "默认 7 天窗口");
        assert_eq!(body["total_calls"], 0);
        let signals = body["signals"].as_array().unwrap();
        assert!(
            signals.iter().any(|s| s.as_str().unwrap().contains("无 LLM 调用")),
            "空窗口应提示无调用，实际: {signals:?}"
        );
        assert!(body["text"].as_str().unwrap().contains("LLM 周报"));
    }

    #[tokio::test]
    async fn metrics_weekly_aggregates_daily() {
        // M4 验收：插入日聚合 → 六指标 + 派生率 + 唤醒成本字段正确返回
        let state = test_state();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        llm_metrics::upsert_daily(
            &state.db,
            &today,
            &llm_metrics::DailyDelta {
                total_calls: 100,
                error_count: 5,
                retry_count: 10,
                fallback_count: 2,
                aborted_count: 1,
                total_tokens: 200_000,
                cached_tokens: 60_000,
                ttft_sum_ms: 120_000,
                ttft_count: 60,
                duration_sum_ms: 800_000,
            },
        )
        .unwrap();

        let resp = get_metrics_weekly(
            State(state),
            Query(WeeklyQuery { days: Some(7) }),
        )
        .await;
        let (_, Json(body)) = resp;
        assert_eq!(body["total_calls"], 100);
        assert_eq!(body["error_count"], 5);
        assert_eq!(body["retry_count"], 10);
        assert_eq!(body["fallback_count"], 2);
        assert_eq!(body["aborted_count"], 1);
        assert_eq!(body["total_tokens"], 200_000);
        assert_eq!(body["cached_tokens"], 60_000);
        assert!((body["cache_rate_pct"].as_f64().unwrap() - 30.0).abs() < 1e-9);
        assert!((body["error_rate_pct"].as_f64().unwrap() - 5.0).abs() < 1e-9);
        assert!((body["avg_ttft_ms"].as_f64().unwrap() - 2_000.0).abs() < 1e-9);
        assert!((body["avg_duration_ms"].as_f64().unwrap() - 8_000.0).abs() < 1e-9);
        assert_eq!(body["wakeup"]["calls"], 0, "无唤醒轮数据");
    }
}
