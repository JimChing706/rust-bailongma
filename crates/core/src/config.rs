//! 配置模块 —— 加载/校验/迁移现有 `config.json`。
//!
//! 与 Node 版 `src/config.js` 兼容：同一文件、同一字段、同一 schemaVersion。
//! 未知字段保留（forward-compatible），缺失字段用默认值补齐。
//!
//! 路径解析顺序（与 Node 版 paths.js 一致）：
//! 1. 环境变量 `BAILONGMA_USER_DIR`
//! 2. 便携模式：`BAILONGMA_PORTABLE_DIR`（data 子目录）
//! 3. 平台用户数据目录：Windows `%APPDATA%/Bailongma`

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::prelude::LogLevel;

/// 当前配置 schema 版本（与 Node 版 CONFIG_SCHEMA_VERSION=3 对齐）
pub const SCHEMA_VERSION: u32 = 3;

/// 配置文件相对用户目录的名字
const CONFIG_FILE_NAME: &str = "config.json";

// ─────────────────────────────────────────────────────────────
// 类型化配置结构（与 config.json 字段一一对应）
// ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    // ── 模型 ──
    pub provider: String,
    pub model: Option<String>,
    pub fast_model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub thinking: bool,
    pub temperature: Option<f32>,

    // ── 结构 ──
    pub schema_version: u32,
    pub security: SecurityConfig,
    pub network: NetworkConfig,
    pub tts: TtsConfig,
    pub social: serde_json::Value,
    pub context_rules: Vec<ContextRule>,

    // ── 语音 ──
    // 注意：Node 版 config.json 里该字段是 snake_case（其余均为 camelCase），显式保留原名。
    #[serde(rename = "minimax_api_key")]
    pub minimax_api_key: Option<String>,

    // ── 微信 clawbot ──
    pub clawbot: Option<ClawbotConfig>,

    // ── 额外字段（forward-compatible，原样保留） ──
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SecurityConfig {
    pub file_sandbox: bool,
    pub exec_sandbox: bool,
    pub blocked_tools: Vec<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct NetworkConfig {
    pub allow_lan_access: bool,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TtsConfig {
    pub tts_provider: String,
    pub tts_voice_id: String,
    pub doubao_speech_rate: String,
    pub doubao_key: Option<String>,
    pub volcano_token: Option<String>,
    pub volcano_app_id: Option<String>,
    pub doubao_app_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextRule {
    pub action: String,
    pub kind: String,
    pub id: String,
    pub patterns: Vec<String>,
    pub provider: String,
    pub context: String,
    pub enabled: bool,
    pub status: Option<String>,
    pub risk: Option<String>,
    pub trust: Option<String>,
    pub updated_at: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ClawbotConfig {
    pub account_id: String,
    pub bot_token: String,
    pub base_url: String,
}

// ── Default 实现（保证任何缺失字段都有安全默认值） ──

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: None,
            fast_model: None,
            api_key: None,
            base_url: None,
            thinking: true,
            temperature: Some(0.3),
            schema_version: SCHEMA_VERSION,
            security: SecurityConfig::default(),
            network: NetworkConfig::default(),
            tts: TtsConfig::default(),
            social: serde_json::json!({}),
            context_rules: Vec::new(),
            minimax_api_key: None,
            clawbot: None,
            extra: serde_json::Map::new(),
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            file_sandbox: true,
            exec_sandbox: true,
            blocked_tools: Vec::new(),
            updated_at: None,
        }
    }
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            tts_provider: "doubao".into(),
            tts_voice_id: String::new(),
            doubao_speech_rate: "0".into(),
            doubao_key: None,
            volcano_token: None,
            volcano_app_id: None,
            doubao_app_id: None,
        }
    }
}

impl Default for ContextRule {
    fn default() -> Self {
        Self {
            action: "propose".into(),
            kind: "context".into(),
            id: String::new(),
            patterns: Vec::new(),
            provider: "static_text".into(),
            context: String::new(),
            enabled: true,
            status: None,
            risk: None,
            trust: None,
            updated_at: None,
            extra: serde_json::Map::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────
// 用户目录解析
// ─────────────────────────────────────────────────────────────

/// 解析用户数据目录（与 Node 版路径逻辑对齐）
pub fn resolve_user_dir() -> Result<PathBuf> {
    // 1. 显式环境变量优先
    if let Ok(dir) = std::env::var("BAILONGMA_USER_DIR") {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir.trim()));
        }
    }
    // 2. 便携模式
    if let Ok(root) = std::env::var("BAILONGMA_PORTABLE_DIR") {
        if !root.trim().is_empty() {
            return Ok(PathBuf::from(root.trim()).join("data"));
        }
    }
    // 3. 平台用户目录
    let dirs = dirs::data_dir()
        .or_else(dirs::config_dir)
        .ok_or_else(|| CoreError::Config("无法确定用户数据目录".into()))?;
    Ok(dirs.join("Bailongma"))
}

