use super::*;

#[test]
fn history_subcommand_typos_are_filtered_against_cached_help() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for (offset, (command, exit_code)) in [
        ("git pull", Some(1)),
        ("git pul", None),
        ("git push origin main", Some(1)),
        ("git pushh", Some(1)),
        ("git psuh", None),
        ("git -C repo pull", Some(1)),
        ("git -C repo pul", None),
        ("git custom-tool", None),
        ("echo ok && git pull", Some(1)),
        ("echo ok && git pul", None),
        ("codex resume", Some(1)),
        ("codex e", None),
        ("codex fix this bug", None),
        ("codex --config value resume", None),
        ("codex --confg value resume", None),
        // Hybrid prompt CLIs still reject a command-like near miss even
        // if the root process happened to exit successfully.
        ("codex upgrad", Some(0)),
    ]
    .into_iter()
    .enumerate()
    {
        index.ingest(
            command,
            1_000 + offset as i64,
            ShellKind::Zsh,
            None,
            exit_code,
            &policy,
        );
    }

    let entry = |name: &str| HelpEntry {
        name: name.to_owned(),
        description: String::new(),
        takes_value: false,
    };
    let help = Arc::new(CommandHelpCache::default());
    help.seed(
        "git",
        CommandHelp {
            flags: vec![HelpEntry {
                name: "-C".into(),
                description: String::new(),
                takes_value: true,
            }],
            subcommands: vec![entry("pull"), entry("push")],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            // Man-derived Git lists are intentionally extensible.
            subcommands_exhaustive: false,
        },
    );
    help.seed(
        "codex",
        CommandHelp {
            flags: vec![HelpEntry {
                name: "--config".into(),
                description: String::new(),
                takes_value: true,
            }],
            subcommands: vec![entry("resume"), entry("review"), entry("update")],
            subcommand_aliases: vec!["e".into()],
            accepts_positionals: true,
            subcommands_exhaustive: true,
        },
    );
    help.seed_scope("git", &["push"], CommandHelp::default());
    let provider = provider_with_executables_and_help(index, &["git", "codex"], help);

    let rows = |text: &str| {
        provider
            .complete(&context(text, None))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    let git = rows("git p");
    assert!(git.contains(&"git pull".to_owned()), "rows: {git:?}");
    assert!(
        git.contains(&"git push origin main".to_owned()),
        "rows: {git:?}"
    );
    for typo in ["git pul", "git pushh", "git psuh"] {
        assert!(!git.contains(&typo.to_owned()), "rows: {git:?}");
    }
    assert_eq!(rows("git -C repo p"), ["git -C repo pull"]);
    assert_eq!(rows("git c"), ["git custom-tool"]);
    assert_eq!(rows("echo ok && git p"), ["echo ok && git pull"]);
    let codex = rows("codex ");
    assert!(
        codex.contains(&"codex resume".to_owned()),
        "rows: {codex:?}"
    );
    assert!(codex.contains(&"codex e".to_owned()), "rows: {codex:?}");
    assert!(
        codex.contains(&"codex fix this bug".to_owned()),
        "free prompt history must survive: {codex:?}"
    );
    assert!(
        !codex.contains(&"codex upgrad".to_owned()),
        "rows: {codex:?}"
    );
    assert_eq!(
        rows("codex --c"),
        ["codex --config value resume"],
        "a one-edit top-level flag typo must not survive history"
    );
}

#[test]
fn maven_lifecycle_typos_are_filtered_without_running_the_build() {
    assert_eq!(
        maven_history_arguments_are_plausible("mvn", &["install"], false),
        Some(true)
    );
    assert_eq!(
        maven_history_arguments_are_plausible("./mvnw", &["instal"], false),
        Some(false)
    );
    assert_eq!(
        maven_history_arguments_are_plausible("mvn", &["-q", "pakage"], false),
        Some(false)
    );
    assert_eq!(
        maven_history_arguments_are_plausible("mvn", &["dependency:tree"], false),
        Some(true)
    );
    assert_eq!(
        maven_history_arguments_are_plausible("mvn", &["clean", "pakage"], false),
        Some(false),
        "every lifecycle phase must be checked, not only the first"
    );
    assert_eq!(
        maven_history_arguments_are_plausible("mvn", &["dependency:tree", "pakage"], false,),
        Some(false),
        "an extension goal must not hide a later lifecycle typo"
    );
    assert_eq!(
        maven_history_arguments_are_plausible("mvn", &["process-test-resources"], false),
        Some(true)
    );
    assert_eq!(
        maven_history_arguments_are_plausible("mvn", &["instal"], true),
        Some(true),
        "a recorded successful extension command must be preserved"
    );
}

