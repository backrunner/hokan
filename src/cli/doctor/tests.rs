use std::{fs, path::PathBuf};

use crate::config::{AiAuth, Config, ConfigPaths, ProviderCredential};

use super::{
    CheckLevel,
    checks::{
        DirectoryPolicy, inspect_ai, inspect_ai_details, inspect_control_channel,
        inspect_debug_logging, inspect_directory,
    },
    zsh::{detect_setup_mode, inspect_zsh_theme, scan_plugin_conflicts, theme_for_contents},
};

#[cfg(unix)]
#[test]
fn directory_checks_enforce_private_state_and_non_writable_config() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private permissions");
    assert_eq!(
        inspect_directory(directory.path(), DirectoryPolicy::Private).level,
        CheckLevel::Ok
    );
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))
        .expect("broad permissions");
    assert_eq!(
        inspect_directory(directory.path(), DirectoryPolicy::Private).level,
        CheckLevel::Error
    );
    assert_eq!(
        inspect_directory(directory.path(), DirectoryPolicy::OwnerOnlyWrites).level,
        CheckLevel::Ok
    );
}

#[cfg(unix)]
#[test]
fn control_channel_must_be_a_fifo_inside_the_private_session() {
    use std::path::Path;

    use nix::{sys::stat::Mode, unistd::mkfifo};

    let directory = tempfile::tempdir().expect("directory");
    let fifo = directory.path().join("control.fifo");
    mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).expect("FIFO");
    assert_eq!(
        inspect_control_channel(Some(directory.path()), &fifo).level,
        CheckLevel::Ok
    );
    assert_eq!(
        inspect_control_channel(Some(Path::new("/elsewhere")), &fifo).level,
        CheckLevel::Error
    );
}

#[test]
fn inspect_ai_follows_the_configured_auth_method() {
    use zeroize::Zeroizing;

    let directory = tempfile::tempdir().expect("directory");
    let paths = ConfigPaths {
        config_file: directory.path().join("config.toml"),
        credentials_file: directory.path().join("credentials.toml"),
        specs_directory: directory.path().join("specs"),
        state_directory: directory.path().join("state"),
        cache_directory: directory.path().join("cache"),
    };
    crate::config::write_credential(
        &paths.credentials_file,
        "grok-oauth",
        &ProviderCredential::OAuth(crate::config::OAuthTokens {
            access_token: Zeroizing::new("access-token".to_owned()),
            refresh_token: Zeroizing::new("refresh-token".to_owned()),
            expires_at: 1_735_689_600,
            account_id: None,
        }),
    )
    .expect("write oauth credential");

    let mut config = Config::default();
    config.ai.enabled = true;
    config.ai.provider = "grok-oauth".into();
    config.ai.auth = AiAuth::OAuth;
    // A stored OAuth token set satisfies an OAuth config; it must not be
    // misreported as an invalid API-key file.
    let check = inspect_ai(Some(&config), Some(&paths));
    assert_eq!(check.level, CheckLevel::Ok, "{}", check.detail);

    // A missing entry is an error, not an "invalid file".
    config.ai.provider = "gemini-oauth".into();
    let check = inspect_ai(Some(&config), Some(&paths));
    assert_eq!(check.level, CheckLevel::Error);
    assert!(check.detail.contains("not configured"), "{}", check.detail);

    // An API-key entry cannot satisfy an OAuth config.
    config.ai.provider = "deepseek".into();
    crate::config::write_credential(
        &paths.credentials_file,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
    )
    .expect("write api key credential");
    assert_eq!(
        inspect_ai(Some(&config), Some(&paths)).level,
        CheckLevel::Error
    );

    // API-key auth still resolves through the legacy file lookup.
    config.ai.auth = AiAuth::ApiKey;
    config.ai.api_key_file = Some(PathBuf::from("credentials.toml"));
    let check = inspect_ai(Some(&config), Some(&paths));
    assert_eq!(check.level, CheckLevel::Ok, "{}", check.detail);
}

