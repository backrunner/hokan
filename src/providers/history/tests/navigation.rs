use super::*;

#[test]
fn every_history_mode_keeps_all_matching_commands_by_default() {
    let policy = HistoryPolicy::new(1024, &[]).expect("history policy");
    let directory = tempfile::tempdir().expect("directory");
    let cwd = directory.path().canonicalize().expect("canonical cwd");
    let mut index = HistoryIndex::default();
    for row in 0..120 {
        index.ingest(
            &format!("echo entry{row:03}"),
            1_000 + row,
            ShellKind::Zsh,
            Some(&cwd),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_executables(index, &["echo"]).with_navigation_limit(0);
    for mode in [
        CompletionMode::Normal,
        CompletionMode::HistoryOnly,
        CompletionMode::HistoryNavigation,
    ] {
        let context = context_in(&cwd, "echo entry", mode);
        assert_eq!(
            provider.complete(&context).candidates.len(),
            120,
            "{mode:?}"
        );
    }
}

#[test]
fn arrow_navigation_keeps_more_than_fifty_rows_within_configured_limit() {
    let policy = HistoryPolicy::new(1024, &[]).expect("history policy");
    let directory = tempfile::tempdir().expect("directory");
    let cwd = directory.path().canonicalize().expect("canonical cwd");
    let mut index = HistoryIndex::default();
    for index_number in 0..120 {
        index.ingest(
            &format!("echo entry{index_number:03}"),
            1_000 + index_number,
            ShellKind::Zsh,
            Some(&cwd),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_executables(index, &["echo"]).with_navigation_limit(100);
    let context = context_in(&cwd, "", CompletionMode::HistoryNavigation);
    let ranked = rank_and_dedupe(&context, provider.complete(&context).candidates, 100);
    assert_eq!(ranked.len(), 100);
    assert_eq!(ranked[0].display.primary, "echo entry119");
    assert_eq!(ranked[99].display.primary, "echo entry020");
}

#[test]
fn arrow_navigation_uses_prefix_history_in_timestamp_order() {
    let policy = HistoryPolicy::new(1024, &[]).expect("history policy");
    let mut index = HistoryIndex::default();
    let current = Path::new("/tmp").canonicalize().expect("current directory");
    let other = Path::new("/var/tmp")
        .canonicalize()
        .expect("other directory");
    index.ingest(
        "echo older",
        1_000,
        ShellKind::Zsh,
        Some(&current),
        Some(0),
        &policy,
    );
    index.ingest_weighted(
        "echo older",
        1_100,
        ShellKind::Zsh,
        Some(&current),
        40,
        Some(0),
        &policy,
    );
    index.ingest(
        "echo newest",
        2_000,
        ShellKind::Zsh,
        Some(&current),
        Some(0),
        &policy,
    );
    index.ingest(
        "printf unrelated",
        3_000,
        ShellKind::Zsh,
        Some(&other),
        Some(0),
        &policy,
    );

    let provider = provider_with_executables(index, &["echo", "printf"]);
    let context = context_in(&current, "echo", CompletionMode::HistoryNavigation);
    let ranked = rank_and_dedupe(&context, provider.complete(&context).candidates, 10);
    let rows: Vec<_> = ranked
        .iter()
        .map(|candidate| candidate.display.primary.as_str())
        .collect();
    assert_eq!(rows, ["echo newest", "echo older"]);
    assert!(ranked.iter().all(|candidate| {
        candidate.source == CandidateSource::History && candidate.score.history_timestamp > 0
    }));

    let normal = context_in(&current, "echo", CompletionMode::Normal);
    let normal_rows: Vec<_> = provider
        .complete(&normal)
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(normal_rows.len(), 2);
    assert!(normal_rows.contains(&"echo newest".to_owned()));
    assert!(normal_rows.contains(&"echo older".to_owned()));
}

#[test]
fn transition_bigram_boosts_the_known_successor_end_to_end() {
    let provider = provider_with_executables(history_index(), &["git"]);

    let boosted = context("git c", Some("git add x"));
    let ranked = rank_and_dedupe(&boosted, provider.complete(&boosted).candidates, 10);
    assert_eq!(ranked[0].display.primary, "git commit -m y");
    assert_eq!(ranked[0].score.transition, 200);
    assert_eq!(ranked[1].display.primary, "git config user.name x");
    assert_eq!(ranked[1].score.transition, 0);

    // Without a matching previous command there is no boost and plain
    // match/frecency ordering decides.
    let plain = context("git c", Some("ls -la"));
    let ranked = rank_and_dedupe(&plain, provider.complete(&plain).candidates, 10);
    assert_eq!(ranked[0].display.primary, "git config user.name x");
    assert_eq!(ranked[0].score.transition, 0);
}

#[test]
fn recently_failed_commands_carry_the_failure_penalty() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    index.ingest("make deploy", 1_000, ShellKind::Zsh, None, Some(0), &policy);
    index.ingest("make deploy", 2_000, ShellKind::Zsh, None, Some(2), &policy);
    index.ingest("make build", 3_000, ShellKind::Zsh, None, Some(0), &policy);
    let provider = provider_with_executables(index, &["make"]);
    let output = provider.complete(&context("make ", None));
    let deploy = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "make deploy")
        .expect("deploy candidate");
    assert_eq!(deploy.score.failed_penalty, 150);
    let build = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "make build")
        .expect("build candidate");
    assert_eq!(build.score.failed_penalty, 0);
}

#[test]
fn argument_position_history_must_continue_the_typed_words() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    // The only row that genuinely continues `kimi `.
    index.ingest("kimi web", 1_000, ShellKind::Zsh, None, Some(0), &policy);
    // Contains `kimi ` as a substring, but is an unrelated command.
    index.ingest(
        "echo kimi rocks",
        2_000,
        ShellKind::Zsh,
        None,
        Some(0),
        &policy,
    );
    // Matches `kimi ` only as a subsequence (k…i…m…i…space).
    index.ingest(
        "docker build -t myimage .",
        3_000,
        ShellKind::Zsh,
        None,
        Some(0),
        &policy,
    );
    index.ingest(
        "kimi > logs/output.log",
        4_000,
        ShellKind::Zsh,
        None,
        Some(0),
        &policy,
    );
    let provider = provider_with_executables(index, &["kimi", "docker"]);

    let primaries = |text: &str| {
        let context = context(text, None);
        let mut primaries: Vec<_> = provider
            .complete(&context)
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        primaries.sort();
        primaries
    };

    // Past the command token only rows that genuinely continue the full
    // typed prefix may surface.
    assert_eq!(
        primaries("kimi "),
        vec!["kimi > logs/output.log", "kimi web"]
    );
    assert_eq!(primaries("kimi w"), vec!["kimi web"]);
    assert_eq!(primaries("kimi > lo"), vec!["kimi > logs/output.log"]);
    // Normal completion accepts literal prefixes only.
    assert_eq!(primaries("kim"), vec!["kimi > logs/output.log", "kimi web"]);
    assert!(primaries("dob").is_empty());

    // Explicit history search keeps broad fuzzy recall for remembered
    // fragments without leaking that low-confidence behavior into the
    // normal recommendation list.
    let history_only = context("dob", None).with_mode(CompletionMode::HistoryOnly);
    let rows: Vec<_> = provider
        .complete(&history_only)
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(rows, ["docker build -t myimage ."]);
}

