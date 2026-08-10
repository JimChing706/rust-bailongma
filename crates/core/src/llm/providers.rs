//! Provider 注册表 —— 对齐 Node 版 `src/config.js` 的 `PROVIDER_CONFIG`。
//!
//! 7 个内置 provider（DeepSeek/MiniMax/OpenAI/Qwen/Moonshot/智谱 GLM/小米 MiMo），
//! 提供 baseURL / 默认模型 / 环境变量名，以及各 provider 的请求参数适配决策
//! （采样省略、max_completion_tokens、thinking 开关、模型降级链）。

/// provider id 常量（与 Node 版一致）
pub const DEEPSEEK: &str = "deepseek";
pub const MINIMAX: &str = "minimax";
pub const OPENAI: &str = "openai";
pub const QWEN: &str = "qwen";
pub const MOONSHOT: &str = "moonshot";
pub const ZHIPU: &str = "zhipu";
pub const MIMO: &str = "mimo";

/// 各 provider 默认模型（与 Node 版 DEFAULT_*_MODEL 一致）
pub const DEFAULT_MODELS: &[(&str, &str)] = &[
    (DEEPSEEK, "deepseek-v4-pro"),
    (MINIMAX, "MiniMax-M2.7"),
    (OPENAI, "gpt-5.5"),
    (QWEN, "qwen-turbo"),
    (MOONSHOT, "kimi-k2.6"),
    (ZHIPU, "glm-5.1"),
    (MIMO, "mimo-v2.5-pro"),
];

/// 单个模型条目（对齐 Node 版 models 数组的 {id, deprecated}）
#[derive(Debug, Clone, Copy)]
pub struct ModelEntry {
    pub id: &'static str,
    pub deprecated: bool,
}

/// provider 静态配置
#[derive(Debug, Clone, Copy)]
pub struct ProviderConfig {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    pub env_var: &'static str,
    pub default_model: &'static str,
    pub models: &'static [ModelEntry],
}

/// 内置 provider 表（与 Node 版 PROVIDER_CONFIG 顺序一致）
pub static PROVIDERS: &[ProviderConfig] = &[
    ProviderConfig {
        id: DEEPSEEK,
        label: "DeepSeek",
        base_url: "https://api.deepseek.com",
        env_var: "DEEPSEEK_API_KEY",
        default_model: "deepseek-v4-pro",
        models: &[],
    },
    ProviderConfig {
        id: MINIMAX,
        label: "MiniMax",
        base_url: "https://api.minimax.chat/v1",
        env_var: "MINIMAX_API_KEY",
        default_model: "MiniMax-M2.7",
        models: &[],
    },
    ProviderConfig {
        id: OPENAI,
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        env_var: "OPENAI_API_KEY",
        default_model: "gpt-5.5",
        models: &[],
    },
    ProviderConfig {
        id: QWEN,
        label: "Qwen",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        env_var: "DASHSCOPE_API_KEY",
        default_model: "qwen-turbo",
        models: &[],
    },
    ProviderConfig {
        id: MOONSHOT,
        label: "Moonshot",
        base_url: "https://api.moonshot.cn/v1",
        env_var: "MOONSHOT_API_KEY",
        default_model: "kimi-k2.6",
        models: &[],
    },
    ProviderConfig {
        id: ZHIPU,
        label: "智谱 GLM",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        env_var: "ZHIPU_API_KEY",
        default_model: "glm-5.1",
        models: &[],
    },
    ProviderConfig {
        id: MIMO,
        label: "小米 MiMo",
        base_url: "https://api.xiaomimimo.com/v1",
        env_var: "MIMO_API_KEY",
        // MiMo 降级链依赖 models 列表：primary 失败后依次尝试全部可用模型
        default_model: "mimo-v2.5-pro",
        models: &[
            ModelEntry {
                id: "mimo-v2.5-pro",
                deprecated: false,
            },
            ModelEntry {
                id: "mimo-v2.5",
                deprecated: false,
            },
            ModelEntry {
                id: "mimo-v2.0",
                deprecated: false,
            },
            ModelEntry {
                id: "mimo-1.2",
                deprecated: true,
            },
        ],
    },
];

