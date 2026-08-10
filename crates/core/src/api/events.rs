//! SSE 事件总线 —— 客户端管理 + 事件广播 + 粘性事件。
//!
//! 对齐 Node 版 `src/events.js`：
//! - `emit` 广播给所有已订阅 SSE 客户端（格式 `data: {type,data,ts}\n\n`）
//! - 粘性事件：新客户端连接时补发（如启动自检音效）
//! - brain-ui 观测历史：L1/L2 路径状态机 + best-effort 落库
//!
//! Rust 侧用 `tokio::sync::broadcast` 分发；SSE 响应层负责把订阅到的
//! 消息写成 SSE 帧。总线本身与 HTTP 层解耦，可独立测试。

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::broadcast;

/// 一次广播的事件负载。
#[derive(Debug, Clone)]
pub struct EventMsg {
    pub r#type: String,
    pub data: Value,
    pub ts: String,
}

/// brain-ui 历史落库回调：`(ts, path, type, payload)`，失败静默。
/// 由 server 层注入（需要 Db 连接）；纯逻辑层可注入 no-op。
pub type PersistFn = Arc<dyn Fn(String, &str, &str, &Value) + Send + Sync>;

/// 事件总线。
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Inner>,
}

struct Inner {
    /// 广播通道（容量 256，慢客户端丢帧 —— 与 Node 的 fire-and-forget 一致）
    tx: broadcast::Sender<EventMsg>,
    /// 粘性事件：type → (data, ts)，顺序保持
    sticky: Mutex<Vec<(String, EventMsg)>>,
    /// brain-ui 历史落库（best-effort）
    persist: PersistFn,
    /// 当前观测路径（l1/l2/None）—— Node 版模块级变量的对应
    path: Mutex<Option<&'static str>>,
}

