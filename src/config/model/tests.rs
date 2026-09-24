use super::*;

#[test]
fn legacy_ascii_icons_field_is_accepted_and_ignored() {
    let config: Config = toml::from_str("version = 1\n[ui]\nascii_icons = true\n")
        .expect("legacy ascii_icons must still parse");
    assert_eq!(config.ui.ascii_icons, Some(true));
    assert!(config.ui.nerd_fonts, "nerd_fonts defaults to true");
    assert_eq!(config.ui.max_rows, 8);
    assert_eq!(config.ui.max_width, 76);
    let rendered = toml::to_string(&config).expect("serialize config");
    assert!(!rendered.contains("ascii_icons"));
}

#[test]
fn candidate_limit_defaults_to_unlimited_and_accepts_explicit_bounds() {
    let mut config = Config::default();
    assert_eq!(config.completion.max_candidates, 0);
    for limit in [0, 10, 1_000, 10_000] {
        config.completion.max_candidates = limit;
        assert!(config.validate().is_ok());
    }
    for limit in [1, 9, 10_001] {
        config.completion.max_candidates = limit;
        assert!(config.validate().is_err());
    }
}

#[test]
fn max_rows_range_requires_room_for_the_bordered_box() {
    let mut config = Config::default();
    config.ui.max_rows = 2;
    assert!(config.validate().is_err());
    config.ui.max_rows = 3;
    assert!(config.validate().is_ok());
    config.ui.max_rows = 51;
    assert!(config.validate().is_err());
}

#[test]
fn rejects_unknown_fields_and_incomplete_ai() {
    assert!(toml::from_str::<Config>("version = 1\nunknown = true").is_err());
    let mut config = Config::default();
    config.ai.enabled = true;
    assert!(config.validate().is_err());

    config.ai.model = "model".into();
    config.ai.endpoint = "https://user:secret@example.com/v1".into();
    assert!(config.validate().is_err());
}

#[test]
fn validates_key_conflicts_and_allows_disabled_bindings() {
    let mut config = Config::default();
    config.keys.activate = KeyBinding::Tab;
    assert!(config.validate().is_err());
    config.keys.activate = KeyBinding::Disabled;
    config.keys.history = KeyBinding::Disabled;
    assert!(config.validate().is_ok());

    let rendered = toml::to_string(&config).expect("serialize config");
    assert!(rendered.contains("activate = \"disabled\""));
    assert!(toml::from_str::<Config>(&rendered).is_ok());
}

#[test]
fn cd_enter_behavior_defaults_and_preferences_roundtrip() {
    for legacy in ["", "[completion]\nmax_candidates = 200\n"] {
        let config: Config = toml::from_str(legacy).expect("legacy config");
        assert_eq!(
            config.completion.cd_enter_behavior,
            CdEnterBehavior::Execute
        );
    }
    for (value, expected) in [
        ("execute", CdEnterBehavior::Execute),
        ("continue", CdEnterBehavior::Continue),
    ] {
        let config: Config =
            toml::from_str(&format!("[completion]\ncd_enter_behavior = \"{value}\"\n"))
                .expect("cd preference");
        config.validate().expect("valid preference");
        assert_eq!(config.completion.cd_enter_behavior, expected);
        let rendered = toml::to_string(&config).expect("serialize");
        assert_eq!(
            toml::from_str::<Config>(&rendered).expect("roundtrip"),
            config
        );
    }
    assert!(toml::from_str::<Config>("[completion]\ncd_enter_behavior = \"invalid\"\n").is_err());
}

#[test]
fn validates_debug_log_bounds() {
    let mut config = Config::default();
    config.logging.enabled = true;
    config.logging.max_bytes = 1_024;
    assert!(config.validate().is_err());
    config.logging.max_bytes = 64 * 1024;
    config.logging.rotations = 0;
    assert!(config.validate().is_err());
    config.logging.rotations = 2;
    assert!(config.validate().is_ok());
}

#[test]
fn parse_errors_never_echo_source_values() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("config.toml");
    let secret = "sk-config-secret-value";
    fs::write(&path, format!("version = 1\napi_key = \"{secret}\"\n")).expect("invalid config");

    let error = Config::load(&path).expect_err("unknown field must fail");
    let detail = error.to_string();
    assert!(detail.contains("invalid TOML configuration"));
    assert!(detail.contains("line 2"));
    assert!(!detail.contains(secret));
}

#[test]
fn rejects_non_regular_configuration_files_without_blocking() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("config.fifo");
    nix::unistd::mkfifo(
        &path,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("config FIFO");
    assert!(Config::load(&path).is_err());
}

#[test]
fn oversized_config_is_rejected_before_parsing() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("config.toml");
    fs::write(&path, vec![b'x'; CONFIG_MAX_BYTES as usize + 1]).expect("oversized config");
    let error = Config::load(&path).expect_err("oversized config should fail");
    assert!(error.to_string().contains("1 MiB configuration limit"));
}