#[test]
fn disabled_ai_does_not_require_or_read_a_default_environment_key() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = ConfigPaths {
        config_file: directory.path().join("config.toml"),
        credentials_file: directory.path().join("credentials.toml"),
        specs_directory: directory.path().join("specs"),
        state_directory: directory.path().join("state"),
        cache_directory: directory.path().join("cache"),
    };
    assert_eq!(
        inspect_ai(Some(&Config::default()), Some(&paths)).level,
        CheckLevel::NotApplicable
    );
}

#[test]
fn ai_details_report_provider_auth_and_oauth_entry_without_secrets() {
    use zeroize::Zeroizing;

    let directory = tempfile::tempdir().expect("directory");
    let paths = ConfigPaths {
        config_file: directory.path().join("config.toml"),
        credentials_file: directory.path().join("credentials.toml"),
        specs_directory: directory.path().join("specs"),
        state_directory: directory.path().join("state"),
        cache_directory: directory.path().join("cache"),
    };
    crate::config::write_credential(
        &paths.credentials_file,
        "grok-oauth",
        &ProviderCredential::OAuth(crate::config::OAuthTokens {
            access_token: Zeroizing::new("access-token".to_owned()),
            refresh_token: Zeroizing::new("refresh-token".to_owned()),
            expires_at: 1_735_689_600,
            account_id: None,
        }),
    )
    .expect("write oauth credential");

    // Disabled configs report no details at all.
    assert!(inspect_ai_details(Some(&Config::default()), Some(&paths)).is_none());

    let mut config = Config::default();
    config.ai.enabled = true;
    config.ai.provider = "grok-oauth".into();
    config.ai.auth = AiAuth::OAuth;
    let details = inspect_ai_details(Some(&config), Some(&paths)).expect("details");
    assert_eq!(details.provider.as_deref(), Some("grok-oauth"));
    assert_eq!(details.auth, "oauth");
    let credential = details.credential.expect("oauth credential line");
    assert!(credential.contains("present"), "{credential}");
    assert!(!credential.contains("access-token"));
    assert!(!credential.contains("refresh-token"));

    // A missing entry is reported as absent, still without secrets.
    config.ai.provider = "gemini-oauth".into();
    let details = inspect_ai_details(Some(&config), Some(&paths)).expect("details");
    let credential = details.credential.expect("oauth credential line");
    assert!(credential.contains("no entry"), "{credential}");

    // API-key configs report provider/auth but no credential detail.
    config.ai.provider = "deepseek".into();
    config.ai.auth = AiAuth::ApiKey;
    let details = inspect_ai_details(Some(&config), Some(&paths)).expect("details");
    assert_eq!(details.provider.as_deref(), Some("deepseek"));
    assert_eq!(details.auth, "api-key");
    assert!(details.credential.is_none());
}

#[test]
fn debug_logging_reports_disabled_and_enabled_policy() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = ConfigPaths {
        config_file: directory.path().join("config.toml"),
        credentials_file: directory.path().join("credentials.toml"),
        specs_directory: directory.path().join("specs"),
        state_directory: directory.path().join("state"),
        cache_directory: directory.path().join("cache"),
    };
    let mut config = Config::default();
    let disabled = inspect_debug_logging(Some(&config), Some(&paths));
    assert_eq!(disabled.level, CheckLevel::NotApplicable);
    assert!(disabled.detail.contains("no log file is created"));

    config.logging.enabled = true;
    let enabled = inspect_debug_logging(Some(&config), Some(&paths));
    assert_eq!(enabled.level, CheckLevel::Ok);
    assert!(enabled.detail.contains("1048576 bytes per file"));
    assert!(enabled.detail.contains("exclude query text"));
}