#[test]
fn nested_history_typos_are_filtered_against_scoped_help() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for (offset, command) in [
        "gh pr create",
        "gh pr creat",
        "gh pr list",
        "gh pr lits",
        "gh pr create --fill",
        "gh pr create --fil",
    ]
    .into_iter()
    .enumerate()
    {
        index.ingest(
            command,
            1_000 + offset as i64,
            ShellKind::Zsh,
            None,
            None,
            &policy,
        );
    }

    let entry = |name: &str| HelpEntry {
        name: name.to_owned(),
        description: String::new(),
        takes_value: false,
    };
    let help = Arc::new(CommandHelpCache::default());
    help.seed(
        "gh",
        CommandHelp {
            flags: Vec::new(),
            subcommands: vec![entry("pr")],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: true,
        },
    );
    help.seed_scope(
        "gh",
        &["pr"],
        CommandHelp {
            flags: Vec::new(),
            subcommands: vec![entry("create"), entry("list")],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: true,
        },
    );
    help.seed_scope(
        "gh",
        &["pr", "create"],
        CommandHelp {
            flags: vec![HelpEntry {
                name: "--fill".into(),
                description: String::new(),
                takes_value: false,
            }],
            subcommands: Vec::new(),
            subcommand_aliases: Vec::new(),
            accepts_positionals: true,
            subcommands_exhaustive: false,
        },
    );
    let provider = provider_with_executables_and_help(index, &["gh"], help);
    let rows = |text: &str| {
        provider
            .complete(&context(text, None))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };

    let create = rows("gh pr c");
    assert!(
        create.contains(&"gh pr create".to_owned()),
        "rows: {create:?}"
    );
    assert!(
        create.contains(&"gh pr create --fill".to_owned()),
        "rows: {create:?}"
    );
    assert!(
        !create.contains(&"gh pr creat".to_owned()),
        "rows: {create:?}"
    );

    assert_eq!(rows("gh pr l"), ["gh pr list"]);
    assert_eq!(rows("gh pr create --f"), ["gh pr create --fill"]);
}

#[test]
fn successful_extensible_subcommands_override_the_typo_heuristic() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    index.ingest("git pul", 1_000, ShellKind::Zsh, None, Some(0), &policy);
    let help = Arc::new(CommandHelpCache::default());
    help.seed(
        "git",
        CommandHelp {
            flags: Vec::new(),
            subcommands: vec![HelpEntry {
                name: "pull".into(),
                description: String::new(),
                takes_value: false,
            }],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: false,
        },
    );
    let provider = provider_with_executables_and_help(index, &["git"], help);
    let rows: Vec<_> = provider
        .complete(&context("git p", None))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["git pul"]);
}

#[test]
fn pending_help_defers_argument_history_until_it_can_be_validated() {
    let directory = tempfile::tempdir().expect("directory");
    let executable = directory.path().join("demo-tool");
    fs::write(&executable, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(
        &std::ffi::OsString::from(directory.path()),
    )));
    let help = Arc::new(CommandHelpCache::default());
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    help.request_with_path("demo-tool", commands.path("demo-tool"), move |_| {
        started_tx.send(()).expect("started");
        release_rx.recv().expect("released");
        CommandHelp {
            flags: Vec::new(),
            subcommands: vec![HelpEntry {
                name: "good".into(),
                description: String::new(),
                takes_value: false,
            }],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: true,
        }
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("help request started");

    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in ["demo-tool good", "demo-tool godd"] {
        index.ingest(command, 1_000, ShellKind::Zsh, None, None, &policy);
    }
    let provider = HistoryProvider::new(
        Arc::new(RwLock::new(index)),
        commands,
        Arc::new(AliasCache::default()),
        Arc::new(SpecRegistry::default()),
        Arc::clone(&help),
    )
    .allow_unknown_cwd_for_tests();
    assert!(
        provider
            .complete(&context("demo-tool ", None))
            .candidates
            .is_empty(),
        "pending help must not allow unvalidated history to flash"
    );

    release_tx.send(()).expect("release help");
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while help.peek("demo-tool").is_none() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(
        help.peek("demo-tool").expect("cached help").subcommands[0].name,
        "good"
    );
    let rows: Vec<_> = provider
        .complete(&context("demo-tool ", None))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["demo-tool good"]);
}

#[test]
fn manager_history_requires_a_currently_runnable_manager() {
    let unavailable = provider_with_executables(HistoryIndex::default(), &[]);
    for command in [
        "pnpm dev",
        "npm run build",
        "yarn test",
        "bun run app",
        "deno task lint",
        "sudo pnpm dev",
    ] {
        assert!(
            !unavailable.plausible_command(&context(command, None), command),
            "unavailable manager history leaked for {command:?}"
        );
    }
    assert_eq!(
        unavailable.known_command_prefix(&context("pnp", None)),
        None
    );

    let available = provider_with_executables(
        HistoryIndex::default(),
        &["pnpm", "npm", "yarn", "bun", "deno"],
    );
    for command in [
        "pnpm install",
        "npm install",
        "yarn install",
        "bun install",
        "deno test",
    ] {
        assert!(
            available.plausible_command(&context(command, None), command),
            "installed manager history was filtered for {command:?}"
        );
    }
    for command in [
        "pnpm dev",
        "npm run build",
        "yarn test",
        "bun run app",
        "deno task lint",
        "sudo pnpm dev",
    ] {
        assert!(
            !available.plausible_command(&context(command, None), command),
            "script history without a current manifest leaked for {command:?}"
        );
    }
    assert!(!available.plausible_command(&context("pnpn dev", None), "pnpn dev"));
    assert_eq!(
        available.known_command_prefix(&context("pnp", None)),
        Some("pnp".into())
    );
}

