use super::*;

#[test]
fn engine_descends_through_confirmed_help_subcommands() {
    let directory = tempfile::tempdir().expect("command directory");
    let path = directory.path().join("gh");
    fs::write(&path, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
        directory.path(),
    ))));
    let cache = Arc::new(CommandHelpCache::default());
    cache.seed("gh", parse_help_output("gh", GH_HELP));
    cache.seed_scope(
        "gh",
        &["pr"],
        parse_help_output_for_scope("gh", &["pr".to_owned()], GH_PR_HELP),
    );
    cache.seed_scope(
            "gh",
            &["pr", "create"],
            parse_help_output_for_scope(
                "gh",
                &["pr".to_owned(), "create".to_owned()],
                "Commands:\n  deep     Continue into a fourth level\nOptions:\n  --fill     Use commit information for title and body\n",
            ),
        );
    cache.seed_scope(
        "gh",
        &["pr", "create", "deep"],
        parse_help_output_for_scope(
            "gh",
            &["pr".to_owned(), "create".to_owned(), "deep".to_owned()],
            "Options:\n  --final     Finish the nested operation\n",
        ),
    );
    let mut engine = CompletionEngine::new(100, 20);
    engine.register(CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        commands,
        cache,
    ));

    let rows: Vec<_> = engine
        .complete(&context("gh pr ", 40))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["gh pr create", "gh pr list"]);

    let rows: Vec<_> = engine
        .complete(&context("gh pr cr", 41))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["gh pr create"]);
    assert!(
        engine
            .complete(&context("gh pr zzz", 42))
            .candidates
            .is_empty()
    );

    let rows: Vec<_> = engine
        .complete(&context("gh pr create --f", 43))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["gh pr create --fill"]);

    let rows: Vec<_> = engine
        .complete(&context("gh pr create deep --f", 44))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["gh pr create deep --final"]);
}

#[test]
fn exhaustive_help_root_unlocks_scopes_for_unlisted_commands() {
    let directory = tempfile::tempdir().expect("command directory");
    let path = directory.path().join("mycli");
    fs::write(&path, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
        directory.path(),
    ))));
    let cache = Arc::new(CommandHelpCache::default());
    // `mycli` is not in the scoped-help allowlist, but its own `--help`
    // exposed a complete Commands section: a self-describing CLI can
    // answer `mycli daemon --help`, so the scope probe is requested.
    cache.seed(
        "mycli",
        parse_help_output("mycli", "Commands:\n  daemon   Manage daemons\n"),
    );
    assert!(
        cache
            .peek("mycli")
            .expect("root help")
            .subcommands_exhaustive
    );
    cache.seed_scope(
        "mycli",
        &["daemon"],
        parse_help_output_for_scope(
            "mycli",
            &["daemon".to_owned()],
            "Options:\n  --json   Emit JSON\n",
        ),
    );
    let provider = CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        Arc::clone(&commands),
        Arc::clone(&cache),
    );
    assert!(provider.applies(&context("mycli daemon ", 1)));

    let mut engine = CompletionEngine::new(100, 20);
    engine.register(provider);
    let rows: Vec<_> = engine
        .complete(&context("mycli daemon --j", 2))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["mycli daemon --json"]);
}

#[test]
fn unlisted_command_with_partial_help_never_requests_scopes() {
    let directory = tempfile::tempdir().expect("command directory");
    let path = directory.path().join("mycli");
    fs::write(&path, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
        directory.path(),
    ))));
    let cache = Arc::new(CommandHelpCache::default());
    // A man-derived subcommand list never proved that `mycli daemon
    // --help` exists; without an exhaustive `--help` Commands section the
    // scope request must stay off for unlisted commands.
    cache.seed(
        "mycli",
        CommandHelp {
            flags: Vec::new(),
            subcommands: vec![HelpEntry {
                name: "daemon".into(),
                description: "Manage daemons.".into(),
                takes_value: false,
            }],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: false,
        },
    );
    let provider = CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        commands,
        Arc::clone(&cache),
    );
    assert!(!provider.applies(&context("mycli daemon ", 1)));
    assert!(!provider.applies(&context("mycli daemon --j", 2)));
    assert_eq!(cache.fetch_count(), 0, "no scoped fetch may be requested");
}

