use super::*;

/// Writes a legacy v1 file directly; `write_api_key` itself now emits v2.
fn write_v1_file(path: &Path, key: &str) {
    let parent = path.parent().expect("parent");
    fs::create_dir_all(parent).expect("parent directories");
    fs::write(path, format!("version = 1\napi_key = \"{key}\"\n")).expect("write v1 file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("permissions");
    }
}

fn oauth_tokens() -> ProviderCredential {
    ProviderCredential::OAuth(OAuthTokens {
        access_token: Zeroizing::new("access-token".to_owned()),
        refresh_token: Zeroizing::new("refresh-token".to_owned()),
        expires_at: 1_735_689_600,
        account_id: Some("account-1".to_owned()),
    })
}

#[test]
fn writes_private_file_and_reads_key() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("config/credentials.toml");
    write_api_key(&path, "test-secret").expect("write credential");
    let config = AiConfig {
        api_key_file: Some(PathBuf::from("credentials.toml")),
        ..AiConfig::default()
    };
    assert_eq!(
        load_api_key(&config, &path)
            .expect("read credential")
            .as_str(),
        "test-secret"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn rejects_broad_permissions_and_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("credentials.toml");
    write_api_key(&path, "test-secret").expect("write credential");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissions");
    assert!(matches!(
        read_api_key_file(&path, LEGACY_PROVIDER_SLUG),
        Err(CredentialError::InsecurePermissions)
    ));

    let link = directory.path().join("linked.toml");
    symlink(&path, &link).expect("symlink");
    assert!(matches!(
        read_api_key_file(&link, LEGACY_PROVIDER_SLUG),
        Err(CredentialError::NotRegular)
    ));

    let fifo = directory.path().join("credentials.fifo");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("credential FIFO");
    assert!(matches!(
        read_api_key_file(&fifo, LEGACY_PROVIDER_SLUG),
        Err(CredentialError::NotRegular)
    ));
}

#[test]
fn rejects_invalid_secret_without_echoing_it() {
    let error =
        write_api_key(Path::new("unused"), "secret\nvalue").expect_err("newline must be rejected");
    assert!(!error.to_string().contains("secret"));

    let error = write_credential(
        Path::new("unused"),
        "grok-oauth",
        &ProviderCredential::OAuth(OAuthTokens {
            access_token: Zeroizing::new("access\nsecret".to_owned()),
            refresh_token: Zeroizing::new("refresh-secret".to_owned()),
            expires_at: 1_735_689_600,
            account_id: None,
        }),
    )
    .expect_err("newline in token must be rejected");
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn v1_file_reads_as_api_key_for_any_slug() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("credentials.toml");
    write_v1_file(&path, "legacy-secret");

    for slug in ["deepseek", LEGACY_PROVIDER_SLUG, "anything"] {
        match read_credential(&path, slug).expect("read v1 credential") {
            ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "legacy-secret"),
            ProviderCredential::OAuth(_) => panic!("v1 files only hold API keys"),
        }
    }
}

#[test]
fn v2_write_read_roundtrip_per_variant() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("credentials.toml");

    write_credential(
        &path,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
    )
    .expect("write api key credential");
    write_credential(&path, "grok-oauth", &oauth_tokens()).expect("write oauth credential");

    match read_credential(&path, "deepseek").expect("read api key credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "deepseek-key"),
        ProviderCredential::OAuth(_) => panic!("deepseek entry must stay an API key"),
    }
    match read_credential(&path, "grok-oauth").expect("read oauth credential") {
        ProviderCredential::OAuth(tokens) => {
            assert_eq!(tokens.access_token.as_str(), "access-token");
            assert_eq!(tokens.refresh_token.as_str(), "refresh-token");
            assert_eq!(tokens.expires_at, 1_735_689_600);
            assert_eq!(tokens.account_id.as_deref(), Some("account-1"));
        }
        ProviderCredential::ApiKey(_) => panic!("grok-oauth entry must stay OAuth tokens"),
    }
    assert!(matches!(
        read_credential(&path, "gemini"),
        Err(CredentialError::Missing)
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn configured_credential_available_follows_the_auth_method() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("credentials.toml");
    write_credential(&path, "grok-oauth", &oauth_tokens()).expect("write oauth credential");
    write_credential(
        &path,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
    )
    .expect("write api key credential");

    let oauth_config = AiConfig {
        provider: "grok-oauth".into(),
        auth: AiAuth::OAuth,
        ..AiConfig::default()
    };
    // An OAuth entry for the configured provider counts as available.
    assert!(configured_credential_available(&oauth_config, &path));

    // No entry for the configured provider does not.
    let missing = AiConfig {
        provider: "gemini-oauth".into(),
        ..oauth_config.clone()
    };
    assert!(!configured_credential_available(&missing, &path));

    // An API-key entry cannot satisfy an OAuth config.
    let wrong_kind = AiConfig {
        provider: "deepseek".into(),
        ..oauth_config
    };
    assert!(!configured_credential_available(&wrong_kind, &path));

    // API-key auth keeps the legacy resolution. The env fallback uses
    // PATH as an always-set variable (`env::set_var` is unsafe in edition
    // 2024 and forbidden in this crate).
    let env_config = AiConfig {
        api_key_env: "PATH".into(),
        ..AiConfig::default()
    };
    assert!(configured_credential_available(&env_config, &path));
    let file_config = AiConfig {
        provider: "deepseek".into(),
        api_key_file: Some(PathBuf::from("credentials.toml")),
        ..AiConfig::default()
    };
    assert!(configured_credential_available(&file_config, &path));

    for provider in ["ollama", "lmstudio"] {
        let no_auth_config = AiConfig {
            provider: provider.into(),
            api_key_env: String::new(),
            api_key_file: None,
            ..AiConfig::default()
        };
        assert!(configured_credential_available(&no_auth_config, &path));
    }
}

#[test]
fn merge_write_preserves_other_providers() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("credentials.toml");

    write_credential(
        &path,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
    )
    .expect("write deepseek credential");
    write_credential(
        &path,
        "gemini",
        &ProviderCredential::ApiKey(Zeroizing::new("gemini-key".to_owned())),
    )
    .expect("write gemini credential");
    write_credential(
        &path,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key-2".to_owned())),
    )
    .expect("replace deepseek credential");

    match read_credential(&path, "gemini").expect("gemini entry preserved") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "gemini-key"),
        ProviderCredential::OAuth(_) => panic!("gemini entry must stay an API key"),
    }
    match read_credential(&path, "deepseek").expect("deepseek entry replaced") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "deepseek-key-2"),
        ProviderCredential::OAuth(_) => panic!("deepseek entry must stay an API key"),
    }
}

