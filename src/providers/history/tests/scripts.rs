use super::*;

#[test]
fn package_manager_native_subcommand_typos_are_filtered() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "npm install",
        "npm instal",
        "npm --prefix repo install",
        "npm --prefix repo instal",
        "pnpm -C repo install",
        "pnpm -C repo instal",
        "bun upgrade",
        "bun upgrad",
    ] {
        index.ingest(command, 1_000, ShellKind::Zsh, None, None, &policy);
    }
    let provider = provider_with_executables(index, &["npm", "pnpm", "bun"]);
    let rows = |text: &str| {
        provider
            .complete(&context(text, None))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    assert_eq!(rows("npm i"), ["npm install"]);
    assert_eq!(rows("npm --prefix repo i"), ["npm --prefix repo install"]);
    assert_eq!(rows("pnpm -C repo i"), ["pnpm -C repo install"]);
    assert_eq!(rows("bun up"), ["bun upgrade"]);
}

#[test]
fn package_manager_script_history_is_filtered_by_the_current_manifest() {
    let project = tempfile::tempdir().expect("project");
    let other = tempfile::tempdir().expect("other project");
    fs::write(
        project.path().join("package.json"),
        r#"{"scripts":{"dev":"vite","build":"vite build"}}"#,
    )
    .expect("package manifest");
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "pnpm run dev",
        "pnpm dev",
        "npm run dev",
        "npm run --if-present dev",
        "yarn dev",
        "bun dev",
        "pnpm run dev -- --watch",
    ] {
        index.ingest(
            command,
            1_000,
            ShellKind::Zsh,
            Some(project.path()),
            Some(0),
            &policy,
        );
    }
    for _ in 0..20 {
        index.ingest(
            "pnpm run deploy",
            1_001,
            ShellKind::Zsh,
            Some(other.path()),
            Some(0),
            &policy,
        );
    }
    for command in [
        "pnpm run missing",
        "pnpm missing",
        "pnpm run --if-present missing",
        "npm run --if-present missing",
        "npm run missing --if-present",
        "yarn missing",
        "bun missing",
    ] {
        index.ingest(
            command,
            1_002,
            ShellKind::Zsh,
            Some(other.path()),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_project(
        index,
        &["pnpm", "npm", "yarn", "bun"],
        Arc::new(ProjectCache::default()),
    );
    let rows = |text: &str| {
        provider
            .complete(&context_in(
                project.path(),
                text,
                CompletionMode::HistoryOnly,
            ))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    assert_eq!(rows("pnpm run missing"), Vec::<String>::new());
    assert_eq!(rows("pnpm missing"), Vec::<String>::new());
    assert!(rows("pnpm run --if-present missing").is_empty());
    assert!(rows("npm run --if-present missing").is_empty());
    assert!(rows("npm run missing --if-present").is_empty());
    assert_eq!(rows("pnpm run deploy"), Vec::<String>::new());
    assert_eq!(rows("pnpm run dev -- --watch"), ["pnpm run dev -- --watch"]);
    assert!(rows("npm run dev").contains(&"npm run dev".to_owned()));
    assert!(rows("npm run --if-present dev").contains(&"npm run --if-present dev".to_owned()));
    assert!(rows("yarn dev").contains(&"yarn dev".to_owned()));
    assert!(rows("bun dev").contains(&"bun dev".to_owned()));
    assert!(!rows("yarn missing").contains(&"yarn missing".to_owned()));
    assert!(!rows("bun missing").contains(&"bun missing".to_owned()));
}

#[test]
fn renamed_package_script_invalidates_cached_history_validation() {
    let project = tempfile::tempdir().expect("project");
    let manifest = project.path().join("package.json");
    fs::write(&manifest, r#"{"scripts":{"old":"echo old"}}"#).expect("old manifest");
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in ["pnpm run old", "pnpm run replacement"] {
        index.ingest(
            command,
            1_000,
            ShellKind::Zsh,
            Some(project.path()),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_project(index, &["pnpm"], Arc::new(ProjectCache::default()));
    let rows = |text: &str| {
        provider
            .complete(&context_in(
                project.path(),
                text,
                CompletionMode::HistoryOnly,
            ))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    assert_eq!(rows("pnpm run old"), ["pnpm run old"]);
    fs::write(
        &manifest,
        r#"{"scripts":{"replacement":"echo replacement"}}"#,
    )
    .expect("replacement manifest");
    assert!(rows("pnpm run old").is_empty());
    assert_eq!(rows("pnpm run replacement"), ["pnpm run replacement"]);
}

#[test]
fn invalid_manifest_fails_closed_for_literal_script_history() {
    let project = tempfile::tempdir().expect("project");
    fs::write(
        project.path().join("package.json"),
        br#"{"scripts":{"dev":"#,
    )
    .expect("invalid manifest");
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "pnpm run stale",
        "pnpm stale",
        "npm run stale",
        "node --run=stale",
        "pnpm install",
    ] {
        index.ingest(
            command,
            1_000,
            ShellKind::Zsh,
            Some(project.path()),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_project(
        index,
        &["pnpm", "npm", "node"],
        Arc::new(ProjectCache::default()),
    );
    for command in [
        "pnpm run stale",
        "pnpm stale",
        "npm run stale",
        "node --run=stale",
    ] {
        let rows: Vec<_> = provider
            .complete(&context_in(
                project.path(),
                command,
                CompletionMode::HistoryOnly,
            ))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert!(rows.is_empty(), "invalid manifest leaked {command:?}");
    }

    assert!(
        provider
            .complete(&context_in(project.path(), "pnpm ", CompletionMode::Normal))
            .candidates
            .is_empty(),
        "normal manager history must stay deferred while the manifest is invalid"
    );
}

#[test]
fn node_run_history_uses_the_current_package_scripts() {
    let project = tempfile::tempdir().expect("project");
    fs::write(
        project.path().join("package.json"),
        r#"{"scripts":{"dev":"vite"}}"#,
    )
    .expect("manifest");
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "node --run=dev",
        "node --run dev",
        "node --run=missing",
        "nodejs --run missing",
    ] {
        index.ingest(
            command,
            1_000,
            ShellKind::Zsh,
            Some(project.path()),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_project(
        index,
        &["node", "nodejs"],
        Arc::new(ProjectCache::default()),
    );
    let rows = |text: &str| {
        provider
            .complete(&context_in(
                project.path(),
                text,
                CompletionMode::HistoryOnly,
            ))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    assert!(rows("node --run=dev").contains(&"node --run=dev".to_owned()));
    assert!(rows("node --run dev").contains(&"node --run dev".to_owned()));
    assert!(rows("node --run=missing").is_empty());
    assert!(rows("nodejs --run missing").is_empty());
}

#[test]
fn deno_task_history_uses_the_current_deno_manifest() {
    let project = tempfile::tempdir().expect("project");
    fs::write(
        project.path().join("deno.json"),
        r#"{"tasks":{"dev":"deno run main.ts"}}"#,
    )
    .expect("deno manifest");
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in ["deno task dev", "deno task missing"] {
        index.ingest(
            command,
            1_000,
            ShellKind::Zsh,
            Some(project.path()),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_project(index, &["deno"], Arc::new(ProjectCache::default()));
    let rows = |text: &str| {
        provider
            .complete(&context_in(
                project.path(),
                text,
                CompletionMode::HistoryOnly,
            ))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    assert_eq!(rows("deno task dev"), ["deno task dev"]);
    assert!(rows("deno task missing").is_empty());
}

#[test]
fn package_manager_directory_options_validate_the_selected_manifest() {
    let root = tempfile::tempdir().expect("root");
    let app = root.path().join("app");
    fs::create_dir(&app).expect("app directory");
    fs::write(app.join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).expect("app manifest");
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "pnpm -C app run dev",
        "pnpm -Capp run dev",
        "pnpm run -C app dev",
        "npm --prefix app run dev",
        "npm --prefix=app run dev",
        "npm run --prefix app dev",
        "yarn --cwd app run dev",
        "bun --cwd app run dev",
        "pnpm -C app run missing",
        "npm --prefix app run missing",
    ] {
        index.ingest(
            command,
            1_000,
            ShellKind::Zsh,
            Some(root.path()),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_project(
        index,
        &["pnpm", "npm", "yarn", "bun"],
        Arc::new(ProjectCache::default()),
    );
    let rows = |text: &str| {
        provider
            .complete(&context_in(root.path(), text, CompletionMode::HistoryOnly))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    for command in [
        "pnpm -C app run dev",
        "pnpm -Capp run dev",
        "pnpm run -C app dev",
        "npm --prefix app run dev",
        "npm --prefix=app run dev",
        "npm run --prefix app dev",
        "yarn --cwd app run dev",
        "bun --cwd app run dev",
    ] {
        assert!(
            rows(command).contains(&command.to_owned()),
            "valid selected-project script was filtered for {command:?}"
        );
    }
    assert!(rows("pnpm -C app run missing").is_empty());
    assert!(rows("npm --prefix app run missing").is_empty());
}

#[test]
fn compound_history_uses_the_directory_selected_by_cd() {
    let root = tempfile::tempdir().expect("root");
    let app = root.path().join("app");
    let dashed = root.path().join("-app");
    fs::create_dir(&app).expect("app directory");
    fs::create_dir(&dashed).expect("dashed directory");
    fs::write(
        root.path().join("package.json"),
        r#"{"scripts":{"root-script":"echo root"}}"#,
    )
    .expect("root manifest");
    for directory in [&app, &dashed] {
        fs::write(
            directory.join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .expect("app manifest");
    }
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "cd app && pnpm dev",
        "cd -P app; pnpm dev",
        "cd -- -app && pnpm dev",
        "cd app && pnpm missing",
        "cd missing || pnpm root-script",
        "cd missing || pnpm dev",
        "cd app || pnpm root-script; pnpm dev",
        "cd app || pnpm root-script; pnpm missing",
        "cd missing && pnpm root-script",
        "cd missing && pnpm dev",
    ] {
        index.ingest(
            command,
            1_000,
            ShellKind::Zsh,
            Some(root.path()),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_project(index, &["pnpm"], Arc::new(ProjectCache::default()));
    let rows = |text: &str| {
        provider
            .complete(&context_in(root.path(), text, CompletionMode::HistoryOnly))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    for command in [
        "cd app && pnpm dev",
        "cd -P app; pnpm dev",
        "cd -- -app && pnpm dev",
        "cd missing || pnpm root-script",
        "cd app || pnpm root-script; pnpm dev",
        "cd missing && pnpm root-script",
    ] {
        assert!(
            rows(command).contains(&command.to_owned()),
            "valid compound history was filtered for {command:?}"
        );
    }
    assert!(!rows("cd app && pnpm missing").contains(&"cd app && pnpm missing".to_owned()));
    assert!(!rows("cd missing || pnpm dev").contains(&"cd missing || pnpm dev".to_owned()));
    assert!(
        !rows("cd app || pnpm root-script; pnpm missing")
            .contains(&"cd app || pnpm root-script; pnpm missing".to_owned()),
        "cwd from a successful cd must survive a skipped || branch"
    );
    assert!(!rows("cd missing && pnpm dev").contains(&"cd missing && pnpm dev".to_owned()));
}

#[test]
fn workspace_script_history_respects_member_and_recursive_semantics() {
    let root = tempfile::tempdir().expect("workspace");
    let app = root.path().join("packages/app");
    let api = root.path().join("packages/api");
    fs::create_dir_all(&app).expect("app");
    fs::create_dir_all(&api).expect("api");
    fs::write(
        root.path().join("package.json"),
        r#"{"scripts":{"root-only":"echo root"},"workspaces":["packages/*"]}"#,
    )
    .expect("root manifest");
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","scripts":{"dev":"vite"}}"#,
    )
    .expect("app manifest");
    fs::write(
        api.join("package.json"),
        r#"{"name":"api","scripts":{"build":"tsc"}}"#,
    )
    .expect("api manifest");
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "pnpm --filter app run dev",
        "pnpm --filter app --filter api run dev",
        "npm --workspace app run dev",
        "npm --workspace app --workspace api run dev",
        "npm --workspace app --workspace api --if-present run dev",
        "yarn workspace app run dev",
        "pnpm -r run dev",
        "npm --workspaces run dev",
        "npm --workspaces --if-present run dev",
        "pnpm -w run root-only",
        "pnpm --filter app run missing",
        "pnpm --filter app --filter api run missing",
        "pnpm --filter app run --if-present missing",
        "npm --workspace app run --if-present missing",
        "npm --workspace app --workspace api run missing",
        "npm --workspace app --workspace api --if-present run missing",
    ] {
        index.ingest(
            command,
            1_000,
            ShellKind::Zsh,
            Some(root.path()),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_project(
        index,
        &["pnpm", "npm", "yarn"],
        Arc::new(ProjectCache::default()),
    );
    let rows = |text: &str| {
        provider
            .complete(&context_in(root.path(), text, CompletionMode::HistoryOnly))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    for command in [
        "pnpm --filter app run dev",
        "pnpm --filter app --filter api run dev",
        "npm --workspace app run dev",
        "npm --workspace app --workspace api --if-present run dev",
        "yarn workspace app run dev",
        "pnpm -r run dev",
        "npm --workspaces --if-present run dev",
        "pnpm -w run root-only",
    ] {
        assert!(
            rows(command).contains(&command.to_owned()),
            "valid workspace script was filtered for {command:?}"
        );
    }
    assert!(rows("pnpm --filter app run missing").is_empty());
    assert!(rows("pnpm --filter app --filter api run missing").is_empty());
    assert!(rows("pnpm --filter app run --if-present missing").is_empty());
    assert!(rows("npm --workspace app run --if-present missing").is_empty());
    assert!(rows("npm --workspace app --workspace api run missing").is_empty());
    assert!(rows("npm --workspace app --workspace api --if-present run missing").is_empty());
    assert!(
        !rows("npm --workspaces run dev").contains(&"npm --workspaces run dev".to_owned()),
        "npm without --if-present must require every selected workspace"
    );
    assert!(
        !rows("npm --workspace app --workspace api run dev")
            .contains(&"npm --workspace app --workspace api run dev".to_owned()),
        "npm multi-workspace scripts must require every selected workspace"
    );
}

#[test]
fn package_manager_nested_subcommand_typos_are_filtered() {
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "pnpm store prune",
        "pnpm store prun",
        "npm cache clean",
        "npm cache cler",
    ] {
        index.ingest(command, 1_000, ShellKind::Zsh, None, None, &policy);
    }
    let help = Arc::new(CommandHelpCache::default());
    let scoped_help = |names: &[&str]| CommandHelp {
        flags: Vec::new(),
        subcommands: names
            .iter()
            .map(|name| HelpEntry {
                name: (*name).to_owned(),
                description: String::new(),
                takes_value: false,
            })
            .collect(),
        subcommand_aliases: Vec::new(),
        accepts_positionals: false,
        subcommands_exhaustive: true,
    };
    help.seed_scope(
        "pnpm",
        &["store"],
        scoped_help(&["add", "path", "prune", "status"]),
    );
    help.seed_scope(
        "npm",
        &["cache"],
        scoped_help(&["add", "clean", "ls", "verify"]),
    );
    let provider = provider_with_executables_and_help(index, &["npm", "pnpm"], Arc::clone(&help));
    let rows = |text: &str| {
        provider
            .complete(&context(text, None))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect::<Vec<_>>()
    };
    assert_eq!(rows("pnpm store pr"), ["pnpm store prune"]);
    assert_eq!(rows("npm cache cl"), ["npm cache clean"]);
}

#[test]
fn static_specs_do_not_make_missing_commands_plausible() {
    assert!(crate::specs::SpecRegistry::load(None).get("ls").is_some());
    let provider = provider_with_executables(HistoryIndex::default(), &[]);
    assert!(!provider.plausible_command(&context("ls -la", None), "ls -la"));
}