#[test]
fn plugin_scan_flags_known_conflicts_once_each() {
    use std::path::Path;

    let path = Path::new("/home/user/.zshrc");
    let contents = "\
# a comment mentioning zsh-autosuggestions is ignored
source ~/.oh-my-zsh/custom/plugins/zsh-autosuggestions/zsh-autosuggestions.zsh
source /opt/homebrew/share/zsh-autocomplete/zsh-autocomplete.plugin.zsh
eval \"$(atuin init zsh)\"
eval \"$(fzf --zsh)\"
source ~/.fzf.zsh
source ~/plugins/zsh-vi-mode/zsh-vi-mode.plugin.zsh
alias fz=fzf
";
    let checks = scan_plugin_conflicts(path, contents);
    assert_eq!(checks.len(), 5);
    assert!(checks.iter().all(|check| check.level == CheckLevel::Warn));
    for expected in [
        "zsh-autosuggestions",
        "zsh-autocomplete",
        "atuin",
        "fzf shell integration",
        "zsh-vi-mode",
    ] {
        assert!(
            checks.iter().any(|check| check.detail.contains(expected)),
            "missing warning for {expected}"
        );
    }
    assert!(
        checks
            .iter()
            .all(|check| check.detail.contains("HOKAN_ACTIVE")
                && check.detail.contains("--on-demand"))
    );
}

#[test]
fn plugin_scan_ignores_guarded_lines_comments_and_clean_files() {
    use std::path::Path;

    let path = Path::new("/home/user/.zshrc");
    let guarded = "\
[[ -z $HOKAN_ACTIVE ]] && source ~/.zsh/zsh-autosuggestions/zsh-autosuggestions.zsh
# eval \"$(atuin init zsh)\"
export EDITOR=vim
";
    assert!(scan_plugin_conflicts(path, guarded).is_empty());
    assert!(scan_plugin_conflicts(path, "export EDITOR=vim\n").is_empty());
}

#[test]
fn detect_setup_mode_classifies_managed_blocks() {
    let auto_exec = format!(
        "{}\n# protocol 2\nexec \"$__hokan_bin\" --shell zsh\n{}\n",
        crate::cli::integration::START,
        crate::cli::integration::END
    );
    assert_eq!(detect_setup_mode(&auto_exec), Some("auto-start (exec)"));
    let on_demand = format!(
        "{}\n# protocol 2 (on-demand)\nalias hk='/usr/local/bin/hokan --shell zsh'\n{}\n",
        crate::cli::integration::START,
        crate::cli::integration::END
    );
    assert_eq!(
        detect_setup_mode(&on_demand),
        Some("on-demand (`hk` alias)")
    );
    assert_eq!(detect_setup_mode("export EDITOR=vim\n"), None);
}

#[test]
fn theme_detection_finds_known_initializers() {
    assert_eq!(
        theme_for_contents("eval \"$(oh-my-posh init zsh)\"\n"),
        Some("oh-my-posh")
    );
    assert_eq!(
        theme_for_contents("eval \"$(starship init zsh)\"\n"),
        Some("starship")
    );
    assert_eq!(
        theme_for_contents("source ~/powerlevel10k/powerlevel10k.zsh-theme\n"),
        Some("powerlevel10k")
    );
    assert_eq!(
        theme_for_contents("# eval \"$(oh-my-posh init zsh)\"\nexport EDITOR=vim\n"),
        None
    );
    assert_eq!(theme_for_contents("export EDITOR=vim\n"), None);
}

#[test]
fn zprofile_only_theme_warns_unless_login_shell() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join(".zprofile"),
        "eval \"$(oh-my-posh init zsh)\"\n",
    )
    .expect("zprofile fixture");
    let warn = inspect_zsh_theme(dir.path(), false);
    assert_eq!(warn.level, CheckLevel::Warn);
    assert!(
        warn.detail.contains("login_shell = true"),
        "{}",
        warn.detail
    );
    let ok = inspect_zsh_theme(dir.path(), true);
    assert_eq!(ok.level, CheckLevel::Ok);

    fs::write(dir.path().join(".zshrc"), "eval \"$(starship init zsh)\"\n").expect("zshrc fixture");
    let in_zshrc = inspect_zsh_theme(dir.path(), false);
    assert_eq!(in_zshrc.level, CheckLevel::Ok);
    assert!(in_zshrc.detail.contains(".zshrc"), "{}", in_zshrc.detail);
}

