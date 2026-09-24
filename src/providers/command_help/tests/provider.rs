use super::*;

#[test]
fn help_supplements_curated_recipes_without_duplicate_rows() {
    let directory = tempfile::tempdir().expect("commands");
    let executable = directory.path().join("ls");
    fs::write(&executable, b"#!/bin/sh\n").expect("executable");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
        directory.path(),
    ))));
    let specs = Arc::new(SpecRegistry::load(None));
    let cache = Arc::new(CommandHelpCache::default());
    cache.seed(
        "ls",
        parse_help_output(
            "ls",
            "Options:\n  -lah  Detailed listing\n  --documented-extra  Additional option\n",
        ),
    );
    let mut engine = CompletionEngine::new(0, 3);
    engine.register(crate::providers::CommandSpecProvider::new(
        Arc::clone(&specs),
        Arc::clone(&commands),
    ));
    engine.register(CommandHelpProvider::new(specs, commands, cache));
    let output = engine.complete(&context("ls -", 1));
    assert!(
        output
            .candidates
            .iter()
            .any(|row| row.display.primary == "ls --documented-extra")
    );
    assert_eq!(
        output
            .candidates
            .iter()
            .filter(|row| row.display.primary == "ls -lah")
            .count(),
        1
    );
}

#[test]
fn applies_gating_includes_specs_but_skips_missing_and_partial_commands() {
    let directory = tempfile::tempdir().expect("command directory");
    for command in ["ls", "git"] {
        let path = directory.path().join(command);
        fs::write(&path, b"#!/bin/sh\n").expect("fake command");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("command mode");
    }
    let path = OsString::from(directory.path());
    let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
    let cache = Arc::new(CommandHelpCache::default());
    cache.seed("git", CommandHelp::default());
    cache.seed("ls", CommandHelp::default());
    let provider = CommandHelpProvider::new(Arc::new(SpecRegistry::load(None)), commands, cache);
    // Built-in recipes do not exhaust the documented command surface.
    assert!(provider.applies(&context("ls ", 1)));
    assert!(provider.applies(&context("ls -", 2)));
    // Not an executable on PATH: skipped.
    assert!(!provider.applies(&context("nosuchcmd ", 3)));
    // Cursor still on the command token: skipped.
    assert!(!provider.applies(&context("gi", 4)));
    // First-argument and flag positions: applies.
    assert!(provider.applies(&context("git ", 5)));
    assert!(provider.applies(&context("git ch", 6)));
    assert!(provider.applies(&context("git -", 7)));
    // Past the first argument without a dash: no.
    assert!(!provider.applies(&context("git add ", 8)));
    // `--` ends flag parsing; a dash-prefixed path after it is not a flag.
    assert!(!provider.applies(&context("git -- -path", 9)));
}

#[test]
fn engine_emits_seeded_subcommands_and_flags() {
    let directory = tempfile::tempdir().expect("command directory");
    let path = directory.path().join("git");
    fs::write(&path, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
    let path_var = OsString::from(directory.path());
    let commands = Arc::new(CommandPathCache::from_path(Some(&path_var)));
    let cache = Arc::new(CommandHelpCache::default());
    cache.seed(
        "git",
        CommandHelp {
            flags: vec![
                HelpEntry {
                    name: "--paginate".into(),
                    description: "Pipe output into less.".into(),
                    takes_value: false,
                },
                HelpEntry {
                    name: "--config".into(),
                    description: "Read configuration from a file.".into(),
                    takes_value: true,
                },
                HelpEntry {
                    name: "--color".into(),
                    description: "Coloring [possible values: auto, always, never]".into(),
                    takes_value: true,
                },
            ],
            subcommands: vec![
                HelpEntry {
                    name: "checkout".into(),
                    description: "Switch branches.".into(),
                    takes_value: false,
                },
                HelpEntry {
                    name: "checkout-index".into(),
                    description: "Copy files from the index.".into(),
                    takes_value: false,
                },
            ],
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
    let mut engine = CompletionEngine::new(100, 20);
    engine.register(provider);

    let output = engine.complete(&context("git", 24));
    let checkout = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "git checkout")
        .expect("bare subcommand candidate");
    let edit = checkout.edit.as_ref().expect("bare edit");
    assert_eq!(edit.range, 0..3);
    assert_eq!(edit.replacement, "git checkout");

    assert!(
        engine
            .complete(&context("sudo git", 25))
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "sudo git checkout")
    );

    let output = engine.complete(&context("git ", 1));
    let checkout = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "git checkout")
        .expect("checkout candidate");
    assert_eq!(checkout.source, CandidateSource::CommandHelp);
    assert!(matches!(
        checkout.action,
        CandidateAction::InsertAndContinue { .. }
    ));
    assert!(matches!(
        checkout.completeness,
        Completeness::NeedsInput { .. }
    ));
    assert_eq!(checkout.edit.as_ref().expect("edit").range, 4..4);

    let output = engine.complete(&context("git --p", 2));
    let flag = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "git --paginate")
        .expect("flag candidate");
    assert!(matches!(flag.action, CandidateAction::Insert));
    assert_eq!(flag.edit.as_ref().expect("edit").range, 4..7);

    let wrapped = engine.complete(&context("sudo git --p", 20));
    assert!(
        wrapped
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "sudo git --paginate")
    );

    let after_global_value = engine.complete(&context("git --config cfg ch", 21));
    let checkout = after_global_value
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "git --config cfg checkout")
        .expect("subcommand after a separated global flag value");
    assert_eq!(
        checkout.edit.as_ref().expect("edit").replacement,
        "checkout"
    );
    assert!(
        engine
            .complete(&context("git --config cf", 22))
            .candidates
            .is_empty(),
        "a required flag value must not be treated as a subcommand"
    );
    let separated = engine.complete(&context("git --color a", 28));
    assert_eq!(separated.candidates.len(), 2);
    assert_eq!(
        separated.candidates[0]
            .edit
            .as_ref()
            .expect("separated enum edit")
            .replacement,
        "auto"
    );
    let attached = engine.complete(&context("git --color=a", 29));
    assert!(attached.candidates.iter().any(|candidate| {
        candidate
            .edit
            .as_ref()
            .is_some_and(|edit| edit.replacement == "--color=auto")
    }));
    assert!(
        engine
            .complete(&context("git --color auto", 30))
            .candidates
            .is_empty(),
        "an exact documented value must stay quiet"
    );
    let longer = engine.complete(&context("git checkout", 23));
    assert_eq!(longer.candidates.len(), 1);
    assert_eq!(longer.candidates[0].display.primary, "git checkout-index");
    assert!(
        engine
            .complete(&context("git ckt", 26))
            .candidates
            .is_empty(),
        "subcommands require a real token prefix"
    );

    assert!(
        engine
            .complete(&context("git checkout -", 3))
            .candidates
            .is_empty(),
        "top-level flags must not leak into a recognized subcommand"
    );

    // Past the first argument without a dash the provider stays silent.
    let output = engine.complete(&context("git checkout ", 4));
    assert!(output.candidates.is_empty());
}