#[test]
fn first_v2_write_migrates_v1_key_under_legacy_slug() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("credentials.toml");
    write_v1_file(&path, "legacy-secret");

    write_credential(&path, "grok-oauth", &oauth_tokens()).expect("write oauth credential");

    // The migrated key stays reachable for legacy configs (no provider set).
    let config = AiConfig {
        api_key_file: Some(PathBuf::from(&path)),
        ..AiConfig::default()
    };
    assert_eq!(
        load_api_key(&config, &path)
            .expect("migrated key loads")
            .as_str(),
        "legacy-secret"
    );
    match read_credential(&path, LEGACY_PROVIDER_SLUG).expect("migrated entry") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "legacy-secret"),
        ProviderCredential::OAuth(_) => panic!("migrated entry must be an API key"),
    }
    // The newly written entry is intact as well.
    assert!(matches!(
        read_credential(&path, "grok-oauth").expect("oauth entry"),
        ProviderCredential::OAuth(_)
    ));
}

#[test]
fn delete_credential_removes_entry_then_file() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("credentials.toml");
    write_credential(
        &path,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
    )
    .expect("write deepseek credential");
    write_credential(&path, "grok-oauth", &oauth_tokens()).expect("write oauth credential");

    delete_credential(&path, "deepseek").expect("delete deepseek");
    assert!(matches!(
        read_credential(&path, "deepseek"),
        Err(CredentialError::Missing)
    ));
    assert!(matches!(
        read_credential(&path, "grok-oauth").expect("oauth entry preserved"),
        ProviderCredential::OAuth(_)
    ));

    delete_credential(&path, "grok-oauth").expect("delete grok-oauth");
    assert!(!path.exists(), "empty store removes the file");
    assert!(matches!(
        delete_credential(&path, "deepseek"),
        Err(CredentialError::Missing)
    ));
}

#[cfg(unix)]
#[test]
fn v2_write_enforces_private_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("nested/credentials.toml");
    write_credential(
        &path,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
    )
    .expect("write credential");
    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(path.parent().expect("parent"))
            .expect("parent metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissions");
    assert!(matches!(
        read_credential(&path, "deepseek"),
        Err(CredentialError::InsecurePermissions)
    ));
}

#[test]
fn load_api_key_falls_back_to_legacy_slug_when_provider_entry_is_missing() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("credentials.toml");
    // Legacy rotation wrote the key under the reserved slug only.
    write_api_key(&path, "rotated-secret").expect("write legacy credential");

    let config = AiConfig {
        provider: "deepseek".into(),
        api_key_file: Some(PathBuf::from(&path)),
        ..AiConfig::default()
    };
    assert_eq!(
        load_api_key(&config, &path)
            .expect("provider miss falls back to the legacy slug")
            .as_str(),
        "rotated-secret"
    );

    // A provider's own entry still wins over the fallback.
    write_credential(
        &path,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
    )
    .expect("write provider credential");
    assert_eq!(
        load_api_key(&config, &path)
            .expect("provider entry loads")
            .as_str(),
        "deepseek-key"
    );
}

#[test]
fn concurrent_writes_to_different_slugs_lose_no_entries() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("credentials.toml");
    let iterations = 100;
    let mut handles = Vec::new();
    for slug in ["alpha", "beta"] {
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            for round in 0..iterations {
                write_credential(
                    &path,
                    slug,
                    &ProviderCredential::ApiKey(Zeroizing::new(format!("{slug}-key-{round}"))),
                )
                .expect("concurrent write");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("writer thread");
    }
    for slug in ["alpha", "beta"] {
        match read_credential(&path, slug).expect("entry survives concurrent writes") {
            ProviderCredential::ApiKey(key) => {
                assert_eq!(key.as_str(), format!("{slug}-key-{}", iterations - 1));
            }
            ProviderCredential::OAuth(_) => panic!("entry must stay an API key"),
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let lock = directory.path().join("credentials.toml.lock");
        assert_eq!(
            fs::metadata(&lock)
                .expect("lock file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn debug_output_never_contains_secrets() {
    let credential = oauth_tokens();
    let rendered = format!("{credential:?}");
    assert!(!rendered.contains("access-token"));
    assert!(!rendered.contains("refresh-token"));
    assert!(rendered.contains("1735689600"));
}
