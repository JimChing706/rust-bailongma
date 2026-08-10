//! API 服务器装配（对齐 Node 版 `src/api.js`）。
//!
//! 职责：
//! - 安全中间件：origin 校验 → access 校验（回环 / Bearer token / LAN）→ CORS → OPTIONS
//! - 路由挂载：`POST /message`、`GET /events`（SSE）、`GET /events/history`、`GET /status`
//! - WebSocket `/scene`：授权（authorize_ws_upgrade_with_credential）后建立连接
//! - 启动时补发粘性事件（agent_name_updated，对齐 api.js）
//!
//! SSE 流：connected → 粘性事件 → 实时广播；axum KeepAlive 保活（15s 注释帧，
//! 对齐 Node 的 `: ping\n\n`）。

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream::Stream;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;

use super::events::EventMsg;
use super::routes::{self, ApiState};
use super::security::{
    authorize_ws_upgrade_with_credential, extract_bearer, get_web_socket_credential,
    is_allowed_origin, is_loopback_address, is_private_lan_address, normalize_remote_address,
    timing_safe_token_equal,
};
use crate::error::Result as CoreResult;

/// SSE 心跳间隔（对齐 Node 15s keepAlive）
const SSE_PING_INTERVAL: Duration = Duration::from_secs(15);

/// API 服务器。
#[derive(Clone)]
pub struct ApiServer {
    pub state: ApiState,
    /// 前端资源根目录（静态服务，对齐 paths.js RESOURCES_DIR）
    pub resources_dir: std::path::PathBuf,
    /// 未激活 → 根路径 302 到 /activation（对齐 config.needsActivation）
    pub needs_activation: bool,
}

impl ApiServer {
    /// 组装服务器。`token` 为空时跳过 token 校验（对齐 getAuthToken）。
    pub fn new(state: ApiState, lan_enabled: bool, token: Option<String>) -> Self {
        let guard = routes::Guard {
            lan_enabled,
            token: token.unwrap_or_default().trim().to_string(),
        };
        let state = state.with_guard(guard);
        // 启动时补发 agent_name 粘性事件（对齐 api.js 307-310 行）
        let name = (state.agent_name)();
        state
            .bus
            .set_sticky("agent_name_updated", json!({ "name": name }));
        Self {
            state,
            resources_dir: super::static_assets::resolve_resources_dir(),
            needs_activation: false,
        }
    }

    /// 覆盖静态资源根目录（测试 / 平移前端资产时使用）。
    pub fn with_resources(mut self, dir: std::path::PathBuf, needs_activation: bool) -> Self {
        self.resources_dir = dir;
        self.needs_activation = needs_activation;
        self
    }

    /// 构建 axum Router（安全中间件 + 路由 + 静态 fallback）。
    pub fn router(&self) -> Router {
        let state = self.state.clone();
        let resources = self.resources_dir.clone();
        let needs_activation = self.needs_activation;
        Router::new()
            .route("/events", get(handle_sse))
            .route("/events/history", get(routes::get_events_history))
            .route("/message", post(routes::post_message))
            .route("/status", get(routes::get_status))
            .route("/scene", get(handle_scene_ws))
            // 静态资源 fallback（对齐 handleStaticRoutes：API 未匹配时尝试页面/资产）
            .fallback(move |req: Request| async move {
                match super::static_assets::handle_static(&req, &resources, needs_activation) {
                    Some(resp) => resp,
                    None => (
                        StatusCode::NOT_FOUND,
                        axum::Json(json!({ "ok": false, "error": "not found" })),
                    )
                        .into_response(),
                }
            })
            .layer(middleware::from_fn_with_state(state.clone(), guard_request))
            .with_state(state)
    }

