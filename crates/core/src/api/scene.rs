//! Scene 传输层 —— /scene WebSocket 协议处理器。
//!
//! 对齐 Node 版 `src/scene/scene-server.js` + `src/api.js` 注入的 intent 处理器：
//! - 下行：hello → `welcome` + 全量快照；resync → 全量快照；store 变更 →
//!   `scene.patch`（upsert/remove）或全量 `scene`（clear）
//! - 上行：intent → 落库 ui_signal + 广播 `ui_signal` + 非 passive intent
//!   经 pushMessage 注入意识循环；pong / 未知消息忽略（向前兼容）
//! - 空闲超时 60s（对齐 attachWebSocketIdleTimeout）

use std::time::Duration;

use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::routes::{ApiState, InboundMessage};
use crate::db::repositories::ui_signals;

/// 空闲超时（对齐 attachWebSocketIdleTimeout 60s）。
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// 被动 intent：仅记录，不注入意识循环（对齐 SCENE_PASSIVE_INTENTS）。
const SCENE_PASSIVE_INTENTS: [&str; 4] = ["dismiss", "ended", "mounted", "dwell"];

/// 处理一条 /scene 连接（对齐 handleSceneConnection）。
pub async fn handle_scene_connection(state: ApiState, socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();
    let mut scene_rx = state.scene.subscribe();
    let mut ready = false;

    // 空闲计时（对齐 attachWebSocketIdleTimeout：60s 无消息则断开）
    let mut idle = tokio::time::interval(WS_IDLE_TIMEOUT);
    idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    idle.tick().await; // interval 首 tick 立即返回，用作启动计时

    loop {
        tokio::select! {
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let replies = handle_scene_text(&state, &text, &mut ready);
                        let mut sent = true;
                        for r in &replies {
                            if sender.send(text_frame(r)).await.is_err() {
                                sent = false;
                                break;
                            }
                        }
                        if !sent { break; }
                        idle.reset();
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = sender.send(Message::Pong(p)).await;
                        idle.reset();
                    }
                    Some(Ok(_)) => {
                        idle.reset();
                    }
                    Some(Err(_)) | None => break,
                }
            }
            op = scene_rx.recv() => {
                if !ready { continue; }
                match op {
                    Ok(op) => {
                        let msg = state.scene.protocol_message(&op);
                        if sender.send(text_frame(&msg)).await.is_err() {
                            break;
                        }
                    }
                    // 订阅落后（丢帧）：回退为全量快照（对齐 store 变更回退逻辑）
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let msg = state.scene.snapshot();
                        if sender.send(text_frame(&msg)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
            _ = idle.tick() => break,
        }
    }
    let _ = sender.send(Message::Close(None)).await;
}

/// 处理一条上行文本消息；`ready` 在收到 hello 后置 true（对齐 client.ready）。
/// 返回需要下发的消息（welcome / 快照等），由连接层统一发送。
fn handle_scene_text(state: &ApiState, text: &str, ready: &mut bool) -> Vec<Value> {
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(), // 非法 JSON 忽略
    };
    if msg.get("v") != Some(&json!(1)) {
        return Vec::new(); // 未知版本忽略
    }
    match msg.get("type").and_then(|t| t.as_str()) {
        Some("hello") => {
            // 握手：标记就绪，回 welcome + 全量快照，使 shell 与真相源对齐
            *ready = true;
            let rev = state.scene.rev();
            vec![
                json!({ "v": 1, "type": "welcome", "rev": rev }),
                state.scene.snapshot(),
            ]
        }
        Some("resync") => {
            // shell 检测到漏帧 / 初始化：重发全量快照
            vec![state.scene.snapshot()]
        }
        Some("intent") => {
            // 处理器出错不影响连接（对齐 try/catch 包裹）
            handle_intent(state, &msg);
            Vec::new()
        }
        Some("pong") | None | Some(_) => {
            // 未知 type 忽略（向前兼容）
            Vec::new()
        }
    }
}

/// intent 上行处理（对齐 api.js 的 setSceneIntentHandler）。
fn handle_intent(state: &ApiState, msg: &Value) {
    let surface = msg
        .get("surface")
        .and_then(|v| v.as_str())
        .unwrap_or("scene");
    let name = msg
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let data = msg.get("data").cloned().unwrap_or_else(|| json!({}));
    let ts = msg
        .get("ts")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(ui_signals::now_epoch_ms);

    // 1. 落库 ui_signal（对齐 insertUISignal）
    let signal_id = ui_signals::insert_ui_signal(
        &state.db,
        &format!("scene.intent.{name}"),
        Some(surface),
        &data,
        ts,
    )
    .unwrap_or_else(|e| {
        tracing::warn!("[scene] insertUISignal failed: {e}");
        0
    });

    // 2. 广播 ui_signal（对齐 emitEvent）
    state.bus.emit(
        "ui_signal",
        json!({
            "id": signal_id,
            "type": name,
            "target": surface,
            "payload": data,
        }),
    );

    // 3. security-confirm 特殊处理：移除该 surface（对齐 api.js 行为）。
    //    安全配置更新（setSecurity）依赖配置子系统，M5+ 接入。
    if name == "select" && surface.starts_with("security-confirm-") {
        let _ = state.scene.set(surface, None);
    }

    // 4. 非 passive intent → pushMessage 注入意识循环（对齐 SCENE_PASSIVE_INTENTS 判断）
    if !SCENE_PASSIVE_INTENTS.contains(&name) {
        let pretty = serde_json::to_string_pretty(&data).unwrap_or_else(|_| "{}".into());
        let content = format!("[UI intent surface={surface} name={name}]\n{pretty}");
        let inbound = InboundMessage {
            from_id: format!("UI:{surface}"),
            content,
            channel: "APP_SIGNAL".into(),
            meta: json!({}),
        };
        let _ = (state.inbound)(inbound);
    }
}

