mod credentials;
mod model;
mod paths;
mod reload;

pub(crate) use credentials::validate_secret;
pub use credentials::{
    CredentialError, OAuthTokens, ProviderCredential, configured_credential_available,
    credential_available, delete_credential, load_api_key, read_credential,
    resolve_credential_path, write_api_key, write_credential,
};
#[cfg(test)]
pub(crate) use model::{AI_OAUTH_PROVIDER_SLUGS, AI_PROVIDER_SLUGS};
pub use model::{
    AiAuth, AiConfig, Config, HistoryConfig, KeyBinding, KeysConfig, LoggingConfig, UiConfig,
};
pub use paths::ConfigPaths;
pub use reload::{ConfigReload, ConfigWatcher};