#[test]
fn history_continuations_respect_token_boundaries_and_midline_suffixes() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in ["kimi web", "kimiko deploy", "git status --short"] {
        index.ingest(command, 1_000, ShellKind::Zsh, None, Some(0), &policy);
    }
    let provider = provider_with_executables(index, &["kimi", "kimiko", "git"]);

    let output = provider.complete(&context("kimi ", None));
    let rows: Vec<_> = output
        .candidates
        .iter()
        .map(|candidate| candidate.display.primary.as_str())
        .collect();
    assert_eq!(rows, ["kimi web"]);

    let text = "git sta --short";
    let output = provider.complete(&context_at(text, "git sta".len(), None));
    assert!(
        output
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "git status --short")
    );

    let text = "git sta --long";
    let output = provider.complete(&context_at(text, "git sta".len(), None));
    assert!(output.candidates.is_empty());
}

#[test]
fn later_command_segments_use_the_full_line_as_the_history_anchor() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "echo ok && codex review",
        "codex unrelated",
        "echo ok && cargo doc",
    ] {
        index.ingest(command, 1_000, ShellKind::Zsh, None, Some(0), &policy);
    }
    let provider = provider_with_executables(index, &["echo", "codex", "cargo"]);
    let output = provider.complete(&context("echo ok && cod", None));
    let rows: Vec<_> = output
        .candidates
        .iter()
        .map(|candidate| candidate.display.primary.as_str())
        .collect();
    assert_eq!(rows, ["echo ok && codex review"]);
}