fn text_frame(msg: &Value) -> Message {
    Message::Text(Utf8Bytes::from(msg.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::events::EventBus;
    use crate::api::routes::InboundQueued;
    use crate::db::Db;
    use std::sync::{Arc, Mutex};

    fn test_state() -> (ApiState, Arc<Mutex<Vec<String>>>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("test.db")).unwrap();
        let bus = EventBus::new(Arc::new(|_, _, _, _| {}));
        let inbound_log = Arc::new(Mutex::new(Vec::new()));
        let log = inbound_log.clone();
        let state = ApiState::new(
            db,
            bus,
            Arc::new(move |m: InboundMessage| {
                log.lock().unwrap().push(m.content.clone());
                Some(InboundQueued { conversation_id: 1 })
            }),
            Arc::new(|| "小白龙".into()),
            Arc::new(|| json!({ "running": true })),
        );
        (state, inbound_log)
    }

    #[test]
    fn hello_returns_welcome_and_snapshot() {
        let (state, _) = test_state();
        state
            .scene
            .set("a", Some(&json!({ "kind": "text" })))
            .unwrap();
        let mut ready = false;
        let replies = handle_scene_text(&state, r#"{"v":1,"type":"hello"}"#, &mut ready);
        assert!(ready);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["type"], "welcome");
        assert_eq!(replies[0]["rev"], 1);
        assert_eq!(replies[1]["type"], "scene");
        assert_eq!(replies[1]["rev"], 1);
        assert_eq!(replies[1]["surfaces"][0]["id"], "a");
    }

    #[test]
    fn resync_returns_snapshot_only() {
        let (state, _) = test_state();
        state
            .scene
            .set("a", Some(&json!({ "kind": "text" })))
            .unwrap();
        let mut ready = false;
        let replies = handle_scene_text(&state, r#"{"v":1,"type":"resync"}"#, &mut ready);
        assert!(!ready); // resync 不改变就绪状态
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["type"], "scene");
    }

    #[test]
    fn bad_version_or_json_is_ignored() {
        let (state, _) = test_state();
        let mut ready = false;
        assert!(handle_scene_text(&state, "not json", &mut ready).is_empty());
        assert!(handle_scene_text(&state, r#"{"v":2,"type":"hello"}"#, &mut ready).is_empty());
        assert!(!ready);
    }

    #[tokio::test]
    async fn intent_persists_broadcasts_and_pushes() {
        let (state, inbound_log) = test_state();
        let mut rx = state.subscribe();

        let mut ready = false;
        let replies = handle_scene_text(
            &state,
            r#"{"v":1,"type":"intent","name":"select","surface":"choice-1","data":{"value":"a"}}"#,
            &mut ready,
        );
        assert!(replies.is_empty()); // intent 无下行回复

        // 广播 ui_signal
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.r#type, "ui_signal");
        assert_eq!(msg.data["type"], "select");
        assert_eq!(msg.data["target"], "choice-1");

        // 落库 ui_signal
        let sigs = ui_signals::get_unconsumed_ui_signals(&state.db, 60_000).unwrap();
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].r#type, "scene.intent.select");

        // pushMessage 注入（非 passive）
        let pushed = inbound_log.lock().unwrap().clone();
        assert_eq!(pushed.len(), 1);
        assert!(pushed[0].starts_with("[UI intent surface=choice-1 name=select]"));
    }

    #[test]
    fn passive_intent_does_not_push() {
        let (state, inbound_log) = test_state();
        let mut ready = false;
        handle_scene_text(
            &state,
            r#"{"v":1,"type":"intent","name":"mounted","surface":"s1","data":{}}"#,
            &mut ready,
        );
        assert!(inbound_log.lock().unwrap().is_empty());
    }

    #[test]
    fn security_confirm_select_removes_surface() {
        let (state, _) = test_state();
        state
            .scene
            .set(
                "security-confirm-1",
                Some(&json!({ "kind": "choice", "data": { "pending": {} } })),
            )
            .unwrap();
        let mut ready = false;
        handle_scene_text(
            &state,
            r#"{"v":1,"type":"intent","name":"select","surface":"security-confirm-1","data":{"value":"confirm"}}"#,
            &mut ready,
        );
        assert!(state.scene.get("security-confirm-1").is_none());
    }
}
