use super::*;

fn wait_for_fetch(cache: &CommandHelpCache, command: &str, scope: &[String]) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while cache.scope_is_pending(command, scope) {
        assert!(std::time::Instant::now() < deadline, "help fetch timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn shell_path_change_recovers_negative_root_and_scoped_help() {
    let directory = tempfile::tempdir().expect("script directory");
    let bin = directory.path().join("bin");
    let runtime = directory.path().join("runtime");
    fs::create_dir(&bin).expect("bin");
    fs::create_dir(&runtime).expect("runtime");
    let executable = bin.join("script-cli");
    fs::write(&executable, "#!/usr/bin/env hokan-cache-runtime\n").expect("launcher");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("mode");
    let interpreter = runtime.join("hokan-cache-runtime");
    fs::write(
        &interpreter,
        "#!/bin/sh\nshift\ncase \"$*\" in\n  --help) printf 'Commands:\\n  update Update the CLI\\n' ;;\n  'plugin --help') printf 'Commands:\\n  install Install a plugin\\n' ;;\n  *) exit 1 ;;\nesac\n",
    ).expect("runtime fixture");
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).expect("mode");
    let commands = Arc::new(CommandPathCache::from_path(Some(bin.as_os_str())));
    let cache = Arc::new(CommandHelpCache::with_commands(Arc::clone(&commands)));
    let scope = vec!["plugin".to_owned()];
    cache.request("script-cli", Some(executable.clone()));
    cache.request_scope("script-cli", Some(executable.clone()), scope.clone());
    wait_for_fetch(&cache, "script-cli", &[]);
    wait_for_fetch(&cache, "script-cli", &scope);
    assert!(
        cache
            .peek("script-cli")
            .expect("negative root")
            .subcommands
            .is_empty()
    );
    assert!(
        cache
            .peek_scope("script-cli", &scope)
            .expect("negative scope")
            .subcommands
            .is_empty()
    );

    let child_path = std::env::join_paths([&bin, &runtime]).expect("child PATH");
    assert!(commands.refresh_from_path(Some(&child_path)));
    assert_eq!(
        commands.path("script-cli"),
        Some(fs::canonicalize(&executable).expect("canonical launcher"))
    );
    assert!(cache.peek("script-cli").is_none());
    assert!(cache.peek_scope("script-cli", &scope).is_none());
    cache.request("script-cli", Some(executable.clone()));
    cache.request_scope("script-cli", Some(executable.clone()), scope.clone());
    wait_for_fetch(&cache, "script-cli", &[]);
    wait_for_fetch(&cache, "script-cli", &scope);
    assert_eq!(
        cache.peek("script-cli").expect("root help").subcommands[0].name,
        "update"
    );
    assert_eq!(
        cache
            .peek_scope("script-cli", &scope)
            .expect("scoped help")
            .subcommands[0]
            .name,
        "install"
    );
    assert_eq!(cache.fetch_count(), 4);
    cache.request("script-cli", Some(executable.clone()));
    cache.request_scope("script-cli", Some(executable), scope);
    assert_eq!(
        cache.fetch_count(),
        4,
        "unchanged PATH must still hit the cache"
    );

    // Even an equivalent command map must invalidate cached output, since
    // programs may inspect PATH or use non-executable helper resources.
    let extra = directory.path().join("empty");
    fs::create_dir(&extra).expect("extra path");
    let changed_path = std::env::join_paths([&bin, &runtime, &extra]).expect("changed PATH");
    assert!(commands.refresh_from_path(Some(&changed_path)));
    assert!(cache.peek("script-cli").is_none());
}

