use super::*;

#[test]
fn parses_and_merges_all_documented_entries_past_two_hundred() {
    let mut modern = String::from("Commands:\n");
    let mut man = String::from("COMMANDS\n");
    for index in 0..260 {
        modern.push_str(&format!("  command{index:03}  Run command {index}\n"));
        man.push_str(&format!("  extra{index:03}  Run extra {index}\n"));
    }
    modern.push_str("Options:\n");
    man.push_str("OPTIONS\n");
    for index in 0..260 {
        modern.push_str(&format!("  --flag{index:03}  Enable flag {index}\n"));
        man.push_str(&format!("  --extra{index:03}  Enable extra {index}\n"));
    }
    let help = parse_help_output("demo", &modern);
    assert_eq!(help.subcommands.len(), 260);
    assert_eq!(help.flags.len(), 260);
    assert!(help.subcommands_exhaustive);
    let merged = merge_help(help, parse_man_page("demo", &man));
    assert_eq!(merged.subcommands.len(), 520);
    assert_eq!(merged.flags.len(), 520);
    assert!(merged.subcommands_exhaustive);
    assert_eq!(merged.subcommands[519].name, "extra259");
}

#[test]
fn parses_go_prose_header_without_leaking_help_topics() {
    let help = parse_help_output("go", GO_HELP);
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["bug", "build", "mod"]);
    assert!(help.subcommands_exhaustive);
}

#[test]
fn parses_git_native_help_command_groups() {
    let help = parse_help_output(
        "git",
        r#"These are common Git commands used in various situations:

start a working area
   clone      Clone a repository
   init       Create an empty repository

work on the current change
   add        Add file contents to the index
"#,
    );
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["clone", "init", "add"]);
}

#[test]
fn parses_uppercase_colon_command_groups_without_help_topics() {
    let help = parse_help_output("gh", GH_HELP);
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["auth", "pr"]);
    assert!(!names.contains(&"accessibility"));
}

#[test]
fn parses_openssl_standard_command_grid_only() {
    let help = parse_help_output("openssl", OPENSSL_HELP);
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["asn1parse", "ca", "ciphers", "x509"]);
    assert!(!names.contains(&"sha256"));
    assert!(!names.contains(&"aes-128-cbc"));
}

#[test]
fn parses_usage_signatures_and_prefers_full_comma_aliases() {
    let help = parse_help_output("demo", SIGNATURE_HELP);
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["agents", "install", "plugin"]);
    assert_eq!(help.subcommand_aliases, ["i", "plugins"]);
}

#[test]
fn parses_argparse_positional_command_choices() {
    let help = parse_help_output(
        "conda",
        "usage: conda [-h] COMMAND ...\n\npositional arguments:\n  {activate,clean,create,install}\n                        Command to run\n\noptions:\n  -h, --help            show this help message\n",
    );
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["activate", "clean", "create", "install"]);
    assert!(help.accepts_positionals);
    assert!(help.subcommands_exhaustive);
}

#[test]
fn parses_pnpm_categorized_and_npm_comma_command_lists() {
    let pnpm = parse_help_output(
        "pnpm",
        "Manage your dependencies:\n  i, install       Install dependencies\n  clean            Remove node_modules\n\nManage your store:\n  store add        Add packages\n\nOptions:\n  -r, --recursive  Run recursively\n",
    );
    assert_eq!(
        pnpm.subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["install", "clean", "store"]
    );
    assert_eq!(pnpm.subcommand_aliases, ["i"]);
    assert!(pnpm.subcommands_exhaustive);

    let npm = parse_help_output(
        "npm",
        "All commands:\n    access, adduser, audit, cache, config, install, run\n\nOptions:\n  -h, --help  Show help\n",
    );
    assert_eq!(
        npm.subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        [
            "access", "adduser", "audit", "cache", "config", "install", "run"
        ]
    );
    assert!(npm.subcommands_exhaustive);
}

#[test]
fn parses_scoped_usage_and_literal_invocation_rows() {
    let npm_scope = vec!["cache".to_owned()];
    let npm = parse_help_output_for_scope(
        "npm",
        &npm_scope,
        "Usage:\nnpm cache add <package-spec>\nnpm cache clean [<key>]\nnpm cache ls [<name>]\nnpm cache verify\n\nOptions:\n  --cache <path>\n",
    );
    assert_eq!(
        npm.subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["add", "clean", "ls", "verify"]
    );
    assert!(!npm.subcommands_exhaustive);

    let bun_scope = vec!["pm".to_owned()];
    let bun = parse_help_output_for_scope(
        "bun",
        &bun_scope,
        "bun pm: Package manager utilities\n\n  bun pm pack       create a tarball\n  bun pm bin        print the bin folder\n  bun pm cache      print the cache folder\n  bun pm cache rm   clear the cache\n",
    );
    assert_eq!(
        bun.subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["pack", "bin", "cache"]
    );

    let bun_cache_scope = vec!["pm".to_owned(), "cache".to_owned()];
    let bun_cache = parse_help_output_for_scope(
        "bun",
        &bun_cache_scope,
        "  bun pm cache      print the cache folder\n  bun pm cache rm   clear the cache\n",
    );
    assert_eq!(
        bun_cache
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["rm"]
    );
}

