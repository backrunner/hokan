use super::*;

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