#[test]
fn invalid_history_rows_cannot_crowd_a_valid_row_out_of_the_top_k() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    index.ingest("git status", 1, ShellKind::Zsh, None, Some(0), &policy);
    for number in 0..75 {
        index.ingest_weighted(
            &format!("gti-{number:02} status"),
            10_000 + number,
            ShellKind::Zsh,
            None,
            50,
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_executables(index, &["git"]);
    let rows: Vec<_> = provider
        .complete(&context("g", None))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["git status"]);
}

#[test]
fn unparseable_or_opaque_history_rows_are_kept() {
    let provider = provider_with_executables(HistoryIndex::default(), &["git"]);
    let ctx = |text: &str| context(text, None);
    assert!(provider.plausible_command(&ctx("echo $(gti status)"), "echo $(gti status)"));
    assert!(provider.plausible_command(&ctx("echo 'unterminated"), "echo 'unterminated"));
    assert!(provider.plausible_command(&ctx(""), ""));
}

#[test]
fn aliases_from_rc_files_are_not_mistaken_for_typos() {
    // `gc` is not on PATH, not a builtin, not spec-covered — but it is
    // defined in the user's rc files, so the row must survive.
    let mut aliases = crate::shell::ShellAliases::default();
    crate::shell::parse_rc_text(ShellKind::Zsh, "alias gc='git commit'\n", &mut aliases);
    let provider = HistoryProvider::new(
        Arc::new(RwLock::new(HistoryIndex::default())),
        Arc::new(CommandPathCache::default()),
        Arc::new(AliasCache::new_fixed(aliases)),
        Arc::new(SpecRegistry::default()),
        Arc::new(CommandHelpCache::default()),
    )
    .allow_unknown_cwd_for_tests();
    assert!(provider.plausible_command(&context("gc", None), "gc"));
    assert!(provider.plausible_command(&context("time gc", None), "time gc"));
    assert!(!provider.plausible_command(&context("sudo gc", None), "sudo gc"));
    assert!(!provider.plausible_command(&context("command gc", None), "command gc"));

    // Without the alias definition the same row is filtered as a typo.
    let provider = provider_with_executables(HistoryIndex::default(), &["git"]);
    assert!(!provider.plausible_command(&context("gc", None), "gc"));
}

#[test]
fn inferred_function_slots_remove_stale_history_arguments() {
    let projects = tempfile::tempdir().expect("projects");
    fs::create_dir(projects.path().join("skillscat")).expect("valid project");
    fs::create_dir(projects.path().join("aipass")).expect("valid project");

    let mut definitions = crate::shell::ShellAliases::default();
    crate::shell::parse_rc_text(
        ShellKind::Zsh,
        &format!(
            "proj() {{\n  if [ -n \"$1\" ]; then\n    cd \"{}/$1\"\n  else\n    cd \"{}\"\n  fi\n}}\n",
            projects.path().display(),
            projects.path().display()
        ),
        &mut definitions,
    );
    let aliases = Arc::new(AliasCache::new_fixed(definitions));

    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    index.ingest(
        "proj skillscat",
        1_000,
        ShellKind::Zsh,
        None,
        Some(0),
        &policy,
    );
    index.ingest("proj aipass", 1_001, ShellKind::Zsh, None, Some(0), &policy);
    index.ingest_weighted(
        "proj start-claaude",
        2_000,
        ShellKind::Zsh,
        None,
        50,
        Some(0),
        &policy,
    );

    let provider = HistoryProvider::new(
        Arc::new(RwLock::new(index)),
        Arc::new(CommandPathCache::default()),
        aliases,
        Arc::new(SpecRegistry::default()),
        Arc::new(CommandHelpCache::default()),
    )
    .allow_unknown_cwd_for_tests();
    let rows: Vec<_> = provider
        .complete(&context("proj ", None))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert!(
        rows.contains(&"proj skillscat".to_owned()),
        "rows: {rows:?}"
    );
    assert!(rows.contains(&"proj aipass".to_owned()), "rows: {rows:?}");
    assert!(
        !rows.contains(&"proj start-claaude".to_owned()),
        "stale function target leaked despite its high frecency: {rows:?}"
    );
}

#[test]
fn fish_modifiers_keep_history_in_the_nested_command_family() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in ["not codex review", "not cargo doc"] {
        index.ingest(command, 1_000, ShellKind::Fish, None, Some(0), &policy);
    }
    let provider = provider_with_executables(index, &["codex", "cargo"]);
    let fish = context_for_shell("not cod", ShellKind::Fish);
    let rows: Vec<_> = provider
        .complete(&fish)
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["not codex review"]);

    assert!(!provider.plausible_command(
        &context_for_shell("not codex review", ShellKind::Zsh),
        "not codex review"
    ));
}
