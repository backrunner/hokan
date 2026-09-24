use super::*;

#[test]
fn pnpm_root_help_probes_the_complete_command_list_first() {
    assert_eq!(
        help_probe_arguments("pnpm", &[]),
        [
            vec!["help".to_owned(), "-a".to_owned()],
            vec!["--help".to_owned()],
        ]
    );
    assert_eq!(
        help_probe_arguments("pnpm", &["store".to_owned()]),
        [vec!["store".to_owned(), "--help".to_owned()]]
    );
    assert_eq!(
        help_probe_arguments("swift", &["package".to_owned()]),
        [
            vec!["help".to_owned(), "package".to_owned()],
            vec!["package".to_owned(), "--help".to_owned()],
        ]
    );
}

#[test]
fn help_fallback_fills_a_man_page_without_subcommands() {
    use std::cell::Cell;

    let help_calls = Cell::new(0);
    let counting_help = |_: &str| {
        help_calls.set(help_calls.get() + 1);
        CommandHelp {
            flags: Vec::new(),
            subcommands: vec![HelpEntry {
                name: "apply".into(),
                description: String::new(),
                takes_value: false,
            }],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: false,
        }
    };
    let man_with_flags = |_: &str| CommandHelp {
        flags: vec![HelpEntry {
            name: "-x".into(),
            description: String::new(),
            takes_value: false,
        }],
        subcommands: Vec::new(),
        subcommand_aliases: Vec::new(),
        accepts_positionals: false,
        subcommands_exhaustive: false,
    };
    // Preserve parsed man flags while filling its missing command list.
    let result = fetch_with_fallback("demo", man_with_flags, counting_help);
    assert_eq!(result.flags.len(), 1);
    assert_eq!(result.subcommands.len(), 1);
    assert_eq!(help_calls.get(), 1);
    // An empty man parse falls back to `--help` exactly once.
    let result = fetch_with_fallback("demo", |_| CommandHelp::default(), counting_help);
    assert_eq!(result.subcommands.len(), 1);
    assert_eq!(help_calls.get(), 2);
    // A nonempty man command list is not necessarily complete either.
    let result = fetch_with_fallback(
        "demo",
        |_| parse_man_page("demo", "COMMANDS\n  status  Show status\n"),
        counting_help,
    );
    assert_eq!(result.subcommands.len(), 2);
    assert_eq!(help_calls.get(), 3);
    assert!(result.subcommands.iter().any(|entry| entry.name == "apply"));
    // Both empty: the negative result is returned for caching.
    let result = fetch_with_fallback(
        "demo",
        |_| CommandHelp::default(),
        |_| CommandHelp::default(),
    );
    assert_eq!(result, CommandHelp::default());
}

#[test]
fn homebrew_help_augments_a_nonempty_man_command_list() {
    let entry = |name: &str| HelpEntry {
        name: name.into(),
        description: String::new(),
        takes_value: false,
    };
    let result = fetch_with_fallback(
        "brew",
        |_| CommandHelp {
            flags: Vec::new(),
            subcommands: vec![entry("install"), entry("doctor")],
            subcommand_aliases: vec!["dr".into()],
            accepts_positionals: false,
            subcommands_exhaustive: false,
        },
        |_| CommandHelp {
            flags: Vec::new(),
            subcommands: vec![entry("install"), entry("update")],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: false,
        },
    );
    let names: Vec<_> = result
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["install", "update", "doctor"]);
    assert_eq!(result.subcommand_aliases, ["dr"]);
}

#[test]
fn failing_or_hanging_help_degrades_to_empty() {
    let directory = tempfile::tempdir().expect("script directory");
    let failing = directory.path().join("failing");
    fs::write(&failing, "#!/bin/sh\nexit 1\n").expect("failing script");
    fs::set_permissions(&failing, fs::Permissions::from_mode(0o700)).expect("failing mode");
    let hanging = directory.path().join("hanging");
    // Note: `run_bounded` joins its output readers after killing the
    // direct child, so a grandchild holding the pipe keeps the fetch
    // blocked until it exits — keep this sleep short.
    fs::write(&hanging, "#!/bin/sh\nsleep 2\n").expect("hanging script");
    fs::set_permissions(&hanging, fs::Permissions::from_mode(0o700)).expect("hanging mode");
    assert_eq!(
        fetch_help_output(failing.to_str().expect("failing path")),
        CommandHelp::default()
    );
    assert_eq!(
        fetch_help_output(hanging.to_str().expect("hanging path")),
        CommandHelp::default()
    );
}

#[test]
fn successful_help_script_is_parsed_end_to_end() {
    let directory = tempfile::tempdir().expect("script directory");
    let tool = directory.path().join("demotool");
    fs::write(
        &tool,
        "#!/bin/sh\nprintf 'Commands:\\n  deploy   Ship it.\\n'\n",
    )
    .expect("help script");
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).expect("script mode");
    let help = fetch_help_output(tool.to_str().expect("script path"));
    assert_eq!(help.subcommands.len(), 1);
    assert_eq!(help.subcommands[0].name, "deploy");
    assert_eq!(help.subcommands[0].description, "Ship it.");
}

#[test]
fn known_help_entrypoints_and_stderr_help_are_accepted() {
    let directory = tempfile::tempdir().expect("script directory");
    let go = directory.path().join("go");
    fs::write(
            &go,
            "#!/bin/sh\n[ \"$1\" = help ] || exit 3\nprintf 'The commands are:\\n  build   Compile packages.\\n' >&2\n",
        )
        .expect("help script");
    fs::set_permissions(&go, fs::Permissions::from_mode(0o700)).expect("script mode");
    assert!(looks_like_help_output(
        "error: help requested\nThe commands are:\n"
    ));
    let help = fetch_help_program("go", go.as_os_str());
    assert_eq!(help.subcommands.len(), 1);
    assert_eq!(help.subcommands[0].name, "build");
}
