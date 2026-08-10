//! SceneStore —— Agent 驱动 UI 的唯一真相源。
//!
//! 对齐 Node 版 `src/scene/scene-store.js`：
//! - 持有当前场景（surfaces + 单调递增的 rev），通过幂等的 `set(id, surface|null)` 变更
//! - 与传输层解耦：变更通过 broadcast 通道通知订阅者，由传输层（`api/scene.rs`）转成协议消息
//! - `set_many` 提供原子批量更新：一次 rev 递增 + 单条 Patch 广播（含多个 upsert/remove），
//!   避免多 surface 依次更新时的中间态闪烁
//!
//! 协议见仓库根目录 SCENE-PROTOCOL.md；理念见桌面《Agent-驱动UI-设计方案.md》。

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::error::{CoreError, Result};

/// 允许的 intent（对齐 ALLOWED_INTENTS）。
const ALLOWED_INTENTS: [&str; 3] = ["ambient", "inform", "confront"];

/// 单块 surface（对齐 Node normalizeSurface 的字段顺序）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Surface {
    pub id: String,
    pub kind: String,
    /// 仅接受 plain object（对齐 isPlainObject 校验）
    #[serde(default = "empty_object")]
    pub data: Value,
    /// 默认 'inform'
    #[serde(default = "default_intent")]
    pub intent: String,
    /// 仅 true 时序列化（对齐 Node 条件赋值）
    #[serde(default, skip_serializing_if = "is_false")]
    pub focus: bool,
    /// 仅 number 时序列化
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
}

fn empty_object() -> Value {
    json!({})
}
fn default_intent() -> String {
    "inform".into()
}
fn is_false(v: &bool) -> bool {
    !*v
}

/// 一次 store 变更（订阅者收到的通知）。
#[derive(Debug, Clone)]
pub enum SceneOp {
    Upsert(Surface),
    Remove(String),
    /// 原子批量：多个 upsert/remove 作为一次变更广播（不含嵌套 Patch）。
    Patch(Vec<SceneOp>),
    Clear,
}

struct SceneInner {
    /// 插入序保持的 surfaces（id -> surface）
    surfaces: Vec<(String, Surface)>,
    /// 单调递增版本号，初始 0
    rev: u64,
}

/// Agent 驱动 UI 的真相源（内部 `Arc<Mutex>`，可 Clone 共享）。
#[derive(Clone)]
pub struct SceneStore {
    inner: Arc<Mutex<SceneInner>>,
    /// 变更通知通道（无订阅者时 send 失败可忽略）
    tx: broadcast::Sender<SceneOp>,
}

