//! Static registry of the AI providers `hokan ai setup` can configure.
//!
//! The slug lists mirrored in `config::model` (`AI_PROVIDER_SLUGS` and
//! `AI_OAUTH_PROVIDER_SLUGS`) must stay in sync with this table; the tests at
//! the bottom assert both directions.

use crate::config::AiAuth;

/// One configurable AI provider.
pub struct ProviderSpec {
    /// Stable identifier stored in `ai.provider` and credentials entries.
    pub slug: &'static str,
    /// Human-readable name shown in the setup wizard menu.
    pub label: &'static str,
    /// One-line description shown next to the label.
    pub description: &'static str,
    /// Credential kinds this entry accepts; empty means no credential at all.
    pub auth_methods: &'static [AiAuth],
    /// Base inference endpoint; `""` for `custom`, where the user supplies it.
    pub default_endpoint: &'static str,
    /// Static fallback used when live model listing fails; empty for providers
    /// that only list live (Ollama) or take a free-form model name (custom).
    pub default_models: &'static [&'static str],
    /// Environment variable that can supply the API key; `""` when N/A.
    pub env_hint: &'static str,
    /// Additional environment variable names accepted by the wizard. The
    /// primary `env_hint` remains the name shown first in the prompt.
    pub env_aliases: &'static [&'static str],
    /// Whether the provider's endpoint can list available models live.
    pub supports_model_listing: bool,
}

const API_KEY_ONLY: &[AiAuth] = &[AiAuth::ApiKey];
const OAUTH_ONLY: &[AiAuth] = &[AiAuth::OAuth];
const NO_AUTH: &[AiAuth] = &[];

const GEMINI_MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
];
const GROK_MODELS: &[&str] = &["grok-4.5", "grok-4.3", "grok-composer-2.5-fast"];
const KIMI_CODING_MODELS: &[&str] = &["kimi-k3", "kimi-k2.7-code", "kimi-k2.6", "kimi-for-coding"];
const OPENCODE_GO_MODELS: &[&str] = &[
    "kimi-k2.7-code",
    "qwen3.7-max",
    "deepseek-v4-pro",
    "glm-5",
    "qwen3.7-plus",
];
const OPENROUTER_MODELS: &[&str] = &[
    "anthropic/claude-sonnet-4.6",
    "google/gemini-3.1-pro-preview",
    "deepseek/deepseek-v4-pro",
    "qwen/qwen-2.5-coder-32b-instruct",
];
const OPENAI_MODELS: &[&str] = &["gpt-5.4", "gpt-5.3-codex", "gpt-4.1", "gpt-4o-mini"];
const GROQ_MODELS: &[&str] = &[
    "llama-3.3-70b-versatile",
    "openai/gpt-oss-120b",
    "qwen/qwen3-32b",
];
const MISTRAL_MODELS: &[&str] = &[
    "mistral-large-latest",
    "devstral-small-latest",
    "codestral-latest",
    "mistral-small-latest",
];
const TOGETHER_MODELS: &[&str] = &[
    "moonshotai/Kimi-K2.5",
    "Qwen/Qwen3-Coder-480B-A35B-Instruct",
    "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    "deepseek-ai/DeepSeek-V3",
];
const KIMI_MODELS: &[&str] = &["kimi-k3", "kimi-k2.6", "kimi-k2.5", "kimi-k2-thinking"];
const ZAI_MODELS: &[&str] = &["glm-5.2", "glm-5.1", "glm-5", "glm-4.7"];
const STEPFUN_MODELS: &[&str] = &["step-3.5-flash", "step-3.5-flash-2603"];
const ALIBABA_MODELS: &[&str] = &[
    "qwen3.7-plus",
    "qwen3.6-plus",
    "qwen3-coder-plus",
    "qwen3-coder-next",
];
const OPENCODE_ZEN_MODELS: &[&str] = &[
    "gpt-5.4",
    "gpt-5.3-codex",
    "claude-sonnet-4.6",
    "gemini-3-flash",
    "kimi-k2.5",
    "glm-5",
    "deepseek-v4-pro",
];
const AI_GATEWAY_MODELS: &[&str] = &[
    "openai/gpt-5.4",
    "anthropic/claude-sonnet-4.6",
    "google/gemini-3-flash",
];
const HUGGINGFACE_MODELS: &[&str] = &[
    "Qwen/Qwen3.5-72B-Instruct",
    "deepseek-ai/DeepSeek-V3.2",
    "moonshotai/Kimi-K2.5",
];
const NVIDIA_MODELS: &[&str] = &[
    "nvidia/nemotron-3-super-120b-a12b",
    "nvidia/nemotron-3-nano-30b-a3b",
    "moonshotai/kimi-k2.6",
];
const KILOCODE_MODELS: &[&str] = &[
    "anthropic/claude-sonnet-4.6",
    "openai/gpt-5.4",
    "google/gemini-3-flash",
];
const XIAOMI_MODELS: &[&str] = &["mimo-v2.5-pro", "mimo-v2.5", "mimo-v2-flash"];
const ANTHROPIC_MODELS: &[&str] = &["claude-sonnet-4-6", "claude-opus-4-6", "claude-haiku-4-5"];
const MINIMAX_MODELS: &[&str] = &["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.5"];
const NOVITA_MODELS: &[&str] = &[
    "moonshotai/kimi-k2.5",
    "minimax/minimax-m2.7",
    "zai-org/glm-5",
    "deepseek/deepseek-v3-0324",
];