    /// 启动监听，返回实际绑定地址。
    pub async fn serve(self, host: &str, port: u16) -> CoreResult<SocketAddr> {
        let listener = tokio::net::TcpListener::bind((host, port)).await?;
        let addr = listener.local_addr()?;
        let router = self.router();
        tracing::info!("[API] Listening at http://{host}:{port}");
        tracing::info!("[API]   POST /message  - send message to agent");
        tracing::info!("[API]   GET  /events   - SSE real-time stream");
        tracing::info!("[API]   GET  /status   - status");
        tracing::info!("[API]   WS   /scene    - Scene channel");
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok(addr)
    }
}

// ─────────────────────────────────────────────────────────────
// 安全中间件（对齐 api.js 的请求守卫）
// ─────────────────────────────────────────────────────────────

/// 请求守卫：origin 校验 → access 校验 → CORS → OPTIONS。
async fn guard_request(
    State(state): State<ApiState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let guard = &state.guard;

    // 1. origin 校验（对齐 api.js：origin 存在且不允许 → 403）
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(ref o) = origin {
        if !is_allowed_origin(o, guard.lan_enabled) {
            return json_response(
                StatusCode::FORBIDDEN,
                json!({ "ok": false, "error": "forbidden origin" }),
            );
        }
    }

    // 2. access 校验：回环 || Bearer token || LAN 请求（对齐 hasAllowedAccess）
    let remote_addr = normalize_remote_address(&remote.ip().to_string());
    let is_loopback = is_loopback_address(&remote_addr);
    let has_token = {
        let auth = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        extract_bearer(auth)
            .map(|b| timing_safe_token_equal(&b, &guard.token))
            .unwrap_or(false)
    };
    let is_lan = guard.lan_enabled && is_private_lan_address(&remote_addr);
    if !is_loopback && !has_token && !is_lan {
        return json_response(
            StatusCode::FORBIDDEN,
            json!({ "ok": false, "error": "forbidden" }),
        );
    }

    // 3. CORS 响应头 + OPTIONS（对齐 setCorsHeaders + OPTIONS 204）
    let mut resp = if req.method() == Method::OPTIONS {
        json_response(StatusCode::NO_CONTENT, json!({}))
    } else {
        next.run(req).await
    };
    if let Some(o) = origin {
        resp.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_str(&o).unwrap_or_else(|_| HeaderValue::from_static("")),
        );
    }
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
    resp
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (status, axum::Json(body)).into_response()
}

// ─────────────────────────────────────────────────────────────
// SSE 流（对齐 handleEventRoutes 的 /events）
// ─────────────────────────────────────────────────────────────

