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

const REGISTRY: [ProviderSpec; 8] = [
    ProviderSpec {
        slug: "deepseek",
        label: "DeepSeek",
        description: "DeepSeek chat models with an API key",
        auth_methods: API_KEY_ONLY,
        default_endpoint: "https://api.deepseek.com/v1",
        default_models: &["deepseek-chat", "deepseek-reasoner"],
        env_hint: "DEEPSEEK_API_KEY",
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
        supports_model_listing: true,
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
    use crate::config::{AI_OAUTH_PROVIDER_SLUGS, AI_PROVIDER_SLUGS};

    #[test]
    fn registry_has_eight_entries_with_unique_slugs() {
        let slugs: Vec<&str> = registry().iter().map(|spec| spec.slug).collect();
        assert_eq!(slugs.len(), 8);
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
    }

    #[test]
    fn oauth_capable_set_is_exactly_the_three_oauth_slugs() {
        for slug in ["openai-oauth", "gemini-oauth", "grok-oauth"] {
            let spec = get(slug).expect("oauth provider entry");
            assert_eq!(spec.auth_methods, &[AiAuth::OAuth]);
        }
        for slug in ["deepseek", "gemini", "grok", "custom"] {
            let spec = get(slug).expect("api key provider entry");
            assert_eq!(spec.auth_methods, &[AiAuth::ApiKey]);
        }
        assert_eq!(
            get("ollama").expect("ollama entry").auth_methods,
            &[],
            "ollama requires no credential"
        );
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
        assert!(get("unknown").is_none());
    }
}