const REGISTRY: [ProviderSpec; 36] = [
    ProviderSpec {
        slug: "deepseek",
        label: "DeepSeek",
        description: "DeepSeek chat models with an API key",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.deepseek.com/v1",
        default_models: &["deepseek-chat", "deepseek-reasoner"],
        env_hint: "DEEPSEEK_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "openai-oauth",
        label: "OpenAI (ChatGPT)",
        description: "ChatGPT account sign-in via OAuth device code (Codex)",
        auth_methods: OAUTH_ONLY,
        default_endpoint: "https://chatgpt.com/backend-api/codex",
        default_models: &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"],
        env_hint: "",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "gemini-oauth",
        label: "Google Gemini (OAuth)",
        description: "Google account sign-in via OAuth (Gemini Code Assist)",
        auth_methods: OAUTH_ONLY,
        default_endpoint: "https://cloudcode-pa.googleapis.com",
        default_models: GEMINI_MODELS,
        env_hint: "",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "gemini",
        label: "Google Gemini (API key)",
        description: "Gemini models with an API key (OpenAI-compatible)",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://generativelanguage.googleapis.com/v1beta/openai",
        default_models: GEMINI_MODELS,
        env_hint: "GEMINI_API_KEY",
        env_aliases: &["GOOGLE_API_KEY"],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "grok-oauth",
        label: "xAI Grok (OAuth)",
        description: "xAI account sign-in via OAuth device code",
        auth_methods: OAUTH_ONLY,
        default_endpoint: "https://api.x.ai/v1",
        default_models: GROK_MODELS,
        env_hint: "",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "grok",
        label: "xAI Grok (API key)",
        description: "Grok models with an API key",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.x.ai/v1",
        default_models: GROK_MODELS,
        env_hint: "XAI_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "ollama",
        label: "Ollama (local)",
        description: "Local Ollama models; no credential required",
        auth_methods: NO_AUTH,
        default_endpoint: "http://localhost:11434/v1",
        default_models: &[],
        env_hint: "",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "lmstudio",
        label: "LM Studio (local)",
        description: "Local LM Studio server; no credential required by default",
        auth_methods: NO_AUTH,
        default_endpoint: "http://127.0.0.1:1234/v1",
        default_models: &[],
        env_hint: "",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "custom",
        label: "Custom (OpenAI-compatible)",
        description: "Any OpenAI-compatible endpoint with an API key",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "",
        default_models: &[],
        env_hint: "OPENAI_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "opencode-go",
        label: "OpenCode Go (subscription)",
        description: "OpenCode Go subscription with an API key from opencode.ai/auth",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://opencode.ai/zen/go/v1",
        default_models: OPENCODE_GO_MODELS,
        env_hint: "OPENCODE_GO_API_KEY",
        env_aliases: &["OPENCODE_API_KEY"],
        supports_model_listing: false,
    },
    ProviderSpec {
        slug: "openrouter",
        label: "OpenRouter",
        description: "One API key for models from many providers",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://openrouter.ai/api/v1",
        default_models: OPENROUTER_MODELS,
        env_hint: "OPENROUTER_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "kimi-coding",
        label: "Kimi Coding Plan",
        description: "Kimi coding subscription via the Anthropic Messages API",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.kimi.com/coding",
        default_models: KIMI_CODING_MODELS,
        env_hint: "KIMI_CODING_API_KEY",
        env_aliases: &["KIMI_API_KEY"],
        supports_model_listing: false,
    },
    ProviderSpec {
        slug: "openai-api",
        label: "OpenAI API",
        description: "OpenAI models with an API key",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.openai.com/v1",
        default_models: OPENAI_MODELS,
        env_hint: "OPENAI_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "anthropic",
        label: "Anthropic Claude",
        description: "Claude models via the Anthropic Messages API",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.anthropic.com/v1",
        default_models: ANTHROPIC_MODELS,
        env_hint: "ANTHROPIC_API_KEY",
        env_aliases: &["ANTHROPIC_TOKEN"],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "groq",
        label: "Groq",
        description: "Fast open models with a Groq API key",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.groq.com/openai/v1",
        default_models: GROQ_MODELS,
        env_hint: "GROQ_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "mistral",
        label: "Mistral",
        description: "Mistral and Devstral models with an API key",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.mistral.ai/v1",
        default_models: MISTRAL_MODELS,
        env_hint: "MISTRAL_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "together",
        label: "Together AI",
        description: "Open models hosted by Together AI",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.together.xyz/v1",
        default_models: TOGETHER_MODELS,
        env_hint: "TOGETHER_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "kimi",
        label: "Kimi / Moonshot",
        description: "Kimi models on the global Moonshot API",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.moonshot.ai/v1",
        default_models: KIMI_MODELS,
        env_hint: "KIMI_API_KEY",
        env_aliases: &["MOONSHOT_API_KEY"],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "kimi-cn",
        label: "Kimi / Moonshot (China)",
        description: "Kimi models on the mainland Moonshot API",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.moonshot.cn/v1",
        default_models: KIMI_MODELS,
        env_hint: "KIMI_CN_API_KEY",
        env_aliases: &["MOONSHOT_CN_API_KEY"],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "zai",
        label: "Z.AI / GLM",
        description: "GLM models from Z.AI (Zhipu) with an API key",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.z.ai/api/paas/v4",
        default_models: ZAI_MODELS,
        env_hint: "ZAI_API_KEY",
        env_aliases: &["GLM_API_KEY", "Z_AI_API_KEY"],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "stepfun",
        label: "StepFun Step Plan",
        description: "StepFun coding models with a Step Plan key",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.stepfun.ai/step_plan/v1",
        default_models: STEPFUN_MODELS,
        env_hint: "STEPFUN_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "alibaba",
        label: "Alibaba Cloud / Qwen",
        description: "Qwen and other models through DashScope",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        default_models: ALIBABA_MODELS,
        env_hint: "DASHSCOPE_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "alibaba-coding-plan",
        label: "Alibaba Coding Plan",
        description: "Dedicated Qwen coding subscription endpoint",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://coding-intl.dashscope.aliyuncs.com/v1",
        default_models: ALIBABA_MODELS,
        env_hint: "ALIBABA_CODING_PLAN_API_KEY",
        env_aliases: &["DASHSCOPE_API_KEY"],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "ollama-cloud",
        label: "Ollama Cloud",
        description: "Cloud-hosted open models from Ollama",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://ollama.com/v1",
        default_models: &["nemotron-3-nano:30b", "llama3.3", "qwen3:32b"],
        env_hint: "OLLAMA_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "opencode-zen",
        label: "OpenCode Zen",
        description: "Curated models through OpenCode Zen pay-as-you-go",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://opencode.ai/zen/v1",
        default_models: OPENCODE_ZEN_MODELS,
        env_hint: "OPENCODE_ZEN_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "ai-gateway",
        label: "Vercel AI Gateway",
        description: "One key for models from multiple providers",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://ai-gateway.vercel.sh/v1",
        default_models: AI_GATEWAY_MODELS,
        env_hint: "AI_GATEWAY_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "huggingface",
        label: "Hugging Face",
        description: "Inference Providers through the Hugging Face router",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://router.huggingface.co/v1",
        default_models: HUGGINGFACE_MODELS,
        env_hint: "HF_TOKEN",
        env_aliases: &["HUGGINGFACE_API_KEY"],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "nvidia",
        label: "NVIDIA NIM",
        description: "Nemotron and partner models through NVIDIA NIM",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://integrate.api.nvidia.com/v1",
        default_models: NVIDIA_MODELS,
        env_hint: "NVIDIA_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "kilocode",
        label: "Kilo Code",
        description: "Multi-provider coding gateway from Kilo",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.kilo.ai/api/gateway",
        default_models: KILOCODE_MODELS,
        env_hint: "KILOCODE_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "xiaomi",
        label: "Xiaomi MiMo",
        description: "MiMo models through the Xiaomi API",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.xiaomimimo.com/v1",
        default_models: XIAOMI_MODELS,
        env_hint: "XIAOMI_API_KEY",
        env_aliases: &[],
        supports_model_listing: false,
    },
    ProviderSpec {
        slug: "tencent-tokenhub",
        label: "Tencent TokenHub",
        description: "Tencent MaaS models through TokenHub",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://tokenhub.tencentmaas.com/v1",
        default_models: &["hy3-preview"],
        env_hint: "TOKENHUB_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "fireworks",
        label: "Fireworks AI",
        description: "Open models hosted by Fireworks AI",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.fireworks.ai/inference/v1",
        default_models: &[
            "accounts/fireworks/models/llama-v3p3-70b-instruct",
            "accounts/fireworks/models/deepseek-v3p1",
        ],
        env_hint: "FIREWORKS_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "novita",
        label: "NovitaAI",
        description: "Multi-model inference through NovitaAI",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.novita.ai/openai/v1",
        default_models: NOVITA_MODELS,
        env_hint: "NOVITA_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "deepinfra",
        label: "DeepInfra",
        description: "Pay-per-use access to hosted open models",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.deepinfra.com/v1/openai",
        default_models: &[],
        env_hint: "DEEPINFRA_API_KEY",
        env_aliases: &[],
        supports_model_listing: true,
    },
    ProviderSpec {
        slug: "minimax",
        label: "MiniMax",
        description: "MiniMax models through the Anthropic Messages API",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.minimax.io/anthropic",
        default_models: MINIMAX_MODELS,
        env_hint: "MINIMAX_API_KEY",
        env_aliases: &[],
        supports_model_listing: false,
    },
    ProviderSpec {
        slug: "minimax-cn",
        label: "MiniMax (China)",
        description: "MiniMax China models through the Anthropic Messages API",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.minimaxi.com/anthropic",
        default_models: MINIMAX_MODELS,
        env_hint: "MINIMAX_CN_API_KEY",
        env_aliases: &[],
        supports_model_listing: false,
    },
];