/// GET /events —— SSE 实时事件流。
async fn handle_sse(
    State(state): State<ApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // 事件源：broadcast（实时）+ mpsc（connected / sticky 补发）
    let (tx, rx) = tokio::sync::mpsc::channel::<EventMsg>(128);
    let mut bcast_rx = state.subscribe();
    let sticky = state.bus.flush_sticky();

    tokio::spawn(async move {
        // 1. connected（对齐 res.write data:{type:'connected'})
        let _ = tx
            .send(EventMsg {
                r#type: "connected".into(),
                data: json!({}),
                ts: super::events::iso_now(),
            })
            .await;
        // 2. 粘性事件补发
        for ev in sticky {
            if tx.send(ev).await.is_err() {
                return;
            }
        }
        // 3. 实时广播（保活交给 axum KeepAlive 注释帧，对齐 Node `: ping`）
        //    客户端断线时 axum 会 drop ReceiverStream → drop rx → tx.closed() 完成，
        //    立即退出并释放 broadcast 订阅者（避免空闲期僵尸任务 + 订阅泄漏）。
        loop {
            tokio::select! {
                _ = tx.closed() => return,
                m = bcast_rx.recv() => match m {
                    Ok(m) => {
                        if tx.send(m).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    });

    let stream = ReceiverStream::new(rx).map(|msg| {
        let payload = json!({ "type": msg.r#type, "data": msg.data, "ts": msg.ts });
        Ok::<Event, Infallible>(Event::default().data(payload.to_string()))
    });
    Sse::new(stream).keep_alive(KeepAlive::default().interval(SSE_PING_INTERVAL))
}

// ─────────────────────────────────────────────────────────────
// WebSocket /scene（对齐 attachWebSocketUpgrades 的授权 + 连接）
// ─────────────────────────────────────────────────────────────

/// GET /scene —— WebSocket upgrade（授权后建立）。
async fn handle_scene_ws(
    State(state): State<ApiState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let guard = &state.guard;
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let protocol = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let credential = get_web_socket_credential(auth, protocol);
    let remote_addr = normalize_remote_address(&remote.ip().to_string());

    let authz = authorize_ws_upgrade_with_credential(
        "/scene",
        origin,
        &remote_addr,
        guard.lan_enabled,
        &guard.token,
        credential.as_deref(),
        &["/scene"],
    );
    if !authz.ok {
        return json_response(
            StatusCode::from_u16(authz.status).unwrap_or(StatusCode::FORBIDDEN),
            json!({
                "ok": false, "error": authz.reason
            }),
        );
    }

    // 建立连接：接入 Scene 协议（hello/resync/intent + store 变更广播）
    ws.on_upgrade(|socket| async move {
        super::scene::handle_scene_connection(state, socket).await;
    })
}

/// Scene 连接处理（协议层在 `api/scene.rs`；空闲超时 60s 对齐 attachWebSocketIdleTimeout）。
// ─────────────────────────────────────────────────────────────
// SSE 事件 → 文本帧（供测试复用）
// ─────────────────────────────────────────────────────────────
pub fn sse_frame(msg: &EventMsg) -> String {
    let payload = json!({ "type": msg.r#type, "data": msg.data, "ts": msg.ts });
    format!("data: {payload}\n\n")
}

/// 测试辅助：构造带默认守卫（loopback 直通）的 ApiServer。
pub fn test_server(state: ApiState) -> ApiServer {
    ApiServer::new(state, false, None)
}

#[cfg(test)]
mod tests {
    use super::super::events::EventBus;
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn test_state() -> ApiState {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(dir.path().join("test.db")).unwrap();
        let bus = EventBus::new(std::sync::Arc::new(|_, _, _, _| {}));
        ApiState::new(
            db,
            bus,
            std::sync::Arc::new(|_| Some(routes::InboundQueued { conversation_id: 7 })),
            std::sync::Arc::new(|| "小白龙".into()),
            std::sync::Arc::new(|| json!({ "running": true })),
        )
    }

    #[tokio::test]
    async fn root_and_status_are_served() {
        let server = test_server(test_state());
        let router = server.router();

        let resp = router.clone().oneshot(local_req("/")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = router.oneshot(local_req("/status")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn message_route_accepts_post() {
        let server = test_server(test_state());
        let router = server.router();
        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/message")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"content":"hello"}"#))
            .unwrap();
        let mut req = local_req_with(req);
        let _ = &mut req;
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn sse_emits_connected_and_sticky() {
        let state = test_state();
        state.bus.set_sticky(
            "audio_play",
            json!({ "url": "boot.wav", "type": "startup" }),
        );
        let server = test_server(state.clone());
        let router = server.router();
        let resp = router.oneshot(local_req("/events")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            "text/event-stream"
        );

        // 读 SSE 前几帧：connected + sticky（流是无限的，读够即止）
        use http_body_util::BodyExt as _;
        let mut body = resp.into_body();
        let mut text = String::new();
        for _ in 0..6 {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(bytes) = frame.into_data() {
                        text.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
                _ => break,
            }
        }
        drop(body); // 断开连接，停止 spawn 的转发任务
        let frames: Vec<&str> = text.split("\n\n").collect();
        assert!(frames.iter().any(|f| f.contains("\"type\":\"connected\"")));
        assert!(frames.iter().any(|f| f.contains("boot.wav")));
    }

    /// 构造带 loopback ConnectInfo 的本地请求（模拟本机访问）。
    fn local_req(uri: &str) -> HttpRequest<Body> {
        local_req_with(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
    }

    fn local_req_with(mut req: HttpRequest<Body>) -> HttpRequest<Body> {
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 54321))));
        req
    }
}
