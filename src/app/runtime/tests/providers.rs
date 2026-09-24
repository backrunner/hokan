use super::*;

#[test]
fn complete_executable_still_schedules_a_provider_query() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("directory");
    let bin = directory.path().join("bin");
    std::fs::create_dir(&bin).expect("bin directory");
    let executable = bin.join("ls");
    std::fs::write(&executable, b"#!/bin/sh\n").expect("write executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("executable mode");

    let mut state = runtime_state(directory.path());
    state.commands.refresh_from_path(Some(bin.as_os_str()));
    state.buffer.set_exact("ls".into(), 2).expect("buffer");
    let mut engine = crate::completion::CompletionEngine::new(20, 12);
    engine.register(crate::providers::CommandSpecProvider::new(
        Arc::new(crate::specs::SpecRegistry::load(None)),
        Arc::clone(&state.commands),
    ));
    let worker = ProviderWorker::start(Arc::new(engine), None).expect("provider worker");

    state.schedule_query(&worker).expect("schedule query");
    assert!(
        state.context.is_some(),
        "a complete executable must not suppress the query"
    );
    assert!(state.provider_pending);

    // Results do not implicitly enter the list. The first navigation action
    // is what creates the selection (covered by move_selection tests below).
    let result = worker
        .results()
        .recv_timeout(Duration::from_secs(2))
        .expect("provider result");
    let (output, join) = test_output();
    handle_provider_result(result, &mut state, &output).expect("provider result handling");
    assert!(!state.candidates.is_empty());
    assert_eq!(state.selected, None);
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
    drop(worker);
}

#[test]
fn runtime_engine_runs_dynamic_help_before_competing_command_sources() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = ConfigPaths {
        config_file: directory.path().join("config.toml"),
        credentials_file: directory.path().join("credentials.toml"),
        specs_directory: directory.path().join("specs"),
        state_directory: directory.path().join("state"),
        cache_directory: directory.path().join("cache"),
    };
    let history = Arc::new(RwLock::new(HistoryIndex::default()));
    let config = Arc::new(Config::default());
    let (engine, _, _, _) = build_engine(&paths, &config, history, None);
    let providers = engine.provider_ids();
    let help = providers
        .iter()
        .position(|provider| *provider == "command_help")
        .expect("command-help provider");

    for provider in [
        "session_command",
        "path_command",
        "history",
        "alias",
        "filesystem",
    ] {
        let position = providers
            .iter()
            .position(|candidate| *candidate == provider)
            .unwrap_or_else(|| panic!("{provider} provider"));
        assert!(
            help < position,
            "command help must run before {provider}: {providers:?}"
        );
    }
}

#[test]
fn exact_path_command_prefetches_help_but_project_paths_stay_cold() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("directory");
    let bin = directory.path().join("bin");
    std::fs::create_dir(&bin).expect("bin directory");
    let executable = bin.join("codex-fixture");
    std::fs::write(
        &executable,
        b"#!/bin/sh\nprintf '%s\\n' 'Commands:' '  resume    Resume a session'\n",
    )
    .expect("write executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("executable mode");
    let commands = CommandPathCache::from_path(Some(&std::ffi::OsString::from(&bin)));
    let specs = crate::specs::SpecRegistry::default();
    let help = Arc::new(crate::providers::CommandHelpCache::default());
    let context = CompletionContext::new(
        QueryId::new(1),
        ShellKind::Zsh,
        directory.path().to_owned(),
        BufferSnapshot::new(
            Arc::<str>::from("codex-fixture"),
            "codex-fixture".len(),
            BufferRevision::ZERO,
            SyncQuality::Exact,
        )
        .expect("snapshot"),
    )
    .expect("context");

    super::state::prefetch_command_help(&context, &commands, &specs, &help);
    assert_eq!(help.fetch_count(), 1);
    // A repeated buffer event must share the same pending/cache entry.
    super::state::prefetch_command_help(&context, &commands, &specs, &help);
    assert_eq!(help.fetch_count(), 1);

    let explicit_help = Arc::new(crate::providers::CommandHelpCache::default());
    let explicit = CompletionContext::new(
        QueryId::new(2),
        ShellKind::Zsh,
        directory.path().to_owned(),
        BufferSnapshot::new(
            Arc::<str>::from("./bin/codex-fixture"),
            "./bin/codex-fixture".len(),
            BufferRevision::ZERO,
            SyncQuality::Exact,
        )
        .expect("snapshot"),
    )
    .expect("context");
    super::state::prefetch_command_help(&explicit, &commands, &specs, &explicit_help);
    assert_eq!(
        explicit_help.fetch_count(),
        0,
        "project-local executables must not be launched just to discover help"
    );
}

#[test]
fn child_shell_path_event_refreshes_the_shared_provider_cache() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("directory");
    let bin = directory.path().join("child-bin");
    std::fs::create_dir(&bin).expect("bin directory");
    let executable = bin.join("second-command");
    std::fs::write(&executable, b"#!/bin/sh\n").expect("write executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("executable mode");

    let mut state = runtime_state(directory.path());
    state.editing = true;
    state.buffer.set_exact("second".into(), 6).expect("buffer");
    let mut engine = crate::completion::CompletionEngine::new(20, 12);
    engine.register(crate::providers::PathCommandProvider::new(Arc::clone(
        &state.commands,
    )));
    let worker = ProviderWorker::start(Arc::new(engine), None).expect("provider worker");
    let (output, join) = test_output();
    let store = HistoryStore::open(&directory.path().join("state")).expect("history store");
    let history = Arc::new(RwLock::new(HistoryIndex::default()));
    let policy = HistoryPolicy::new(1024, &[]).expect("history policy");

    handle_control_message(
        ControlMessage::Event(ShellEvent::PathChanged {
            path: bin.as_os_str().to_owned(),
        }),
        &mut state,
        &output,
        &worker,
        &store,
        &history,
        &policy,
    )
    .expect("PATH event");

    assert!(state.commands.contains("second-command"));
    let result = worker
        .results()
        .recv_timeout(Duration::from_secs(2))
        .expect("refreshed completion result");
    assert!(result.output.candidates.iter().any(|candidate| {
        candidate.display.primary == "second-command"
            && candidate.source == CandidateSource::PathCommand
    }));

    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn background_help_refresh_does_not_steal_owned_overlay_states() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state.editing = true;
    state
        .buffer
        .set_exact("natural language request".into(), 24)
        .expect("buffer");
    state.ai_owns_candidates = true;
    state.help.bump_revision();

    let worker = ProviderWorker::start(
        Arc::new(crate::completion::CompletionEngine::new(20, 12)),
        None,
    )
    .expect("provider worker");
    let before = state.query_id;
    state
        .refresh_help_results(&worker)
        .expect("refresh help revision");

    assert_eq!(state.query_id, before, "AI-owned rows must not be replaced");
    assert_eq!(state.help_revision, state.help.revision());
}