impl Default for SceneStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SceneStore {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Mutex::new(SceneInner {
                surfaces: Vec::new(),
                rev: 0,
            })),
            tx,
        }
    }

    /// 订阅场景变更，返回接收端（对齐 subscribe / _emit）。
    pub fn subscribe(&self) -> broadcast::Receiver<SceneOp> {
        self.tx.subscribe()
    }

    /// 当前版本号。
    pub fn rev(&self) -> u64 {
        self.inner.lock().unwrap().rev
    }

    /// 幂等 upsert / remove。返回是否真的发生了变化。
    /// `input = None` 移除该 surface；`Some(object)` 插入或整体替换。
    /// 对齐 `set(id, input)`：非法 id / 缺 kind 返回错误（Node 为 throw）。
    pub fn set(&self, id: &str, input: Option<&Value>) -> Result<bool> {
        if id.is_empty() {
            return Err(CoreError::Other("scene.set: id 必须是非空字符串".into()));
        }
        let mut inner = self.inner.lock().unwrap();

        // 置空 = 移除
        if input.is_none() {
            if !inner.surfaces.iter().any(|(i, _)| i == id) {
                return Ok(false); // 无变化，不 bump
            }
            inner.surfaces.retain(|(i, _)| i != id);
            inner.rev += 1;
            let op = SceneOp::Remove(id.to_string());
            let _ = self.tx.send(op.clone());
            return Ok(true);
        }

        let input = input.expect("checked above");
        let kind = input.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        if kind.is_empty() {
            return Err(CoreError::Other(
                "scene.set: surface 必须含字符串 kind".into(),
            ));
        }
        let next = normalize_surface(id, input);
        // 幂等：内容无变化 → 不 bump、不广播
        if let Some((_, prev)) = inner.surfaces.iter().find(|(i, _)| i == id) {
            if *prev == next {
                return Ok(false);
            }
        }
        // 存在则原位替换（保持插入序），否则追加
        if let Some(slot) = inner.surfaces.iter_mut().find(|(i, _)| i == id) {
            *slot = (id.to_string(), next.clone());
        } else {
            inner.surfaces.push((id.to_string(), next.clone()));
        }
        inner.rev += 1;
        let op = SceneOp::Upsert(next);
        let _ = self.tx.send(op.clone());
        Ok(true)
    }

    /// 原子批量更新：`(id, Option<Value>)` 列表一次应用，rev 只递增 1，
    /// 广播单条 `SceneOp::Patch`（含实际发生的 upsert/remove）。
    ///
    /// - `None` 表示移除；`Some` 必须含非空 kind
    /// - 幂等项跳过；全部无变化 → `Ok(false)`，不 bump 不广播
    /// - 任一输入非法 → 整体 `Err`，已合法项也不应用（原子性）
    pub fn set_many(&self, inputs: &[(String, Option<Value>)]) -> Result<bool> {
        if inputs.is_empty() {
            return Ok(false);
        }
        // 第一遍：全量校验（原子性前提——校验不过整体拒绝）
        for (id, input) in inputs {
            if id.is_empty() {
                return Err(CoreError::Other(
                    "scene.set_many: id 必须是非空字符串".into(),
                ));
            }
            if let Some(v) = input {
                let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                if kind.is_empty() {
                    return Err(CoreError::Other(
                        "scene.set_many: surface 必须含字符串 kind".into(),
                    ));
                }
            }
        }

        let mut inner = self.inner.lock().unwrap();
        let mut ops: Vec<SceneOp> = Vec::new();

        for (id, input) in inputs {
            match input {
                None => {
                    if inner.surfaces.iter().any(|(i, _)| i == id) {
                        inner.surfaces.retain(|(i, _)| i != id);
                        ops.push(SceneOp::Remove(id.clone()));
                    }
                }
                Some(v) => {
                    let next = normalize_surface(id, v);
                    // 幂等：内容无变化 → 跳过
                    if let Some((_, prev)) = inner.surfaces.iter().find(|(i, _)| i == id) {
                        if *prev == next {
                            continue;
                        }
                    }
                    if let Some(slot) = inner.surfaces.iter_mut().find(|(i, _)| i == id) {
                        *slot = (id.clone(), next.clone());
                    } else {
                        inner.surfaces.push((id.clone(), next.clone()));
                    }
                    ops.push(SceneOp::Upsert(next));
                }
            }
        }

        if ops.is_empty() {
            return Ok(false); // 全部幂等命中，无变化
        }
        inner.rev += 1;
        let op = SceneOp::Patch(ops);
        let _ = self.tx.send(op.clone());
        Ok(true)
    }

    /// 读取单个 surface 的规范化副本（只读；不存在返回 None）。
    pub fn get(&self, id: &str) -> Option<Surface> {
        let inner = self.inner.lock().unwrap();
        inner
            .surfaces
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, s)| s.clone())
    }

    /// 当前全量快照（协议 §3.1 的 scene 消息体）。
    pub fn snapshot(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let surfaces = ordered_surfaces(&inner.surfaces);
        json!({
            "v": 1,
            "type": "scene",
            "rev": inner.rev,
            "surfaces": surfaces,
        })
    }

    /// 紧凑清单，供回注 Agent 上下文（只给 id/kind/intent/focus）。
    pub fn manifest(&self) -> Vec<Value> {
        let inner = self.inner.lock().unwrap();
        ordered_surfaces(&inner.surfaces)
            .into_iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "kind": s.kind,
                    "intent": s.intent,
                    "focus": s.focus,
                })
            })
            .collect()
    }

    /// 清空全部 surface（广播一次 Clear，传输层回退为全量快照）。
    pub fn clear(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.surfaces.is_empty() {
            return false;
        }
        inner.surfaces.clear();
        inner.rev += 1;
        let _ = self.tx.send(SceneOp::Clear);
        true
    }

    /// 把一次变更转成协议消息（对齐 scene-server 的 ensureSubscribed 回调）：
    /// upsert / remove / patch → 增量补丁 `scene.patch`；其他（clear）→ 全量快照。
    pub fn protocol_message(&self, op: &SceneOp) -> Value {
        match op {
            SceneOp::Upsert(surface) => json!({
                "v": 1,
                "type": "scene.patch",
                "rev": self.rev(),
                "base": self.rev().saturating_sub(1),
                "ops": [json!({ "op": "upsert", "surface": surface })],
            }),
            SceneOp::Remove(id) => json!({
                "v": 1,
                "type": "scene.patch",
                "rev": self.rev(),
                "base": self.rev().saturating_sub(1),
                "ops": [json!({ "op": "remove", "id": id })],
            }),
            SceneOp::Patch(ops) => {
                let base = self.rev().saturating_sub(1);
                let json_ops: Vec<Value> = ops
                    .iter()
                    .map(|o| match o {
                        SceneOp::Upsert(s) => json!({ "op": "upsert", "surface": s }),
                        SceneOp::Remove(id) => json!({ "op": "remove", "id": id }),
                        _ => unreachable!("SceneOp::Patch 内只含 upsert/remove"),
                    })
                    .collect();
                json!({
                    "v": 1,
                    "type": "scene.patch",
                    "rev": self.rev(),
                    "base": base,
                    "ops": json_ops,
                })
            }
            SceneOp::Clear => self.snapshot(),
        }
    }
}