/// 配置文件完整路径
pub fn config_path(user_dir: &Path) -> PathBuf {
    user_dir.join(CONFIG_FILE_NAME)
}

// ─────────────────────────────────────────────────────────────
// 加载 / 保存 / 迁移
// ─────────────────────────────────────────────────────────────

/// 加载配置。文件不存在时返回默认配置；存在时解析 + 迁移 + 校验。
pub fn load_config(user_dir: &Path) -> Result<Config> {
    let path = config_path(user_dir);
    if !path.exists() {
        let cfg = Config::default();
        tracing::info!("配置文件不存在，使用默认配置: {}", path.display());
        return Ok(cfg);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| CoreError::Config(format!("读取 {} 失败: {e}", path.display())))?;
    let mut cfg = parse_config(&raw)?;
    migrate(&mut cfg);
    validate(&cfg)?;
    tracing::info!(
        "配置已加载 (schema v{}) from {}",
        cfg.schema_version,
        path.display()
    );
    Ok(cfg)
}

/// 从字符串解析配置（供测试与 CLI 使用）
pub fn parse_config(raw: &str) -> Result<Config> {
    serde_json::from_str::<Config>(raw)
        .map_err(|e| CoreError::Config(format!("config.json 解析失败: {e}")))
}

/// 保存配置（原子写：先写临时文件再 rename）
pub fn save_config(user_dir: &Path, cfg: &Config) -> Result<()> {
    let path = config_path(user_dir);
    std::fs::create_dir_all(user_dir)
        .map_err(|e| CoreError::Config(format!("创建用户目录失败: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(cfg)
        .map_err(|e| CoreError::Config(format!("配置序列化失败: {e}")))?;
    std::fs::write(&tmp, data)
        .map_err(|e| CoreError::Config(format!("写入 {} 失败: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| CoreError::Config(format!("原子替换配置失败: {e}")))?;
    tracing::info!("配置已保存: {}", path.display());
    Ok(())
}

/// 迁移：把低版本配置升级到当前 SCHEMA_VERSION（幂等）。
/// 现阶段的迁移为空操作，预留扩展点（Node 版 CONFIG_MIGRATIONS 的 Rust 对应）。
fn migrate(cfg: &mut Config) {
    if cfg.schema_version < SCHEMA_VERSION {
        tracing::info!(
            "配置迁移: schema {} → {}",
            cfg.schema_version,
            SCHEMA_VERSION
        );
        cfg.schema_version = SCHEMA_VERSION;
        // 后续版本迁移按需追加：
        // if cfg.schema_version < 2 { ... }
    }
}

/// 校验：不满足约束则拒绝启动（返回错误）。
fn validate(cfg: &Config) -> Result<()> {
    if cfg.schema_version > SCHEMA_VERSION {
        return Err(CoreError::Config(format!(
            "配置文件 schema v{} 高于当前支持的 v{}，请升级 Bailongma",
            cfg.schema_version, SCHEMA_VERSION
        )));
    }
    if !cfg.provider.is_empty() && cfg.provider.len() > 64 {
        return Err(CoreError::Config("provider 名称过长".into()));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────
// 便捷访问器
// ─────────────────────────────────────────────────────────────

impl Config {
    /// 是否已配置 LLM provider（至少指定了 provider）
    pub fn is_llm_configured(&self) -> bool {
        !self.provider.is_empty()
    }

    /// 网络是否允许局域网访问
    pub fn allow_lan_access(&self) -> bool {
        self.network.allow_lan_access
    }

    /// 日志级别（BAILONGMA_LOG 环境变量 > 默认 info）
    pub fn log_level(&self) -> LogLevel {
        std::env::var("BAILONGMA_LOG")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(LogLevel::Info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const SAMPLE: &str = r#"{
      "provider": "deepseek",
      "social": {},
      "tts": {
        "ttsProvider": "doubao",
        "ttsVoiceId": "zh_female_xiaohe",
        "doubaoSpeechRate": "0",
        "doubaoKey": "ark-xxx"
      },
      "schemaVersion": 3,
      "security": { "fileSandbox": true, "execSandbox": true, "blockedTools": [] },
      "thinking": true,
      "temperature": 0.3,
      "network": { "allowLanAccess": true },
      "contextRules": [],
      "minimax_api_key": "http://127.0.0.1:15721",
      "clawbot": {
        "accountId": "f64837f454e8@im.bot",
        "botToken": "f64837f454e8@im.bot:060000",
        "baseUrl": "https://ilinkai.weixin.qq.com"
      },
      "someFutureField": { "nested": 42 }
    }"#;

    #[test]
    fn parses_real_world_config() {
        let cfg = parse_config(SAMPLE).expect("sample should parse");
        assert_eq!(cfg.provider, "deepseek");
        assert_eq!(cfg.schema_version, 3);
        assert!(cfg.security.file_sandbox);
        assert!(cfg.network.allow_lan_access);
        assert_eq!(cfg.tts.tts_provider, "doubao");
        assert_eq!(cfg.tts.doubao_key.as_deref(), Some("ark-xxx"));
        assert_eq!(
            cfg.clawbot.as_ref().unwrap().account_id,
            "f64837f454e8@im.bot"
        );
        assert_eq!(
            cfg.minimax_api_key.as_deref(),
            Some("http://127.0.0.1:15721")
        );
        // forward-compatible：未知字段保留
        assert_eq!(
            cfg.extra.get("someFutureField").unwrap().get("nested"),
            Some(&serde_json::json!(42))
        );
    }

    #[test]
    fn missing_fields_use_defaults() {
        let cfg = parse_config(r#"{"provider":"openai"}"#).expect("sparse config");
        assert_eq!(cfg.provider, "openai");
        assert!(cfg.thinking); // default true
        assert_eq!(cfg.temperature, Some(0.3));
        assert!(!cfg.network.allow_lan_access); // default false
        assert!(cfg.context_rules.is_empty());
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempdir().unwrap();
        let cfg = parse_config(SAMPLE).unwrap();
        save_config(dir.path(), &cfg).expect("save");
        let loaded = load_config(dir.path()).expect("load");
        assert_eq!(loaded.provider, cfg.provider);
        assert_eq!(loaded.tts.tts_voice_id, cfg.tts.tts_voice_id);
        assert_eq!(
            loaded.extra.get("someFutureField"),
            cfg.extra.get("someFutureField")
        );
    }

    #[test]
    fn older_schema_is_migrated() {
        let mut cfg = parse_config(r#"{"provider":"qwen","schemaVersion":2}"#).unwrap();
        assert_eq!(cfg.schema_version, 2);
        migrate(&mut cfg);
        assert_eq!(cfg.schema_version, SCHEMA_VERSION); // 迁移到 3
        validate(&cfg).expect("migrated config valid");
    }

    #[test]
    fn load_migrates_older_file() {
        let dir = tempdir().unwrap();
        let path = config_path(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(&path, r#"{"provider":"qwen","schemaVersion":2}"#).unwrap();
        let cfg = load_config(dir.path()).unwrap();
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn old_file_missing_new_fields_gets_defaults_not_panics() {
        // 旧文件缺 v3 字段（clawbot/minimaxApiKey/contextRules/tts 等）：
        // 空迁移 + serde(default) 必须补齐默认值，未知字段保留在 extra（forward-compatible）
        let dir = tempdir().unwrap();
        let path = config_path(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            &path,
            r#"{"provider":"deepseek","schemaVersion":2,"someFutureField":{"k":1}}"#,
        )
        .unwrap();
        let cfg = load_config(dir.path()).unwrap();
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
        assert!(cfg.clawbot.is_none());
        assert!(cfg.minimax_api_key.is_none());
        assert!(cfg.context_rules.is_empty());
        assert!(cfg.thinking); // bool 默认 true
        assert_eq!(cfg.extra.get("someFutureField").unwrap()["k"], 1);
        // 保存后未知字段原样保留（不丢失）
        let dir2 = tempdir().unwrap();
        save_config(dir2.path(), &cfg).expect("save");
        let reloaded = load_config(dir2.path()).expect("reload");
        assert_eq!(reloaded.extra.get("someFutureField").unwrap()["k"], 1);
        assert_eq!(reloaded.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_is_rejected() {
        let err = parse_config(r#"{"schemaVersion":99}"#).and_then(|mut c| {
            migrate(&mut c);
            validate(&c)
        });
        assert!(err.is_err());
    }

    #[test]
    fn invalid_json_is_error() {
        assert!(parse_config("{not json").is_err());
    }

    #[test]
    fn user_dir_resolution_prefers_env() {
        let guard = std::env::var("BAILONGMA_USER_DIR");
        std::env::set_var("BAILONGMA_USER_DIR", "C:\\fake\\user\\dir");
        let resolved = resolve_user_dir().unwrap();
        assert_eq!(resolved, PathBuf::from("C:\\fake\\user\\dir"));
        // 还原
        match guard {
            Ok(v) => std::env::set_var("BAILONGMA_USER_DIR", v),
            Err(_) => std::env::remove_var("BAILONGMA_USER_DIR"),
        }
    }
}