#[test]
fn bracketed_flag_arguments_and_documented_values_are_recognized() {
    assert!(flag_takes_separate_value("[<SPEC>]  Select a package"));
    assert!(flag_takes_separate_value("[DIRECTORY]  Change directory"));
    assert!(!flag_takes_separate_value(
        "[=<WHEN>]  Optional attached value"
    ));
    assert_eq!(
        documented_value_choices("Coloring [possible values: auto, always, never]"),
        ["auto", "always", "never"]
    );
    assert_eq!(
        documented_value_choices("Build mode (values: debug, release; default: debug)"),
        ["debug", "release"]
    );
    assert!(documented_value_choices("Set an arbitrary string value").is_empty());
}

#[test]
fn parses_kubectl_style_help() {
    let help = parse_help_output("kubectl", KUBECTL_HELP);
    assert!(help.subcommands_exhaustive);
    let names: Vec<&str> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["apply", "api-versions", "create", "get"]);
    assert_eq!(
        help.subcommands[1].description,
        "Print the supported API versions on the server"
    );
    let flags: Vec<(&str, &str)> = help
        .flags
        .iter()
        .map(|entry| (entry.name.as_str(), entry.description.as_str()))
        .collect();
    assert_eq!(
        flags,
        [
            ("-h", "help for kubectl"),
            ("--help", "help for kubectl"),
            ("--kubeconfig", "Path to the kubeconfig file"),
        ]
    );
}

#[test]
fn parses_docker_style_help_with_management_commands() {
    let help = parse_help_output("docker", DOCKER_HELP);
    assert!(help.subcommands_exhaustive);
    let names: Vec<&str> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["builder", "container", "attach", "build"]);
    let flags: Vec<&str> = help.flags.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(flags, ["--config", "-D", "--debug"]);
    assert_eq!(help.flags[0].description, "Location of client config files");
    assert!(help.flags[0].takes_value);
    assert!(!help.flags[1].takes_value);
}

#[test]
fn parses_cargo_style_help_with_aliases() {
    let help = parse_help_output("cargo", CARGO_HELP);
    assert!(help.subcommands_exhaustive);
    let names: Vec<&str> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["build", "check", "run"]);
    assert_eq!(
        help.subcommands[0].description,
        "Compile the current package"
    );
    let flags: Vec<&str> = help.flags.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(flags, ["-V", "--version"]);
    assert_eq!(help.subcommand_aliases, ["b", "c"]);
}

#[test]
fn parses_pipe_and_description_subcommand_aliases() {
    let help = parse_help_output("ai", AI_CLI_HELP);
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["exec", "apply", "update"]);
    assert_eq!(help.subcommand_aliases, ["e", "a", "upgrade"]);
    assert!(help.subcommands_exhaustive);
    assert!(help.accepts_positionals);
    for alias in ["e", "a", "upgrade"] {
        assert!(history_arguments_are_plausible(
            &help,
            &[alias],
            false,
            false
        ));
    }
    assert!(!history_arguments_are_plausible(
        &help,
        &["upgrad"],
        false,
        false
    ));
    assert!(history_arguments_are_plausible(
        &help,
        &["fix", "this", "bug"],
        false,
        false
    ));
}

#[test]
fn parses_homebrew_invocation_sections_without_claiming_exhaustiveness() {
    let help = parse_help_output("brew", BREW_HELP);
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "search",
            "info",
            "install",
            "update",
            "upgrade",
            "uninstall",
            "list",
            "config",
            "doctor",
            "create",
            "edit",
            "commands",
            "help",
        ]
    );
    assert_eq!(help.subcommands[2].description, "FORMULA|CASK...");
    assert!(!help.subcommands_exhaustive);
    assert!(!help.accepts_positionals);
}

#[test]
fn proven_hidden_subcommands_survive_a_closed_help_surface() {
    let help = CommandHelp {
        flags: Vec::new(),
        subcommands: vec![HelpEntry {
            name: "deploy".into(),
            description: String::new(),
            takes_value: false,
        }],
        subcommand_aliases: Vec::new(),
        accepts_positionals: false,
        subcommands_exhaustive: true,
    };
    assert!(!history_arguments_are_plausible(
        &help,
        &["hidden"],
        false,
        false
    ));
    assert!(history_arguments_are_plausible(
        &help,
        &["hidden"],
        true,
        false
    ));
}

#[test]
fn help_garbage_yields_no_entries() {
    assert_eq!(parse_help_output("demo", ""), CommandHelp::default());
    assert_eq!(
        parse_help_output("demo", "just some prose\n"),
        CommandHelp::default()
    );
    // A commands header with no parseable rows still yields no
    // subcommands; rows outside any commands section are ignored.
    let orphan_rows = "  start   Start the service.\nCommands:\n\nFlags:\n";
    let help = parse_help_output("demo", orphan_rows);
    assert!(help.subcommands.is_empty());
    assert!(!help.subcommands_exhaustive);
}