#[test]
fn local_budget_keeps_both_cached_help_and_session_commands() {
    let directory = tempfile::tempdir().expect("command directory");
    let path = directory.path().join("hokan");
    fs::write(&path, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
        directory.path(),
    ))));
    let cache = Arc::new(CommandHelpCache::default());
    cache.seed(
        "hokan",
        parse_help_output("hokan", "Commands:\n  config   Configure Hokan\n"),
    );
    cache.seed_scope(
        "hokan",
        &["config"],
        parse_help_output_for_scope(
            "hokan",
            &["config".to_owned()],
            "Commands:\n  ai   Configure AI\n",
        ),
    );

    // With a zero local budget the next provider is intentionally not
    // allowed to run once a useful row exists. The semantic help provider
    // must therefore precede the matching `hokan-leave` session row.
    let mut engine = CompletionEngine::new(100, 20).with_local_timeout(Duration::ZERO);
    engine.register(CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        Arc::clone(&commands),
        Arc::clone(&cache),
    ));
    engine.register(crate::providers::SessionCommandProvider);
    engine.register(crate::providers::PathCommandProvider::new(commands));

    let rows: Vec<_> = engine
        .complete(&context("hokan", 1))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["hokan config", "hokan-leave"]);

    let rows: Vec<_> = engine
        .complete(&context("hokan config a", 2))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["hokan config ai"]);
}

#[test]
fn standalone_prompt_clis_offer_help_rows_without_a_space() {
    let directory = tempfile::tempdir().expect("command directory");
    let path = directory.path().join("codex");
    fs::write(&path, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
        directory.path(),
    ))));
    let cache = Arc::new(CommandHelpCache::default());
    cache.seed(
        "codex",
        parse_help_output("codex", "Commands:\n  exec   Run non-interactively\n"),
    );
    let mut engine = CompletionEngine::new(100, 20);
    engine.register(CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        commands,
        cache,
    ));

    let bare_rows: Vec<_> = engine
        .complete(&context("codex", 27))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(bare_rows, ["codex exec"]);
    let rows: Vec<_> = engine
        .complete(&context("codex ", 28))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["codex exec"]);
}

#[test]
fn swift_repl_command_offers_subcommands_without_a_space() {
    let directory = tempfile::tempdir().expect("command directory");
    let path = directory.path().join("swift");
    fs::write(&path, b"#!/bin/sh\n").expect("fake command");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
        directory.path(),
    ))));
    let cache = Arc::new(CommandHelpCache::default());
    cache.seed(
        "swift",
        parse_help_output(
            "swift",
            "Subcommands:\n  swift build   Build packages\n  swift run     Run a product\n",
        ),
    );
    let mut engine = CompletionEngine::new(100, 20);
    engine.register(CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        commands,
        cache,
    ));

    let bare_rows: Vec<_> = engine
        .complete(&context("swift", 31))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(bare_rows, ["swift run", "swift build"]);
    let rows: Vec<_> = engine
        .complete(&context("swift ", 32))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["swift build", "swift run"]);
}
