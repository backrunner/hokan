use super::*;

#[derive(serde::Deserialize)]
struct HelpCase {
    command: String,
    scope: Vec<String>,
    fixture: String,
    commands: Vec<String>,
}

fn cases() -> Vec<HelpCase> {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/command-help/matrix.json"
    )))
    .expect("help matrix")
}

fn fixture(case: &HelpCase) -> String {
    fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/command-help")
            .join(&case.fixture),
    )
    .expect("captured help")
}

#[test]
fn captured_cli_formats_retain_all_commands_and_no_prose_rows() {
    let mut failures = Vec::new();
    for case in cases() {
        let help = parse_help_output_for_scope(&case.command, &case.scope, &fixture(&case));
        let mut actual: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        let mut expected = case.commands;
        actual.sort();
        expected.sort();
        if actual != expected {
            failures.push(format!(
                "{}: actual {actual:?}; expected {expected:?}",
                case.fixture
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn captured_cli_formats_produce_every_editable_recommendation() {
    let directory = tempfile::tempdir().expect("command directory");
    let cases = cases();
    for case in &cases {
        let executable = directory.path().join(&case.command);
        fs::write(&executable, b"#!/bin/sh\nexit 1\n").expect("fake command");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("command mode");
    }
    let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
        directory.path(),
    ))));
    for case in &cases {
        let cache = Arc::new(CommandHelpCache::default());
        let help = parse_help_output_for_scope(&case.command, &case.scope, &fixture(case));
        if case.scope.is_empty() {
            cache.seed(&case.command, help);
        } else {
            // Each confirmed ancestor permits descending to the next captured scope.
            for (depth, name) in case.scope.iter().enumerate() {
                let ancestor = CommandHelp {
                    subcommands: vec![HelpEntry {
                        name: name.clone(),
                        description: String::new(),
                        takes_value: false,
                    }],
                    subcommands_exhaustive: true,
                    ..CommandHelp::default()
                };
                let scope: Vec<_> = case.scope[..depth].iter().map(String::as_str).collect();
                if scope.is_empty() {
                    cache.seed(&case.command, ancestor);
                } else {
                    cache.seed_scope(&case.command, &scope, ancestor);
                }
            }
            let scope: Vec<_> = case.scope.iter().map(String::as_str).collect();
            cache.seed_scope(&case.command, &scope, help);
        }
        let mut engine = CompletionEngine::new(0, 3);
        engine.register(CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            Arc::clone(&commands),
            cache,
        ));
        let prefix = std::iter::once(case.command.as_str())
            .chain(case.scope.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        let input = format!("{prefix} ");
        let candidates = engine.complete(&context(&input, 1)).candidates;
        let mut actual: Vec<_> = candidates
            .iter()
            .map(|candidate| {
                let edit = candidate.edit.as_ref().expect("editable help row");
                assert_eq!(edit.range, input.len()..input.len());
                assert!(matches!(
                    candidate.completeness,
                    Completeness::NeedsInput { .. }
                ));
                candidate.display.primary.clone()
            })
            .collect();
        let mut expected: Vec<_> = case
            .commands
            .iter()
            .map(|name| format!("{prefix} {name}"))
            .collect();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected, "fixture: {}", case.fixture);
    }
}

#[test]
#[ignore = "requires installed CLIs matching the captured help matrix; probes only help/list output"]
fn installed_cli_help_matrix_recommends_every_captured_command() {
    let commands = Arc::new(CommandPathCache::from_environment());
    let cache = Arc::new(CommandHelpCache::default());
    let mut engine = CompletionEngine::new(0, 3);
    engine.register(CommandHelpProvider::new(
        Arc::new(SpecRegistry::load(None)),
        Arc::clone(&commands),
        cache,
    ));
    let mut total = 0;
    for (index, case) in cases().into_iter().enumerate() {
        assert!(
            commands.contains(&case.command),
            "{} must be installed for the live matrix",
            case.command
        );
        let prefix = std::iter::once(case.command.as_str())
            .chain(case.scope.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        let input = format!("{prefix} ");
        let expected: Vec<_> = case
            .commands
            .iter()
            .map(|name| format!("{prefix} {name}"))
            .collect();
        let context = context(&input, index as u64 + 1);
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let rows: Vec<_> = engine
                .complete(&context)
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect();
            let missing: Vec<_> = expected
                .iter()
                .filter(|name| !rows.contains(name))
                .collect();
            if missing.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{}: missing {missing:?}; actual {rows:?}",
                case.fixture
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        total += expected.len();
        println!(
            "{}: {}/{} captured commands recommended",
            case.fixture,
            expected.len(),
            expected.len()
        );
    }
    println!("Live help matrix passed: {total} captured command recommendations");
}