#[test]
fn history_rows_with_unknown_commands_are_filtered() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for (command, kept) in [
        ("git status", true),                      // executable on PATH
        ("gti status", false),                     // typo: not executable
        ("sl -la", false),                         // typo
        ("sudo gti status", false),                // wrapper peeled, still a typo
        ("FOO=bar git diff", true),                // assignment peeled
        ("cd /tmp", true),                         // builtin
        ("if true; then echo ok; fi", true),       // reserved word in shell syntax
        ("builtin if", false),                     // keyword is not a callable builtin
        ("command if", false),                     // `command` cannot execute keywords
        ("for f in *; do git add $f; done", true), // shell keyword
        ("./run.sh --fast", false),                // missing explicit path
        ("echo done | gti log", false),            // later segments are validated too
    ] {
        index.ingest(command, 1_000, ShellKind::Zsh, None, Some(0), &policy);
        let provider = provider_with_executables(HistoryIndex::default(), &["git"]);
        assert_eq!(
            provider.plausible_command(&context(command, None), command),
            kept,
            "plausibility of {command:?}"
        );
    }
    // End to end: the typo row never leaves the provider.
    let provider = provider_with_executables(index, &["git"]);
    let output = provider.complete(&context("g", None));
    let primaries: Vec<_> = output
        .candidates
        .iter()
        .map(|candidate| candidate.display.primary.as_str())
        .collect();
    assert!(primaries.contains(&"git status"), "rows: {primaries:?}");
    assert!(!primaries.contains(&"gti status"), "rows: {primaries:?}");
}

#[test]
fn explicit_history_commands_must_still_be_executable() {
    let directory = tempfile::tempdir().expect("directory");
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).expect("bin");
    let script = bin.join("run.sh");
    fs::write(&script, b"#!/bin/sh\n").expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("executable mode");
    let provider = provider_with_executables(HistoryIndex::default(), &[]);
    let mut current = context("./bin/run.sh --fast", None);
    current.cwd = Arc::new(directory.path().to_owned());

    assert!(provider.plausible_command(&current, "./bin/run.sh --fast"));
    assert!(provider.plausible_command(&current, "env -C bin ./run.sh --fast"));

    fs::set_permissions(&script, fs::Permissions::from_mode(0o600)).expect("plain mode");
    assert!(!provider.plausible_command(&current, "./bin/run.sh --fast"));
    assert!(!provider.plausible_command(&current, "env -C bin ./run.sh --fast"));
}

#[test]
fn every_builtin_can_establish_its_command_prefix() {
    let provider = provider_with_executables(HistoryIndex::default(), &[]);
    for prefix in ["aut", "ret", "seto", "unf", "zmo"] {
        assert_eq!(
            provider.known_command_prefix(&context(prefix, None)),
            Some(prefix.to_owned()),
            "builtin prefix {prefix:?}"
        );
    }
    for shell in [ShellKind::Bash, ShellKind::Zsh, ShellKind::Fish] {
        for builtin in crate::providers::shell_builtins_and_keywords(shell) {
            assert!(crate::providers::is_shell_builtin_or_keyword(
                shell, builtin
            ));
            assert!(crate::providers::shell_symbol_has_prefix(shell, builtin));
            if crate::providers::is_shell_builtin(shell, builtin) {
                assert!(crate::providers::shell_builtin_has_prefix(shell, builtin));
            }
        }
    }
    assert!(!crate::providers::is_shell_builtin(ShellKind::Bash, "if"));
    assert!(!crate::providers::is_shell_builtin(
        ShellKind::Zsh,
        "nocorrect"
    ));
    assert!(!crate::providers::is_shell_builtin(ShellKind::Fish, "not"));
    assert!(crate::providers::is_shell_builtin_or_keyword(
        ShellKind::Bash,
        "shopt"
    ));
    assert!(crate::providers::is_shell_builtin_or_keyword(
        ShellKind::Zsh,
        "autoload"
    ));
    assert!(crate::providers::is_shell_builtin_or_keyword(
        ShellKind::Fish,
        "string"
    ));
    assert!(!crate::providers::is_shell_builtin_or_keyword(
        ShellKind::Bash,
        "autoload"
    ));
}
