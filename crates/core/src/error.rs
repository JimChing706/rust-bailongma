//! 统一错误类型（thiserror）。
//!
//! 对齐 Node 版常见异常分类：配置 / 数据库 / LLM / API / 工具 / 语音 / 社交，
//! 编译期强类型，避免 JS 版「错误全是字符串」导致的分发不可靠。

use thiserror::Error;

/// 全局结果别名
pub type Result<T> = std::result::Result<T, CoreError>;

/// 核心错误类型
#[derive(Debug, Error)]
pub enum CoreError {
    // ── 基础设施 ──
    #[error("配置错误: {0}")]
    Config(String),
    #[error("配置迁移失败: {0}")]
    ConfigMigration(String),

    #[error("I/O 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("序列化错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("数据库错误: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("内部状态错误: {0}")]
    State(String),

    // ── 网络 / 服务 ──
    #[error("网络错误: {0}")]
    Network(#[from] reqwest::Error),

    #[error("API 服务器错误: {0}")]
    Api(String),

    #[error("WebSocket 错误: {0}")]
    Ws(String),

    // ── LLM ──
    #[error("LLM 尚未激活或凭据缺失: {0}")]
    LlmNotConfigured(String),
    #[error("LLM 调用失败: {0}")]
    Llm(String),
    #[error("LLM 调用失败 (HTTP {status}): {message}")]
    LlmHttp { status: u16, message: String },
    #[error("LLM 流中断: {message}")]
    LlmStream { message: String, had_content: bool },
    #[error("LLM 调用被中止")]
    LlmAborted,

    // ── 工具 ──
    #[error("工具执行失败: {0}")]
    Tool(String),
    #[error("工具权限被拒绝: {0}")]
    ToolForbidden(String),
    #[error("工具参数非法: {0}")]
    ToolInvalidArgs(String),

    // ── 其他 ──
    #[error("不支持的平台: {0}")]
    UnsupportedPlatform(String),
    #[error("无效输入: {0}")]
    InvalidInput(String),
    #[error("校验失败: {0}")]
    Validation(String),
    #[error("未找到: {0}")]
    NotFound(String),
    #[error("其他错误: {0}")]
    Other(String),
}

impl CoreError {
    /// 判断是否可安全重试（网络抖动/LLM 瞬时错误）。
    /// 与 `llm::retry::is_transient_error` 保持一致：流中断且未流出内容也可重试。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            CoreError::Network(_)
                | CoreError::Api(_)
                | CoreError::Llm(_)
                | CoreError::Ws(_)
                | CoreError::LlmStream {
                    had_content: false,
                    ..
                }
        )
    }
}

impl From<String> for CoreError {
    fn from(s: String) -> Self {
        CoreError::Other(s)
    }
}

impl From<&str> for CoreError {
    fn from(s: &str) -> Self {
        CoreError::Other(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification() {
        assert!(CoreError::Api("upstream 502".into()).is_retryable());
        assert!(CoreError::Llm("stream reset".into()).is_retryable());
        assert!(CoreError::LlmStream {
            message: "idle".into(),
            had_content: false
        }
        .is_retryable());
        assert!(!CoreError::LlmStream {
            message: "mid".into(),
            had_content: true
        }
        .is_retryable());
        assert!(CoreError::Ws("conn closed".into()).is_retryable());
        assert!(!CoreError::Config("bad".into()).is_retryable());
        assert!(!CoreError::ToolForbidden("no".into()).is_retryable());
        assert!(!CoreError::LlmNotConfigured("missing key".into()).is_retryable());
    }

    #[test]
    fn from_strings() {
        let e: CoreError = "oops".into();
        assert!(matches!(e, CoreError::Other(_)));
    }
}