/// 规范化一个 surface，丢弃非法 / 未知形态（对齐 normalizeSurface）。
fn normalize_surface(id: &str, input: &Value) -> Surface {
    let data = if input.get("data").is_some_and(|d| d.is_object()) {
        input["data"].clone()
    } else {
        json!({})
    };
    let intent = match input.get("intent").and_then(|v| v.as_str()) {
        Some(s) if ALLOWED_INTENTS.contains(&s) => s.to_string(),
        _ => "inform".to_string(),
    };
    let focus = input.get("focus").is_some_and(|f| f == &json!(true));
    let order = input.get("order").and_then(|o| o.as_i64());
    Surface {
        id: id.to_string(),
        kind: input["kind"].as_str().unwrap_or("").to_string(),
        data,
        intent,
        focus,
        order,
    }
}

/// 按 order 升序排列（无 order 视为 0），order 相同保持插入序（稳定）。
fn ordered_surfaces(surfaces: &[(String, Surface)]) -> Vec<Surface> {
    let mut items: Vec<(i64, usize, Surface)> = surfaces
        .iter()
        .enumerate()
        .map(|(i, (_, s))| (s.order.unwrap_or(0), i, s.clone()))
        .collect();
    items.sort_by_key(|(o, i, _)| (*o, *i));
    items.into_iter().map(|(_, _, s)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SceneStore {
        SceneStore::new()
    }

    #[test]
    fn set_upsert_and_idempotent() {
        let s = store();
        let v = json!({ "kind": "text", "data": { "text": "hi" }, "intent": "inform" });
        assert!(s.set("a", Some(&v)).unwrap());
        assert_eq!(s.rev(), 1);
        // 幂等：同内容不 bump
        assert!(!s.set("a", Some(&v)).unwrap());
        assert_eq!(s.rev(), 1);
        // 同 id 不同 kind → 变更
        assert!(s.set("a", Some(&json!({ "kind": "metric" }))).unwrap());
        assert_eq!(s.rev(), 2);
        let surf = s.get("a").unwrap();
        assert_eq!(surf.kind, "metric");
        // 缺省值对齐 Node：data={}、intent='inform'、focus/order 缺省
        assert_eq!(surf.data, json!({}));
        assert_eq!(surf.intent, "inform");
        assert!(!surf.focus);
        assert_eq!(surf.order, None);
    }

    #[test]
    fn set_remove_and_clear() {
        let s = store();
        s.set("a", Some(&json!({ "kind": "text" }))).unwrap();
        s.set("b", Some(&json!({ "kind": "text" }))).unwrap();
        // remove
        assert!(s.set("a", None).unwrap());
        assert!(s.get("a").is_none());
        // 重复 remove → false
        assert!(!s.set("a", None).unwrap());
        assert_eq!(s.rev(), 3); // set a + set b + remove a
                                // clear
        assert!(s.clear());
        assert_eq!(s.manifest().len(), 0);
        assert!(!s.clear()); // 空 → false
        assert_eq!(s.rev(), 4);
    }

    #[test]
    fn normalize_drops_bad_fields() {
        let s = store();
        // intent 不在白名单 → 'inform'；data 非对象 → {}；focus 非 true → false
        let v = json!({
            "kind": "choice",
            "data": [1, 2, 3],
            "intent": "evil",
            "focus": "yes",
            "order": "3"
        });
        s.set("c", Some(&v)).unwrap();
        let surf = s.get("c").unwrap();
        assert_eq!(surf.intent, "inform");
        assert_eq!(surf.data, json!({}));
        assert!(!surf.focus);
        assert_eq!(surf.order, None);
    }

    #[test]
    fn set_rejects_bad_id_or_kind() {
        let s = store();
        assert!(s.set("", Some(&json!({ "kind": "text" }))).is_err());
        assert!(s.set("x", Some(&json!({ "data": {} }))).is_err());
    }

    #[test]
    fn snapshot_is_ordered_by_order_then_insertion() {
        let s = store();
        s.set("z", Some(&json!({ "kind": "text", "order": 2 })))
            .unwrap();
        s.set("a", Some(&json!({ "kind": "text" }))).unwrap(); // order 0
        s.set("m", Some(&json!({ "kind": "text", "order": 1 })))
            .unwrap();
        s.set("b", Some(&json!({ "kind": "text" }))).unwrap(); // order 0，插入在 a 后
        let snap = s.snapshot();
        assert_eq!(snap["v"], 1);
        assert_eq!(snap["type"], "scene");
        assert_eq!(snap["rev"], 4);
        let ids: Vec<&str> = snap["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "b", "m", "z"]);
        // 字段顺序固定 id → kind → data → intent（序列化顺序）
        let first = snap["surfaces"][0].to_string();
        assert!(first.starts_with(r#"{"id":"a","kind":"text","data":{},"#));
    }

    #[test]
    fn manifest_only_exposes_meta() {
        let s = store();
        s.set("a", Some(&json!({ "kind": "text", "focus": true })))
            .unwrap();
        let m = s.manifest();
        assert_eq!(m.len(), 1);
        assert_eq!(
            m[0],
            json!({ "id": "a", "kind": "text", "intent": "inform", "focus": true })
        );
    }

    #[test]
    fn subscribe_receives_ops_and_protocol_message() {
        let s = store();
        let mut rx = s.subscribe();
        s.set("a", Some(&json!({ "kind": "text" }))).unwrap();
        let op = rx.try_recv().unwrap();
        match op {
            SceneOp::Upsert(_) => {}
            _ => panic!("expected upsert"),
        }
        let msg = s.protocol_message(&op);
        assert_eq!(msg["type"], "scene.patch");
        assert_eq!(msg["rev"], 1);
        assert_eq!(msg["base"], 0);
        assert_eq!(msg["ops"][0]["op"], "upsert");
        // remove → patch；clear → 全量快照
        s.set("a", None).unwrap();
        let op = rx.try_recv().unwrap();
        let msg = s.protocol_message(&op);
        assert_eq!(msg["ops"][0]["op"], "remove");
        s.set("b", Some(&json!({ "kind": "x" }))).unwrap();
        let _ = rx.try_recv().unwrap();
        s.clear();
        let op = rx.try_recv().unwrap();
        let msg = s.protocol_message(&op);
        assert_eq!(msg["type"], "scene");
        assert_eq!(msg["rev"], 4); // set a(1) + remove(2) + set b(3) + clear(4)
    }

    #[test]
    fn set_many_applies_batch_and_emits_single_patch() {
        let s = store();
        s.set("a", Some(&json!({ "kind": "text" }))).unwrap();
        // 批量：更新 a、新增 b、移除 c（不存在 → 忽略，不进 ops）
        let changed = s
            .set_many(&[
                ("a".into(), Some(json!({ "kind": "metric" }))),
                ("b".into(), Some(json!({ "kind": "text" }))),
                ("c".into(), None),
            ])
            .unwrap();
        assert!(changed);
        assert_eq!(s.rev(), 2); // set a(1) + batch(2)
        assert_eq!(s.get("a").unwrap().kind, "metric");
        assert!(s.get("b").is_some());
        assert!(s.get("c").is_none());

        // 订阅者收到一条 Patch，含 2 个 op
        let mut rx = s.subscribe();
        s.set_many(&[("d".into(), Some(json!({ "kind": "x" })))])
            .unwrap();
        match rx.try_recv().unwrap() {
            SceneOp::Patch(ops) => assert_eq!(ops.len(), 1),
            _ => panic!("expected Patch"),
        }
        // 无变化 → false，不广播
        assert!(
            !s.set_many(&[("d".into(), Some(json!({ "kind": "x" })))])
                .unwrap()
        );
    }

    #[test]
    fn set_many_rejects_bad_input_atomically() {
        let s = store();
        s.set("a", Some(&json!({ "kind": "text" }))).unwrap();
        // 列表中有非法项（缺 kind）→ 整体报错，合法项也不应用
        let err = s.set_many(&[
            ("b".into(), Some(json!({ "kind": "text" }))),
            ("c".into(), Some(json!({ "data": {} }))), // 缺 kind
        ]);
        assert!(err.is_err());
        assert!(s.get("b").is_none()); // 未应用
        assert_eq!(s.rev(), 1); // 未 bump
        // 空 id 同样整体拒绝
        assert!(
            s.set_many(&[("".into(), Some(json!({ "kind": "text" })))])
                .is_err()
        );
        // 空列表 → false 无副作用
        assert!(!s.set_many(&[]).unwrap());
    }

    #[test]
    fn set_many_patch_protocol_message() {
        let s = store();
        let mut rx = s.subscribe();
        s.set_many(&[
            ("a".into(), Some(json!({ "kind": "text" }))),
            ("b".into(), Some(json!({ "kind": "metric", "order": 1 }))),
        ])
        .unwrap();
        let op = rx.try_recv().unwrap();
        let msg = s.protocol_message(&op);
        assert_eq!(msg["type"], "scene.patch");
        assert_eq!(msg["rev"], 1);
        assert_eq!(msg["base"], 0);
        assert_eq!(msg["ops"].as_array().unwrap().len(), 2);
        assert_eq!(msg["ops"][0]["op"], "upsert");
        assert_eq!(msg["ops"][0]["surface"]["id"], "a");
        assert_eq!(msg["ops"][1]["op"], "upsert");
        assert_eq!(msg["ops"][1]["surface"]["order"], 1);
        // 快照视角与批量一致
        let snap = s.snapshot();
        assert_eq!(snap["rev"], 1);
        assert_eq!(snap["surfaces"].as_array().unwrap().len(), 2);
    }
}