#[test]
fn update_section_reports_config_cache_and_exe_writability() {
    use super::checks::inspect_update;

    let directory = tempfile::tempdir().expect("directory");
    let paths = ConfigPaths {
        config_file: directory.path().join("config.toml"),
        credentials_file: directory.path().join("credentials.toml"),
        specs_directory: directory.path().join("specs"),
        state_directory: directory.path().join("state"),
        cache_directory: directory.path().join("cache"),
    };
    fs::create_dir_all(&paths.state_directory).expect("state dir");
    fs::write(
        paths.state_directory.join("update-check.json"),
        "{\"last_check_epoch\":1,\"channel\":\"stable\",\"latest_known\":\"0.2.0\"}",
    )
    .expect("seed update cache");
    let exe = directory.path().join("bin/hokan");
    fs::create_dir_all(exe.parent().expect("exe parent")).expect("bin dir");

    let details = inspect_update(Some(&Config::default()), Some(&paths), &exe);
    assert_eq!(
        details.check.level,
        CheckLevel::Ok,
        "{}",
        details.check.detail
    );
    assert!(details.check.detail.contains("channel stable"));
    assert!(details.check.detail.contains("every 1800s"));
    assert_eq!(details.channel.as_deref(), Some("stable"));
    assert_eq!(details.interval_secs, Some(1_800));
    assert_eq!(details.latest_known.as_deref(), Some("0.2.0"));
    assert_eq!(details.exe.level, CheckLevel::Ok, "{}", details.exe.detail);

    // Disabled configs say so, and still report channel/interval/cache.
    let mut config = Config::default();
    config.update.enabled = false;
    config.update.channel = "beta".into();
    let details = inspect_update(Some(&config), Some(&paths), &exe);
    assert_eq!(details.check.level, CheckLevel::NotApplicable);
    assert!(details.check.detail.contains("disabled"));
    assert_eq!(details.channel.as_deref(), Some("beta"));
    assert_eq!(details.latest_known.as_deref(), Some("0.2.0"));

    // No config at all is an error with no optional fields.
    let details = inspect_update(None, None, &exe);
    assert_eq!(details.check.level, CheckLevel::Error);
    assert!(details.channel.is_none());
    assert!(details.interval_secs.is_none());
    assert!(details.latest_known.is_none());

    // Without a cache file there is no latest-known line.
    let empty = tempfile::tempdir().expect("directory");
    let empty_paths = ConfigPaths {
        state_directory: empty.path().join("state"),
        ..paths.clone()
    };
    let details = inspect_update(Some(&Config::default()), Some(&empty_paths), &exe);
    assert!(details.latest_known.is_none());
}

#[cfg(unix)]
#[test]
fn update_exe_in_a_system_directory_is_a_warning() {
    use std::os::unix::fs::PermissionsExt;

    use super::checks::inspect_update;

    if nix::unistd::geteuid().is_root() {
        // Root ignores permission bits; the probe would succeed.
        return;
    }
    let directory = tempfile::tempdir().expect("directory");
    let paths = ConfigPaths {
        config_file: directory.path().join("config.toml"),
        credentials_file: directory.path().join("credentials.toml"),
        specs_directory: directory.path().join("specs"),
        state_directory: directory.path().join("state"),
        cache_directory: directory.path().join("cache"),
    };
    let bin = directory.path().join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o555)).expect("read-only bin");

    let details = inspect_update(Some(&Config::default()), Some(&paths), &bin.join("hokan"));
    assert_eq!(details.exe.level, CheckLevel::Warn);
    assert!(
        details.exe.detail.contains("package manager"),
        "{}",
        details.exe.detail
    );

    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("restore bin");
}
