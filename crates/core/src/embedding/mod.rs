//! 向量嵌入抽象（对齐 `src/embedding.js` + `src/embedding-local.js` 的接口面）。
//!
//! M4 检索管线通过 `dyn Embedder` 取向量，屏蔽具体实现：
//! - `NoopEmbedder`：当前唯一实现，`compute` 恒返回 `None` ——
//!   管线据此走「纯 FTS5 + LIKE」路径（与 Node 版 embedder 不可用时行为一致）。
//! - 后续接入本地 ONNX（`embedding-local.js` 的 Xenova/bge-large-zh-v1.5）或云端
//!   OpenAI 时新增实现即可，检索层无需改动。

/// 一次文本 → 向量的计算。返回 `None` 表示当前无可用 embedder（走关键词路径）。
///
/// 注意：同步接口。检索管线在 async 上下文中调用；真实 embedder 若含网络/推理，
/// 应内部自行用 `spawn_blocking` 之类的机制避免阻塞 async 运行时。
pub trait Embedder: Send + Sync {
    /// 计算文本向量；`is_query` 提示当前是查询（Query）还是入库文本（Passage），
    /// 用于 bge 系模型的无查询指令前缀（对齐 Node 版 bgeEmbedding 的 isQuery 分支）。
    fn compute(&self, text: &str, is_query: bool) -> Option<Vec<f32>>;
}

/// 无向量能力的实现：FTS5-only 完整路径。
#[derive(Debug, Clone, Default)]
pub struct NoopEmbedder;

impl Embedder for NoopEmbedder {
    fn compute(&self, _text: &str, _is_query: bool) -> Option<Vec<f32>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_embedder_returns_none() {
        let e = NoopEmbedder;
        assert!(e.compute("hello", true).is_none());
        assert!(e.compute("hello", false).is_none());
    }

    #[test]
    fn noop_trait_object_works_in_pipeline_shape() {
        let e: &dyn Embedder = &NoopEmbedder;
        assert!(e.compute("今天咖啡如何", true).is_none());
    }
}