/// 按 id 查找 provider 配置；未知 id 回退 DeepSeek（与 Node 版 `|| PROVIDER_CONFIG[DEEPSEEK_PROVIDER]` 一致）
pub fn get_provider_config(provider: &str) -> &'static ProviderConfig {
    PROVIDERS
        .iter()
        .find(|p| p.id == provider)
        .unwrap_or(&PROVIDERS[0])
}

/// 未知 provider 也可用（返回 None），供调用方报错
pub fn get_provider_config_opt(provider: &str) -> Option<&'static ProviderConfig> {
    PROVIDERS.iter().find(|p| p.id == provider)
}

/// 归一化模型名：空值回退到 provider 默认模型（对齐 normalizeModel）
pub fn normalize_model(model: Option<&str>, provider: &str) -> String {
    let value = model.map(str::trim).unwrap_or("");
    if value.is_empty() {
        get_provider_config(provider).default_model.to_string()
    } else {
        value.to_string()
    }
}

fn is_moonshot_kimi_model(model: &str) -> bool {
    model.trim().to_ascii_lowercase().starts_with("kimi-")
}

fn is_openai_default_sampling_model(model: &str) -> bool {
    let value = model.trim().to_ascii_lowercase();
    value.starts_with("gpt-5") || {
        // /^o\d/
        let bytes = value.as_bytes();
        bytes.len() >= 2 && bytes[0] == b'o' && bytes[1].is_ascii_digit()
    }
}

/// 该 provider+model 是否省略 temperature/top_p（对齐 shouldOmitSamplingForProviderModel）
pub fn should_omit_sampling_for_provider_model(provider: &str, model: &str) -> bool {
    if provider == OPENAI && is_openai_default_sampling_model(model) {
        return true;
    }
    provider == MOONSHOT && is_moonshot_kimi_model(model)
}

/// 是否使用 max_completion_tokens 而非 max_tokens（对齐 shouldUseMaxCompletionTokensForProviderModel）
pub fn should_use_max_completion_tokens_for_provider_model(provider: &str, model: &str) -> bool {
    provider == OPENAI && is_openai_default_sampling_model(model)
}

fn is_moonshot_thinking_always_on_model(model: &str) -> bool {
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "kimi-k2.7-code" | "kimi-k2.7-code-highspeed"
    )
}

fn is_moonshot_thinking_toggle_supported_model(model: &str) -> bool {
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "kimi-k2.6" | "kimi-k2.5"
    )
}

/// 是否该发 `thinking: {type:"disabled"}`（对齐 shouldSendThinkingDisabledForProviderModel）
pub fn should_send_thinking_disabled_for_provider_model(provider: &str, model: &str) -> bool {
    if provider == ZHIPU {
        return true;
    }
    if provider != MOONSHOT {
        return false;
    }
    is_moonshot_thinking_toggle_supported_model(model)
        && !is_moonshot_thinking_always_on_model(model)
}

/// 模型降级链（对齐 getProviderModelFallbacks）：
/// 非 MiMo provider 只有主模型；MiMo 返回 [primary] + 全部非 deprecated 模型
pub fn get_provider_model_fallbacks(provider: &str, model: Option<&str>) -> Vec<String> {
    let cfg = match get_provider_config_opt(provider) {
        Some(c) => c,
        None => {
            let m = model.map(str::trim).filter(|s| !s.is_empty());
            return m.map(|s| vec![s.to_string()]).unwrap_or_default();
        }
    };
    let primary = normalize_model(model, provider);
    if provider != MIMO {
        return vec![primary];
    }
    let mut chain = vec![primary.clone()];
    for entry in cfg.models {
        if entry.deprecated || chain.contains(&entry.id.to_string()) {
            continue;
        }
        chain.push(entry.id.to_string());
    }
    chain
}

