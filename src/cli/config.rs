use std::{
    io::{Read, Write},
    path::PathBuf,
};

use super::ConfigCommand;
use crate::config::{Config, ConfigPaths};

const API_KEY_MAX_BYTES: u64 = 8 * 1024;
const API_KEY_STDIN_MAX_BYTES: u64 = API_KEY_MAX_BYTES + 2;

pub fn run(output: &mut dyn Write, command: ConfigCommand) -> crate::Result<()> {
    let paths = ConfigPaths::discover()?;
    let mut input = std::io::stdin().lock();
    run_with_io(output, &mut input, command, &paths)
}

fn run_with_io(
    output: &mut dyn Write,
    input: &mut dyn Read,
    command: ConfigCommand,
    paths: &ConfigPaths,
) -> crate::Result<()> {
    match command {
        ConfigCommand::Path => {
            writeln!(output, "{}", paths.config_file.display())?;
        }
        ConfigCommand::Show => {
            let config = Config::load(&paths.config_file)?;
            let rendered = toml::to_string_pretty(&config)
                .map_err(|error| crate::Error::Config(error.to_string()))?;
            output.write_all(rendered.as_bytes())?;
        }
        ConfigCommand::Validate => {
            let config = Config::load(&paths.config_file)?;
            config.validate()?;
            writeln!(output, "valid: {}", paths.config_file.display())?;
        }
        ConfigCommand::Init => {
            Config::write_default(&paths.config_file)?;
            writeln!(output, "created: {}", paths.config_file.display())?;
        }
        ConfigCommand::Ai {
            enable,
            disable,
            endpoint,
            model,
            api_key_env,
            api_key_stdin,
        } => {
            let mut config = Config::load(&paths.config_file)?;
            let changed = enable
                || disable
                || endpoint.is_some()
                || model.is_some()
                || api_key_env.is_some()
                || api_key_stdin;
            if enable {
                config.ai.enabled = true;
            }
            if disable {
                config.ai.enabled = false;
            }
            if let Some(endpoint) = endpoint {
                config.ai.endpoint = endpoint;
            }
            if let Some(model) = model {
                config.ai.model = model;
            }
            if let Some(api_key_env) = api_key_env {
                config.ai.api_key_env = api_key_env;
                config.ai.api_key_file = None;
            }
            if api_key_stdin {
                let key = read_api_key(input)?;
                // A configured provider owns its own slug so key rotation
                // lands where `load_api_key` actually reads; legacy setups
                // (no provider) keep the reserved migration slug.
                let provider = config.ai.provider.trim();
                if provider.is_empty() {
                    crate::config::write_api_key(&paths.credentials_file, &key)
                        .map_err(|error| crate::Error::Config(error.to_string()))?;
                } else {
                    crate::config::write_credential(
                        &paths.credentials_file,
                        provider,
                        &crate::config::ProviderCredential::ApiKey(key),
                    )
                    .map_err(|error| crate::Error::Config(error.to_string()))?;
                }
                config.ai.api_key_file = Some(PathBuf::from("credentials.toml"));
            }
            if changed {
                config.write_atomic(&paths.config_file)?;
                writeln!(output, "updated: {}", paths.config_file.display())?;
            }
            write_ai_status(output, &config, paths)?;
        }
    }
    Ok(())
}

