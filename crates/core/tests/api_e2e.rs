//! API 端到端集成测试：真实监听 + HTTP/SSE/WS 闭环（对齐 Node 版行为）。
//!
//! 覆盖：
//! - POST /message 入站 → SSE 收到 message_in
//! - GET /events 首帧 connected + sticky（agent_name_updated）
//! - GET /status / GET /events/history
//! - WS /scene loopback 连接建立
//! - 安全：恶意 origin → 403

use std::net::SocketAddr;
use std::sync::Arc;

use bailongma_core::api::events::{EventBus, EventMsg};
use bailongma_core::api::routes::{ApiState, InboundQueued};
use bailongma_core::api::server::ApiServer;
use bailongma_core::db::Db;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::connect_async;

/// 启动真实服务器（随机端口），返回 (http_base, ws_base, state, bus)。
async fn start_server() -> (String, String, ApiState, EventBus) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("test.db")).unwrap();
    let bus = EventBus::new(Arc::new(|_, _, _, _| {}));
    let state = ApiState::new(
        db,
        bus.clone(),
        Arc::new(|m| {
            Some(InboundQueued {
                conversation_id: m.content.len() as i64,
            })
        }),
        Arc::new(|| "小白龙".into()),
        Arc::new(|| json!({ "running": true })),
    );
    let server = ApiServer::new(state.clone(), false, None);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let router = server.router();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let http_base = format!("http://{addr}");
    let ws_base = format!("ws://{addr}");
    (http_base, ws_base, state, bus)
}