#[test]
fn default_config_never_follows_an_existing_symlink() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("config.toml");
    let target = directory.path().join("must-not-be-created");
    std::os::unix::fs::symlink(&target, &path).expect("dangling config symlink");

    let error = Config::write_default(&path).expect_err("existing path must be rejected");

    assert!(error.to_string().contains("refusing to overwrite"));
    assert!(!target.exists());
    assert!(
        fs::symlink_metadata(path)
            .expect("config symlink")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn ai_provider_fields_default_to_legacy_behavior() {
    let config = AiConfig::default();
    assert_eq!(config.provider, "");
    assert_eq!(config.auth, AiAuth::ApiKey);
    assert_eq!(config.account_id, None);

    let rendered = toml::to_string(&config).expect("serialize ai config");
    assert!(!rendered.contains("account_id"));
    assert!(rendered.contains("auth = \"api-key\""));
}

#[test]
fn ai_provider_fields_serde_roundtrip() {
    let parsed: AiConfig =
        toml::from_str("provider = \"grok-oauth\"\nauth = \"oauth\"\naccount_id = \"acct-1\"\n")
            .expect("parse ai config");
    assert_eq!(parsed.provider, "grok-oauth");
    assert_eq!(parsed.auth, AiAuth::OAuth);
    assert_eq!(parsed.account_id.as_deref(), Some("acct-1"));

    let rendered = toml::to_string(&parsed).expect("serialize ai config");
    let reparsed: AiConfig = toml::from_str(&rendered).expect("reparse ai config");
    assert_eq!(parsed, reparsed);
}

#[test]
fn legacy_ai_config_without_provider_fields_loads_unchanged() {
    let parsed: Config = toml::from_str(
            "version = 1\n[ai]\nenabled = false\nendpoint = \"https://api.example.com/v1\"\nmodel = \"m\"\n",
        )
        .expect("legacy ai config must parse");
    assert_eq!(parsed.ai.provider, "");
    assert_eq!(parsed.ai.auth, AiAuth::ApiKey);
    assert_eq!(parsed.ai.account_id, None);
    assert_eq!(parsed.ai.endpoint, "https://api.example.com/v1");
}

#[test]
fn oauth_auth_requires_an_oauth_capable_provider() {
    let mut config = Config::default();
    config.ai.enabled = true;
    config.ai.endpoint = "https://api.x.ai/v1".into();
    config.ai.model = "grok-4.5".into();
    config.ai.auth = AiAuth::OAuth;

    // OAuth with no provider is rejected.
    assert!(config.validate().is_err());
    // OAuth with an API-key-only provider is rejected.
    config.ai.provider = "deepseek".into();
    assert!(config.validate().is_err());
    // OAuth with an OAuth-capable provider needs no api_key source.
    config.ai.provider = "grok-oauth".into();
    config.ai.api_key_env = String::new();
    config.ai.api_key_file = None;
    assert!(config.validate().is_ok());
}

#[test]
fn api_key_auth_requires_known_provider_and_credential_source() {
    let mut config = Config::default();
    config.ai.enabled = true;
    config.ai.endpoint = "https://api.deepseek.com/v1".into();
    config.ai.model = "deepseek-chat".into();

    // Unknown provider slugs are rejected.
    config.ai.provider = "unknown".into();
    assert!(config.validate().is_err());
    // Known providers still require a credential source.
    config.ai.provider = "deepseek".into();
    config.ai.api_key_env = String::new();
    config.ai.api_key_file = None;
    assert!(config.validate().is_err());
    config.ai.api_key_env = "DEEPSEEK_API_KEY".into();
    assert!(config.validate().is_ok());
    // `custom` accepts any endpoint.
    config.ai.provider = "custom".into();
    config.ai.endpoint = "http://localhost:8080/v1".into();
    assert!(config.validate().is_ok());
}

#[test]
fn credential_free_provider_does_not_require_a_source() {
    for (provider, endpoint) in [
        ("ollama", "http://localhost:11434/v1"),
        ("lmstudio", "http://127.0.0.1:1234/v1"),
    ] {
        let mut config = Config::default();
        config.ai.enabled = true;
        config.ai.provider = provider.into();
        config.ai.endpoint = endpoint.into();
        config.ai.model = "local-model".into();
        config.ai.api_key_env.clear();
        config.ai.api_key_file = None;
        assert!(config.validate().is_ok(), "{provider} should need no key");
    }
}

#[test]
fn update_config_defaults_to_the_build_channel() {
    let update = UpdateConfig::default();
    assert!(update.enabled);
    assert_eq!(update.channel, None);
    assert_eq!(update.interval_secs, 1_800);
    assert_eq!(Config::default().update, update);
}

#[test]
fn update_config_serde_roundtrip() {
    let parsed: UpdateConfig =
        toml::from_str("enabled = false\ninterval_secs = 600\n").expect("parse update config");
    assert!(!parsed.enabled);
    assert_eq!(parsed.channel, None);
    assert_eq!(parsed.interval_secs, 600);

    let rendered = toml::to_string(&parsed).expect("serialize update config");
    let reparsed: UpdateConfig = toml::from_str(&rendered).expect("reparse update config");
    assert_eq!(parsed, reparsed);
}

#[test]
fn legacy_channel_is_accepted_but_not_written_back() {
    let config: Config = toml::from_str("[update]\nchannel = \"stable\"\n").expect("config");
    assert_eq!(config.update.channel.as_deref(), Some("stable"));
    assert!(
        !toml::to_string(&config)
            .expect("serialize")
            .contains("channel")
    );
}

#[test]
fn update_config_rejects_invalid_interval() {
    let mut config = Config::default();
    config.update.interval_secs = 59;
    assert!(config.validate().is_err());
    config.update.interval_secs = 60;
    assert!(config.validate().is_ok());
    config.update.interval_secs = 86_400;
    assert!(config.validate().is_ok());
    config.update.interval_secs = 86_401;
    assert!(config.validate().is_err());
}

#[test]
fn legacy_config_without_update_section_loads_with_defaults() {
    let parsed: Config = toml::from_str("version = 1\n[ui]\nmax_rows = 8\n")
        .expect("legacy config without [update] must parse");
    assert_eq!(parsed.update, UpdateConfig::default());

    let rendered = toml::to_string(&parsed).expect("serialize config");
    assert!(rendered.contains("[update]"));
    let reparsed: Config = toml::from_str(&rendered).expect("reparse config");
    assert!(reparsed.validate().is_ok());
}