/// deepseek 思考开关：仅 deepseek-chat 关闭（对齐 isThinkingEnabledForModel）
pub fn is_thinking_enabled_for_model(model: &str) -> bool {
    normalize_model(Some(model), DEEPSEEK) != "deepseek-chat"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_models_match_node() {
        assert_eq!(DEFAULT_MODELS.len(), 7);
        assert_eq!(normalize_model(None, DEEPSEEK), "deepseek-v4-pro");
        assert_eq!(normalize_model(None, MIMO), "mimo-v2.5-pro");
        assert_eq!(normalize_model(Some("  custom-m  "), DEEPSEEK), "custom-m");
        // 未知 provider 回退 DeepSeek 默认
        assert_eq!(normalize_model(None, "unknown"), "deepseek-v4-pro");
    }

    #[test]
    fn unknown_provider_returns_none() {
        assert!(get_provider_config_opt("bogus").is_none());
        assert!(get_provider_config_opt(DEEPSEEK).is_some());
    }

    #[test]
    fn mimo_fallback_chain_skips_deprecated() {
        let chain = get_provider_model_fallbacks(MIMO, None);
        assert_eq!(chain, vec!["mimo-v2.5-pro", "mimo-v2.5", "mimo-v2.0"]);
        // 指定主模型时链从它开始
        let chain2 = get_provider_model_fallbacks(MIMO, Some("mimo-v2.5"));
        assert_eq!(chain2, vec!["mimo-v2.5", "mimo-v2.5-pro", "mimo-v2.0"]);
        // 非 MiMo 只有主模型
        let chain3 = get_provider_model_fallbacks(DEEPSEEK, None);
        assert_eq!(chain3, vec!["deepseek-v4-pro"]);
    }

    #[test]
    fn sampling_omission_rules() {
        assert!(should_omit_sampling_for_provider_model(OPENAI, "gpt-5.5"));
        assert!(should_omit_sampling_for_provider_model(OPENAI, "o3-mini"));
        assert!(should_omit_sampling_for_provider_model(
            MOONSHOT,
            "kimi-k2.6"
        ));
        assert!(!should_omit_sampling_for_provider_model(
            DEEPSEEK,
            "deepseek-chat"
        ));
        assert!(!should_omit_sampling_for_provider_model(OPENAI, "gpt-4o"));
    }

    #[test]
    fn max_completion_tokens_rule() {
        assert!(should_use_max_completion_tokens_for_provider_model(
            OPENAI, "gpt-5.5"
        ));
        assert!(should_use_max_completion_tokens_for_provider_model(
            OPENAI, "o1"
        ));
        assert!(!should_use_max_completion_tokens_for_provider_model(
            DEEPSEEK,
            "deepseek-v4-pro"
        ));
        assert!(!should_use_max_completion_tokens_for_provider_model(
            OPENAI, "gpt-4o"
        ));
    }

    #[test]
    fn thinking_disabled_rules() {
        assert!(should_send_thinking_disabled_for_provider_model(
            ZHIPU, "glm-5.1"
        ));
        assert!(should_send_thinking_disabled_for_provider_model(
            MOONSHOT,
            "kimi-k2.6"
        ));
        assert!(should_send_thinking_disabled_for_provider_model(
            MOONSHOT,
            "kimi-k2.5"
        ));
        assert!(!should_send_thinking_disabled_for_provider_model(
            MOONSHOT,
            "kimi-k2.7-code"
        ));
        assert!(!should_send_thinking_disabled_for_provider_model(
            DEEPSEEK,
            "deepseek-v4-pro"
        ));
        // k2.7-code-highspeed 也不可关闭
        assert!(!should_send_thinking_disabled_for_provider_model(
            MOONSHOT,
            "kimi-k2.7-code-highspeed"
        ));
    }

    #[test]
    fn thinking_enabled_for_deepseek_chat() {
        assert!(!is_thinking_enabled_for_model("deepseek-chat"));
        assert!(is_thinking_enabled_for_model("deepseek-v4-pro"));
    }
}