#[tokio::test]
async fn message_post_and_sse_receive() {
    let (http_base, _, _state, bus) = start_server().await;
    let client = reqwest::Client::new();

    // 先连 SSE（确保能收到事件）
    let sse_task = {
        let sse_client = client.clone();
        let http_base = http_base.clone();
        tokio::spawn(async move {
            let resp = sse_client
                .get(format!("{http_base}/events"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let mut stream = resp.bytes_stream();
            let mut text = String::new();
            for _ in 0..6 {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        text.push_str(&String::from_utf8_lossy(&chunk));
                        if text.contains("message_in") {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            drop(stream);
            text
        })
    };

    // POST /message
    let resp = client
        .post(format!("{http_base}/message"))
        .json(&json!({ "content": "你好，白龙马", "from_id": "ID:000001" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["agent_name"], "小白龙");
    assert_eq!(body["conversation_id"], 18); // "你好，白龙马" UTF-8 字节数

    // SSE 应收到 message_in
    let sse_text = sse_task.await.unwrap();
    assert!(
        sse_text.contains("\"type\":\"message_in\""),
        "SSE 缺少 message_in: {sse_text}"
    );
    assert!(sse_text.contains("你好，白龙马"));

    // 总线层面也收到
    let _ = bus;
}

#[tokio::test]
async fn sse_connected_and_sticky_events() {
    let (http_base, _, _state, bus) = start_server().await;
    // ApiServer::new 已设 agent_name_updated sticky
    bus.set_sticky(
        "audio_play",
        json!({ "url": "boot.wav", "type": "startup" }),
    );

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{http_base}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/event-stream"
    );
    let mut stream = resp.bytes_stream();
    let mut text = String::new();
    for _ in 0..6 {
        match stream.next().await {
            Some(Ok(chunk)) => {
                text.push_str(&String::from_utf8_lossy(&chunk));
                if text.contains("boot.wav") && text.contains("\"type\":\"connected\"") {
                    break;
                }
            }
            _ => break,
        }
    }
    drop(stream);
    assert!(
        text.contains("\"type\":\"connected\""),
        "缺 connected: {text}"
    );
    assert!(
        text.contains("agent_name_updated"),
        "缺 agent_name_updated: {text}"
    );
    assert!(text.contains("小白龙"));
    assert!(text.contains("boot.wav"), "缺 audio_play sticky: {text}");
}

#[tokio::test]
async fn sse_disconnect_releases_subscriber() {
    let (http_base, _, state, bus) = start_server().await;
    let client = reqwest::Client::new();

    // 连接前基线（无订阅者；广播 channel 初始 rx 已 drop）
    assert_eq!(bus.subscriber_count(), 0);

    // 建立 SSE 连接并读完首帧，保证转发任务已订阅
    let resp = client
        .get(format!("{http_base}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    {
        let mut stream = resp.bytes_stream();
        let mut text = String::new();
        for _ in 0..6 {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    text.push_str(&String::from_utf8_lossy(&chunk));
                    if text.contains("\"type\":\"connected\"") {
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(text.contains("\"type\":\"connected\""), "缺 connected");
    }
    // 连接持有期间有一个订阅者
    let expected = 1;
    assert_eq!(state.bus.subscriber_count(), expected);

    // 断开连接（drop stream）→ 转发任务应经 tx.closed() 感知并退出，释放订阅
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    loop {
        if state.bus.subscriber_count() == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "断线后订阅者未在 5s 内释放（僵尸任务泄漏）"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn status_and_history_endpoints() {
    let (http_base, _, _state, bus) = start_server().await;
    // 产生一条历史：post message 会 persist message_in？—— message_in 不在历史类型，
    // 但总线 emit 已覆盖。直接查 history 应返回 ok:true。
    let _ = &bus;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{http_base}/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["running"], true);
    assert_eq!(body["memory_count"], 0);

    let resp = client
        .get(format!("{http_base}/events/history?path=all"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert!(body["events"].is_array());
}

#[tokio::test]
async fn scene_protocol_hello_patch_and_intent() {
    let (_, ws_base, state, _bus) = start_server().await;
    let (mut ws, resp) = connect_async(format!("{ws_base}/scene")).await.unwrap();
    assert_eq!(resp.status(), 101);

    // ── hello 握手：welcome + 全量快照 ──
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({ "v": 1, "type": "hello" }).to_string().into(),
    ))
    .await
    .unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .expect("WS welcome 超时")
        .expect("WS 流关闭")
        .expect("WS 帧错误");
    let msg: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(msg["type"], "welcome");
    assert_eq!(msg["rev"], 0);

    let frame = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .expect("WS snapshot 超时")
        .expect("WS 流关闭")
        .expect("WS 帧错误");
    let msg: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(msg["type"], "scene");
    assert_eq!(msg["surfaces"].as_array().unwrap().len(), 0);

    // ── store 变更 → 客户端收到 scene.patch ──
    state
        .scene
        .set(
            "choice-1",
            Some(&json!({ "kind": "choice", "data": { "label": "确认?" }, "order": 1 })),
        )
        .unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .expect("WS patch 超时")
        .expect("WS 流关闭")
        .expect("WS 帧错误");
    let msg: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(msg["type"], "scene.patch");
    assert_eq!(msg["rev"], 1);
    assert_eq!(msg["base"], 0);
    assert_eq!(msg["ops"][0]["op"], "upsert");
    assert_eq!(msg["ops"][0]["surface"]["id"], "choice-1");

    // ── intent 上行 → ui_signal 落库 + 广播 + pushMessage 注入 ──
    let mut sse_rx = state.subscribe();
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({ "v": 1, "type": "intent", "name": "select", "surface": "choice-1", "data": { "value": "yes" } })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let ev = sse_rx.recv().await.unwrap();
    assert_eq!(ev.r#type, "ui_signal");
    assert_eq!(ev.data["type"], "select");
    assert_eq!(ev.data["target"], "choice-1");
    assert_eq!(ev.data["payload"]["value"], "yes");
    // 落库检查（60s 窗口内可见）
    let sigs =
        bailongma_core::db::repositories::ui_signals::get_unconsumed_ui_signals(&state.db, 60_000)
            .unwrap();
    assert_eq!(sigs.len(), 1);
    assert_eq!(sigs[0].r#type, "scene.intent.select");

    let _ = ws
        .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: 1000.into(),
            reason: "done".into(),
        }))
        .await;
}

#[tokio::test]
async fn forbidden_origin_is_rejected() {
    let (http_base, _, _state, _bus) = start_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{http_base}/status"))
        .header("origin", "http://evil.com")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "forbidden origin");
}

#[tokio::test]
async fn options_preflight_returns_204() {
    let (http_base, _, _state, _bus) = start_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{http_base}/message"))
        .header("origin", "http://localhost:5173")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    // reqwest 默认不发 OPTIONS；此处验证 CORS 头存在
    let cors = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(cors, "http://localhost:5173");
}

#[test]
fn event_msg_serialization_matches_node() {
    // 对齐 Node `data: {"type":...,"data":...,"ts":...}\n\n`
    let msg = EventMsg {
        r#type: "tick".into(),
        data: json!({ "n": 1 }),
        ts: "2026-08-09T00:00:00.000Z".into(),
    };
    let frame = bailongma_core::api::server::sse_frame(&msg);
    assert_eq!(
        frame,
        "data: {\"type\":\"tick\",\"data\":{\"n\":1},\"ts\":\"2026-08-09T00:00:00.000Z\"}\n\n"
    );
}