fn write_ai_status(
    output: &mut dyn Write,
    config: &Config,
    paths: &ConfigPaths,
) -> crate::Result<()> {
    let credential_path =
        crate::config::resolve_credential_path(&config.ai, &paths.credentials_file);
    let key_available =
        crate::config::configured_credential_available(&config.ai, &paths.credentials_file);
    writeln!(output, "config: {}", paths.config_file.display())?;
    writeln!(
        output,
        "enabled: {}",
        if config.ai.enabled { "yes" } else { "no" }
    )?;
    writeln!(
        output,
        "provider: {}",
        if config.ai.provider.trim().is_empty() {
            "<not configured>"
        } else {
            config.ai.provider.trim()
        }
    )?;
    writeln!(
        output,
        "auth: {}",
        match config.ai.auth {
            crate::config::AiAuth::ApiKey => "api-key",
            crate::config::AiAuth::OAuth => "oauth",
        }
    )?;
    writeln!(
        output,
        "endpoint: {}",
        if config.ai.endpoint.is_empty() {
            "<not configured>"
        } else {
            &config.ai.endpoint
        }
    )?;
    writeln!(
        output,
        "model: {}",
        if config.ai.model.is_empty() {
            "<not configured>"
        } else {
            &config.ai.model
        }
    )?;
    if let Some(path) = credential_path {
        writeln!(output, "api key source: file:{}", path.display())?;
    } else {
        writeln!(output, "api key source: env:{}", config.ai.api_key_env)?;
    }
    writeln!(
        output,
        "api key available: {}",
        if key_available { "yes" } else { "no" }
    )?;
    if !key_available
        && config.ai.auth == crate::config::AiAuth::ApiKey
        && let Err(error) = crate::config::load_api_key(&config.ai, &paths.credentials_file)
    {
        writeln!(output, "api key diagnostic: {error}")?;
    }
    if !config.ai.enabled || config.ai.model.is_empty() || !key_available {
        writeln!(
            output,
            "run `hokan config ai --help` to configure endpoint, model, and credentials"
        )?;
    }
    Ok(())
}