#[must_use]
pub fn registry() -> &'static [ProviderSpec] {
    &REGISTRY
}

#[must_use]
pub fn get(slug: &str) -> Option<&'static ProviderSpec> {
    REGISTRY.iter().find(|spec| spec.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AI_NO_AUTH_PROVIDER_SLUGS, AI_OAUTH_PROVIDER_SLUGS, AI_PROVIDER_SLUGS};

    #[test]
    fn registry_has_thirty_six_entries_with_unique_slugs() {
        let slugs: Vec<&str> = registry().iter().map(|spec| spec.slug).collect();
        assert_eq!(slugs.len(), 36);
        let mut unique = slugs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), slugs.len(), "slugs must be unique");
    }

    #[test]
    fn registry_slugs_match_config_validation_lists() {
        let slugs: Vec<&str> = registry().iter().map(|spec| spec.slug).collect();
        assert_eq!(slugs, AI_PROVIDER_SLUGS);

        let oauth: Vec<&str> = registry()
            .iter()
            .filter(|spec| spec.auth_methods.contains(&AiAuth::OAuth))
            .map(|spec| spec.slug)
            .collect();
        assert_eq!(oauth, AI_OAUTH_PROVIDER_SLUGS);

        let no_auth: Vec<&str> = registry()
            .iter()
            .filter(|spec| spec.auth_methods.is_empty())
            .map(|spec| spec.slug)
            .collect();
        assert_eq!(no_auth, AI_NO_AUTH_PROVIDER_SLUGS);
    }

    #[test]
    fn oauth_capable_set_is_exactly_the_three_oauth_slugs() {
        for slug in ["openai-oauth", "gemini-oauth", "grok-oauth"] {
            let spec = get(slug).expect("oauth provider entry");
            assert_eq!(spec.auth_methods, &[AiAuth::OAuth]);
        }
        for slug in [
            "deepseek",
            "gemini",
            "grok",
            "custom",
            "opencode-go",
            "openrouter",
            "kimi-coding",
            "openai-api",
            "anthropic",
            "groq",
            "mistral",
            "together",
            "kimi",
            "kimi-cn",
            "zai",
            "stepfun",
            "alibaba",
            "alibaba-coding-plan",
            "ollama-cloud",
            "opencode-zen",
            "ai-gateway",
            "huggingface",
            "nvidia",
            "kilocode",
            "xiaomi",
            "tencent-tokenhub",
            "fireworks",
            "novita",
            "deepinfra",
            "minimax",
            "minimax-cn",
        ] {
            let spec = get(slug).expect("api key provider entry");
            assert_eq!(spec.auth_methods, &[AiAuth::ApiKey]);
        }
        for slug in ["ollama", "lmstudio"] {
            assert_eq!(
                get(slug)
                    .expect("local no-auth provider entry")
                    .auth_methods,
                &[],
                "{slug} requires no credential by default"
            );
        }
    }

    #[test]
    fn get_resolves_endpoints_and_rejects_unknown_slugs() {
        assert_eq!(
            get("deepseek").map(|spec| spec.default_endpoint),
            Some("https://api.deepseek.com/v1")
        );
        assert_eq!(
            get("openai-oauth").map(|spec| spec.default_endpoint),
            Some("https://chatgpt.com/backend-api/codex")
        );
        assert_eq!(
            get("gemini-oauth").map(|spec| spec.default_endpoint),
            Some("https://cloudcode-pa.googleapis.com")
        );
        assert_eq!(
            get("gemini").map(|spec| spec.default_endpoint),
            Some("https://generativelanguage.googleapis.com/v1beta/openai")
        );
        assert_eq!(
            get("grok-oauth").map(|spec| spec.default_endpoint),
            Some("https://api.x.ai/v1")
        );
        assert_eq!(
            get("grok").map(|spec| spec.default_endpoint),
            Some("https://api.x.ai/v1")
        );
        assert_eq!(
            get("ollama").map(|spec| spec.default_endpoint),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(
            get("lmstudio").map(|spec| spec.default_endpoint),
            Some("http://127.0.0.1:1234/v1")
        );
        assert_eq!(
            get("opencode-go").map(|spec| spec.default_endpoint),
            Some("https://opencode.ai/zen/go/v1")
        );
        assert_eq!(
            get("openrouter").map(|spec| spec.default_endpoint),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(
            get("kimi-coding").map(|spec| spec.default_endpoint),
            Some("https://api.kimi.com/coding")
        );
        assert_eq!(
            get("anthropic").map(|spec| spec.default_endpoint),
            Some("https://api.anthropic.com/v1")
        );
        assert_eq!(
            get("kimi").map(|spec| spec.default_endpoint),
            Some("https://api.moonshot.ai/v1")
        );
        assert_eq!(
            get("opencode-zen").map(|spec| spec.default_endpoint),
            Some("https://opencode.ai/zen/v1")
        );
        assert!(get("unknown").is_none());
    }
}