#[test]
fn explicit_executable_paths_receive_dynamic_help() {
    let directory = tempfile::tempdir().expect("command directory");
    let path = directory.path().join("demotool");
    fs::write(&path, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
    let commands = Arc::new(CommandPathCache::default());
    let cache = Arc::new(CommandHelpCache::default());
    cache.seed(
        "./demotool",
        parse_help_output("demotool", "Commands:\n  deploy   Ship it\n"),
    );
    let mut engine = CompletionEngine::new(100, 20);
    engine.register(CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        commands,
        cache,
    ));
    let mut spaced = context("./demotool ", 44);
    spaced.cwd = Arc::new(directory.path().to_owned());
    let rows: Vec<_> = engine
        .complete(&spaced)
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["./demotool deploy"]);

    let mut bare = context("./demotool", 45);
    bare.cwd = Arc::new(directory.path().to_owned());
    let rows: Vec<_> = engine
        .complete(&bare)
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["./demotool deploy"]);
}

#[test]
fn cold_help_probe_never_executes_project_paths_or_build_wrappers() {
    let directory = tempfile::tempdir().expect("command directory");
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).expect("bin");
    for name in ["gradlew", "mvnw"] {
        let path = bin.join(name);
        fs::write(&path, b"#!/bin/sh\nexit 99\n").expect("wrapper");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("wrapper mode");
    }
    let local = directory.path().join("demotool");
    fs::write(&local, b"#!/bin/sh\nexit 99\n").expect("local executable");
    fs::set_permissions(&local, fs::Permissions::from_mode(0o700)).expect("local mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(&bin))));
    let cache = Arc::new(CommandHelpCache::default());
    let provider = CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        commands,
        Arc::clone(&cache),
    );

    for text in ["gradlew ", "mvnw ", "./demotool "] {
        let mut current = context(text, 46);
        current.cwd = Arc::new(directory.path().to_owned());
        assert!(!provider.applies(&current), "{text:?} must stay cache-only");
    }
    assert_eq!(cache.fetch_count(), 0);
}

#[test]
fn rustc_toolchain_selector_does_not_block_documented_flags() {
    let help = CommandHelp {
        flags: vec![HelpEntry {
            name: "--edition".into(),
            description: String::new(),
            takes_value: true,
        }],
        ..CommandHelp::default()
    };
    assert_eq!(
        help_position_for_arguments("rustc", true, &help, &["+nightly"], "--ed"),
        Some(HelpPosition::Flags)
    );
}

#[test]
fn homebrew_help_completes_bare_space_and_strict_prefix_positions() {
    let directory = tempfile::tempdir().expect("command directory");
    let path = directory.path().join("brew");
    fs::write(&path, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
        directory.path(),
    ))));
    let cache = Arc::new(CommandHelpCache::default());
    cache.seed("brew", parse_help_output("brew", BREW_HELP));
    let mut engine = CompletionEngine::new(100, 20);
    engine.register(CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        commands,
        cache,
    ));

    for (text, query) in [("brew", 30), ("brew ", 31)] {
        let rows: Vec<_> = engine
            .complete(&context(text, query))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert!(rows.contains(&"brew install".to_owned()), "rows: {rows:?}");
        assert!(rows.contains(&"brew update".to_owned()), "rows: {rows:?}");
    }

    let rows: Vec<_> = engine
        .complete(&context("brew in", 32))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["brew info", "brew install"]);
    assert!(
        engine
            .complete(&context("brew zzz", 33))
            .candidates
            .is_empty()
    );
}