fn read_api_key(input: &mut dyn Read) -> crate::Result<zeroize::Zeroizing<String>> {
    use zeroize::Zeroizing;

    let mut bytes = Zeroizing::new(Vec::new());
    input
        .take(API_KEY_STDIN_MAX_BYTES)
        .read_to_end(&mut bytes)?;
    // Strip the line ending first so a maximum-length key is accepted with
    // either LF or CRLF; only the key bytes count against the limit.
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    if bytes.len() as u64 > API_KEY_MAX_BYTES {
        return Err(crate::Error::Config(
            "API key from stdin exceeded 8 KiB".into(),
        ));
    }
    let key = String::from_utf8(std::mem::take(&mut *bytes))
        .map_err(|_| crate::Error::Config("API key from stdin was not UTF-8".into()))?;
    if key.is_empty() {
        return Err(crate::Error::Config("API key from stdin was empty".into()));
    }
    Ok(Zeroizing::new(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(directory: &tempfile::TempDir) -> ConfigPaths {
        let config = directory.path().join("config");
        ConfigPaths {
            config_file: config.join("config.toml"),
            credentials_file: config.join("credentials.toml"),
            specs_directory: config.join("specs"),
            state_directory: directory.path().join("state"),
            cache_directory: directory.path().join("cache"),
        }
    }

    #[test]
    fn ai_command_writes_private_credential_and_configures_file_source() {
        let directory = tempfile::tempdir().expect("directory");
        let paths = paths(&directory);
        let command = ConfigCommand::Ai {
            enable: true,
            disable: false,
            endpoint: Some("http://127.0.0.1:8080/v1".into()),
            model: Some("test-model".into()),
            api_key_env: None,
            api_key_stdin: true,
        };
        let mut output = Vec::new();
        run_with_io(&mut output, &mut &b"test-key\n"[..], command, &paths).expect("configure AI");

        let config = Config::load(&paths.config_file).expect("load config");
        assert!(config.ai.enabled);
        assert_eq!(
            config.ai.api_key_file.as_deref(),
            Some(std::path::Path::new("credentials.toml"))
        );
        let key = crate::config::load_api_key(&config.ai, &paths.credentials_file)
            .expect("load credential");
        assert_eq!(key.as_str(), "test-key");
        let rendered = String::from_utf8(output).expect("UTF-8 output");
        assert!(!rendered.contains("test-key"));
    }

    #[test]
    fn env_source_replaces_file_source_without_storing_a_secret() {
        let directory = tempfile::tempdir().expect("directory");
        let paths = paths(&directory);
        let mut config = Config::default();
        config.ai.api_key_file = Some(PathBuf::from("credentials.toml"));
        config
            .write_atomic(&paths.config_file)
            .expect("write config");
        let command = ConfigCommand::Ai {
            enable: false,
            disable: false,
            endpoint: None,
            model: None,
            api_key_env: Some("MY_AI_KEY".into()),
            api_key_stdin: false,
        };
        run_with_io(&mut Vec::new(), &mut &b""[..], command, &paths)
            .expect("configure environment source");
        let config = Config::load(&paths.config_file).expect("load config");
        assert_eq!(config.ai.api_key_env, "MY_AI_KEY");
        assert!(config.ai.api_key_file.is_none());
    }

    #[test]
    fn api_key_stdin_rotates_the_configured_providers_slug() {
        let directory = tempfile::tempdir().expect("directory");
        let paths = paths(&directory);
        let mut config = Config::default();
        config.ai.provider = "deepseek".into();
        config.ai.api_key_file = Some(PathBuf::from("credentials.toml"));
        config
            .write_atomic(&paths.config_file)
            .expect("write config");
        crate::config::write_credential(
            &paths.credentials_file,
            "deepseek",
            &crate::config::ProviderCredential::ApiKey(zeroize::Zeroizing::new(
                "old-key".to_owned(),
            )),
        )
        .expect("write existing provider credential");

        let command = ConfigCommand::Ai {
            enable: false,
            disable: false,
            endpoint: None,
            model: None,
            api_key_env: None,
            api_key_stdin: true,
        };
        run_with_io(&mut Vec::new(), &mut &b"rotated-key\n"[..], command, &paths)
            .expect("rotate key");

        // The new key lands under the provider slug, replacing the old one.
        match crate::config::read_credential(&paths.credentials_file, "deepseek")
            .expect("provider credential")
        {
            crate::config::ProviderCredential::ApiKey(key) => {
                assert_eq!(key.as_str(), "rotated-key")
            }
            crate::config::ProviderCredential::OAuth(_) => panic!("entry must stay an API key"),
        }
        // Nothing is written under the reserved legacy slug.
        assert!(matches!(
            crate::config::read_credential(&paths.credentials_file, "default"),
            Err(crate::config::CredentialError::Missing)
        ));
        let config = Config::load(&paths.config_file).expect("load config");
        let key = crate::config::load_api_key(&config.ai, &paths.credentials_file)
            .expect("rotated key loads");
        assert_eq!(key.as_str(), "rotated-key");
    }

    #[test]
    fn ai_status_reports_oauth_credential_as_available() {
        let directory = tempfile::tempdir().expect("directory");
        let paths = paths(&directory);
        crate::config::write_credential(
            &paths.credentials_file,
            "grok-oauth",
            &crate::config::ProviderCredential::OAuth(crate::config::OAuthTokens {
                access_token: zeroize::Zeroizing::new("access-token".to_owned()),
                refresh_token: zeroize::Zeroizing::new("refresh-token".to_owned()),
                expires_at: 1_735_689_600,
                account_id: None,
            }),
        )
        .expect("write oauth credential");

        let mut config = Config::default();
        config.ai.enabled = true;
        config.ai.provider = "grok-oauth".into();
        config.ai.auth = crate::config::AiAuth::OAuth;
        let mut output = Vec::new();
        write_ai_status(&mut output, &config, &paths).expect("status");
        let rendered = String::from_utf8(output).expect("UTF-8 output");
        assert!(rendered.contains("api key available: yes"), "{rendered}");
        assert!(!rendered.contains("access-token"));
    }

    #[test]
    fn read_api_key_accepts_max_length_key_with_crlf() {
        let mut input = vec![b'a'; API_KEY_MAX_BYTES as usize];
        input.extend_from_slice(b"\r\n");
        let key = read_api_key(&mut &input[..]).expect("maximum key with CRLF");
        assert_eq!(key.len(), API_KEY_MAX_BYTES as usize);

        let mut input = vec![b'a'; API_KEY_MAX_BYTES as usize];
        input.extend_from_slice(b"\n");
        let key = read_api_key(&mut &input[..]).expect("maximum key with LF");
        assert_eq!(key.len(), API_KEY_MAX_BYTES as usize);

        let mut input = vec![b'a'; API_KEY_MAX_BYTES as usize + 1];
        input.extend_from_slice(b"\n");
        read_api_key(&mut &input[..]).expect_err("over-limit key must be rejected");
    }
}