impl EventBus {
    /// 创建总线。`persist` 用于 brain-ui 历史落库（可传 no-op）。
    pub fn new(persist: PersistFn) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                tx,
                sticky: Mutex::new(Vec::new()),
                persist,
                path: Mutex::new(None),
            }),
        }
    }

    /// 订阅事件流（返回接收端）。
    pub fn subscribe(&self) -> broadcast::Receiver<EventMsg> {
        self.inner.tx.subscribe()
    }

    /// 当前订阅者数（测试用：验证断线后订阅者被释放）。
    pub fn subscriber_count(&self) -> usize {
        self.inner.tx.receiver_count()
    }

    /// 广播事件（对齐 emitEvent：先走 brain-ui 历史，再广播）。
    /// 注意：内部用 `std::sync::Mutex`（短临界区、不跨 await），可在 async 上下文安全调用。
    pub fn emit(&self, r#type: &str, data: Value) {
        let ts = iso_now();
        // brain-ui 历史：路径状态机（对齐 persistBrainUiEvent）
        self.persist_brain_ui(r#type, &data, &ts);
        let msg = EventMsg {
            r#type: r#type.to_string(),
            data,
            ts,
        };
        let _ = self.inner.tx.send(msg);
    }

    /// 设置粘性事件（对齐 setStickyEvent；同 type 覆盖）。
    pub fn set_sticky(&self, r#type: &str, data: Value) {
        let ts = iso_now();
        let msg = EventMsg {
            r#type: r#type.to_string(),
            data,
            ts,
        };
        let mut sticky = self.inner.sticky.lock().unwrap();
        if let Some(slot) = sticky.iter_mut().find(|(t, _)| t == r#type) {
            slot.1 = msg;
        } else {
            sticky.push((r#type.to_string(), msg));
        }
    }

    /// 清除粘性事件（对齐 clearStickyEvent）。
    pub fn clear_sticky(&self, r#type: &str) {
        let mut sticky = self.inner.sticky.lock().unwrap();
        sticky.retain(|(t, _)| t != r#type);
    }

    /// 取全部粘性事件（连接建立时补发，对齐 flushStickyEvents）。
    pub fn flush_sticky(&self) -> Vec<EventMsg> {
        self.inner
            .sticky
            .lock()
            .unwrap()
            .iter()
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// brain-ui 观测历史状态机（对齐 persistBrainUiEvent）。
    fn persist_brain_ui(&self, r#type: &str, data: &Value, ts: &str) {
        const HISTORY_TYPES: &[&str] = &[
            "message_received",
            "tick",
            "stream_start",
            "stream_end",
            "tool_preparing",
            "tool_executing",
            "tool_call",
            "response",
            "processing_preempted",
            "llm_retry",
            "message_requeued",
            "message_dropped",
            "error",
            "protocol_violation",
        ];
        let mut path = self.inner.path.lock().unwrap();
        // message_received：若当前在 l2，先补一条 preemption
        if r#type == "message_received" {
            if *path == Some("l2") {
                (self.inner.persist)(
                    ts.to_string(),
                    "l2",
                    "processing_preempted",
                    &json!({ "reason": "收到用户消息，心跳让路" }),
                );
            }
            *path = Some("l1");
            (self.inner.persist)(ts.to_string(), "l1", r#type, data);
            return;
        }
        if r#type == "tick" {
            *path = Some("l2");
        }
        let event_path = *path;
        let should_persist =
            matches!(event_path, Some("l1") | Some("l2")) && HISTORY_TYPES.contains(&r#type);
        if should_persist {
            (self.inner.persist)(ts.to_string(), event_path.unwrap_or("l2"), r#type, data);
        }
        if matches!(
            r#type,
            "response" | "processing_preempted" | "message_dropped" | "protocol_violation"
        ) {
            *path = None;
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(Arc::new(|_, _, _, _| {}))
    }
}

/// 当前 UTC ISO 时间戳（对齐 new Date().toISOString()）。
pub fn iso_now() -> String {
    // 简单实现：从 epoch 秒换算，格式 YYYY-MM-DDTHH:MM:SS.mmmZ
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.as_millis() as u64;
    let secs = millis / 1000;
    let ms = millis % 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 是周四；用 civil_from_days 换算
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

/// 儒略日 → 公历（Howard Hinnant 算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_timestamp_format() {
        let s = iso_now();
        assert!(s.ends_with('Z'));
        assert!(s.contains('T'));
        // 长度约 24 字符：2026-08-09T03:29:44.600Z
        assert_eq!(s.len(), 24);
    }

    #[tokio::test]
    async fn emit_reaches_subscribers() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        bus.emit("tick", json!({"n": 1}));
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.r#type, "tick");
        assert_eq!(msg.data["n"], 1);
        assert_eq!(msg.ts.len(), 24);
    }

    #[tokio::test]
    async fn sticky_flushed_on_subscribe() {
        let bus = EventBus::default();
        bus.set_sticky("agent_name_updated", json!({"name": "小白龙"}));
        let flushed = bus.flush_sticky();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].r#type, "agent_name_updated");
        assert_eq!(flushed[0].data["name"], "小白龙");
        // 覆盖同 type
        bus.set_sticky("agent_name_updated", json!({"name": "小黑龙"}));
        let flushed = bus.flush_sticky();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].data["name"], "小黑龙");
        // 清除
        bus.clear_sticky("agent_name_updated");
        assert!(bus.flush_sticky().is_empty());
    }

    #[test]
    fn brain_ui_path_state_machine() {
        let persisted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = persisted.clone();
        let bus = EventBus::new(Arc::new(move |ts, path, ty, payload| {
            sink.lock()
                .unwrap()
                .push((ts, path.to_string(), ty.to_string(), payload.clone()));
        }));
        // message_received → l1
        bus.emit("message_received", json!({"id": 1}));
        // tick → l2
        bus.emit("tick", json!({}));
        // 非历史类型（如 ui_signal）不落库
        bus.emit("ui_signal", json!({}));
        // response → 落库并重置路径
        bus.emit("response", json!({"t": "ok"}));
        // 路径已重置 → tick 仍会设回 l2 并落库（对齐 Node：tick 总是设 l2）
        bus.emit("tick", json!({}));

        let got = persisted.lock().unwrap().clone();
        // [message_received/l1, tick/l2, response/l2, tick/l2]
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].1, "l1");
        assert_eq!(got[0].2, "message_received");
        assert_eq!(got[1].1, "l2");
        assert_eq!(got[1].2, "tick");
        assert_eq!(got[2].1, "l2");
        assert_eq!(got[2].2, "response");
        assert_eq!(got[3].1, "l2");
        assert_eq!(got[3].2, "tick");
    }

    #[test]
    fn message_received_while_l2_emits_preemption() {
        let persisted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = persisted.clone();
        let bus = EventBus::new(Arc::new(move |ts, path, ty, payload| {
            sink.lock()
                .unwrap()
                .push((ts, path.to_string(), ty.to_string(), payload.clone()));
        }));
        bus.emit("tick", json!({})); // → l2
        bus.emit("message_received", json!({"id": 2})); // 应补 preemption + l1 message_received
        let got = persisted.lock().unwrap().clone();
        // 顺序：tick(l2) → preemption(l2) → message_received(l1)
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].1, "l2");
        assert_eq!(got[0].2, "tick");
        assert_eq!(got[1].1, "l2");
        assert_eq!(got[1].2, "processing_preempted");
        assert_eq!(got[2].1, "l1");
        assert_eq!(got[2].2, "message_received");
    }
}