#[test]
fn help_finishing_after_a_shell_path_change_is_discarded() {
    let directory = tempfile::tempdir().expect("path fixture");
    let commands = Arc::new(CommandPathCache::default());
    let cache = Arc::new(CommandHelpCache::with_commands(Arc::clone(&commands)));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    cache.request_with("script-cli", move |_| {
        started_tx.send(()).expect("started");
        release_rx.recv().expect("released");
        parse_help_output("script-cli", "Commands:\n  obsolete Old command\n")
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("started probe");
    assert!(commands.refresh_from_path(Some(directory.path().as_os_str())));
    release_tx.send(()).expect("release old probe");
    wait_for_fetch(&cache, "script-cli", &[]);
    assert!(
        cache.peek("script-cli").is_none(),
        "old PATH output must not be cached"
    );
    cache.request_with("script-cli", |_| {
        parse_help_output("script-cli", "Commands:\n  update Current command\n")
    });
    wait_for_fetch(&cache, "script-cli", &[]);
    assert_eq!(
        cache.peek("script-cli").expect("current help").subcommands[0].name,
        "update"
    );
}

#[test]
fn negative_results_are_cached_after_one_fetch() {
    let cache = CommandHelpCache::default();
    let missing = "hokan-definitely-missing-command";
    let first = cache.get(missing);
    assert!(first.flags.is_empty() && first.subcommands.is_empty());
    assert_eq!(cache.fetch_count(), 1);
    let second = cache.get(missing);
    assert_eq!(cache.fetch_count(), 1);
    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn concurrent_cold_misses_share_a_single_fetch() {
    let cache = Arc::new(CommandHelpCache::default());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    // Slow enough that both callers overlap on a cold miss without the
    // cross-fetch lock.
    let fetch = |_: &str| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        CommandHelp::default()
    };
    std::thread::scope(|scope| {
        let worker_cache = Arc::clone(&cache);
        let worker_barrier = Arc::clone(&barrier);
        let fetch = &fetch;
        let worker = scope.spawn(move || {
            worker_barrier.wait();
            worker_cache.get_with("shared-missing", fetch)
        });
        barrier.wait();
        let main = cache.get_with("shared-missing", fetch);
        let spawned = worker.join().expect("worker thread");
        assert!(Arc::ptr_eq(&main, &spawned));
    });
    assert_eq!(cache.fetch_count(), 1);
}

#[test]
fn background_requests_return_immediately_and_dedupe() {
    let cache = Arc::new(CommandHelpCache::default());
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    cache.request_with("slow-tool", move |_| {
        started_tx.send(()).expect("started");
        release_rx.recv().expect("released");
        CommandHelp {
            flags: Vec::new(),
            subcommands: vec![HelpEntry {
                name: "deploy".into(),
                description: "Ship it".into(),
                takes_value: false,
            }],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: false,
        }
    });
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "request must not wait for the fetch"
    );
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("background fetch started");
    assert!(cache.is_pending("slow-tool"));
    cache.request_with("slow-tool", |_| panic!("duplicate fetch"));
    assert_eq!(cache.fetch_count(), 1);
    assert!(cache.peek("slow-tool").is_none());

    release_tx.send(()).expect("release fetch");
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while cache.peek("slow-tool").is_none() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    let help = cache.peek("slow-tool").expect("cached help");
    assert!(!cache.is_pending("slow-tool"));
    assert_eq!(help.subcommands[0].name, "deploy");
}

#[test]
fn resolved_executable_change_invalidates_cached_help() {
    let cache = Arc::new(CommandHelpCache::default());
    let first_path = PathBuf::from("/toolchains/one/demo");
    cache.request_with_path("demo", Some(first_path.clone()), |_| CommandHelp {
        flags: Vec::new(),
        subcommands: vec![HelpEntry {
            name: "first".into(),
            description: String::new(),
            takes_value: false,
        }],
        subcommand_aliases: Vec::new(),
        accepts_positionals: false,
        subcommands_exhaustive: false,
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while cache.peek("demo").is_none() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(
        cache.peek("demo").expect("first help").subcommands[0].name,
        "first"
    );

    // Same resolution is a cache hit; a different executable path gets a
    // fresh parse for the identically named command.
    cache.request_with_path("demo", Some(first_path), |_| panic!("duplicate fetch"));
    cache.request_with_path("demo", Some(PathBuf::from("/toolchains/two/demo")), |_| {
        CommandHelp {
            flags: Vec::new(),
            subcommands: vec![HelpEntry {
                name: "second".into(),
                description: String::new(),
                takes_value: false,
            }],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: false,
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while cache
        .peek("demo")
        .is_none_or(|help| help.subcommands[0].name != "second")
        && std::time::Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    assert_eq!(
        cache.peek("demo").expect("second help").subcommands[0].name,
        "second"
    );
    assert_eq!(cache.fetch_count(), 2);
}

#[test]
fn in_place_executable_upgrade_invalidates_cached_help() {
    fn help(name: &str) -> CommandHelp {
        CommandHelp {
            flags: Vec::new(),
            subcommands: vec![HelpEntry {
                name: name.to_owned(),
                description: String::new(),
                takes_value: false,
            }],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: false,
        }
    }

    let directory = tempfile::tempdir().expect("command directory");
    let executable = directory.path().join("demo");
    fs::write(&executable, b"#!/bin/sh\n").expect("binary");

    let cache = Arc::new(CommandHelpCache::default());
    cache.request_with_path("demo", Some(executable.clone()), |_| help("first"));
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while cache.peek("demo").is_none() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(
        cache.peek("demo").expect("first help").subcommands[0].name,
        "first"
    );

    // A same-path rebuild must not serve the old binary's help: the
    // stamp check at read time drops the entry and the next request
    // refetches.
    fs::write(&executable, b"#!/bin/sh\nexec real-demo --upgraded\n").expect("upgrade");
    assert!(
        cache.peek("demo").is_none(),
        "an in-place upgrade must invalidate the stamped entry"
    );

    cache.request_with_path("demo", Some(executable.clone()), |_| help("second"));
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while cache
        .peek("demo")
        .is_none_or(|help| help.subcommands[0].name != "second")
        && std::time::Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    assert_eq!(
        cache.peek("demo").expect("second help").subcommands[0].name,
        "second"
    );
    assert_eq!(cache.fetch_count(), 2);
}
