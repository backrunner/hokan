use super::*;

#[test]
fn pipeline_and_background_cd_do_not_change_history_validation_cwd() {
    let root = tempfile::tempdir().expect("root");
    let app = root.path().join("app");
    fs::create_dir(&app).expect("app directory");
    fs::write(
        root.path().join("package.json"),
        r#"{"scripts":{"root-script":"echo root"}}"#,
    )
    .expect("root manifest");
    fs::write(app.join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).expect("app manifest");
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in [
        "cd app | pnpm root-script",
        "cd app | pnpm dev",
        "cd app & pnpm root-script",
        "cd app & pnpm dev",
        "cd app && pnpm dev & pnpm root-script",
        "cd app && pnpm dev & pnpm dev",
        "cd app & cd app & pnpm root-script",
        "cd app & cd app & pnpm dev",
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
    assert!(rows("cd app | pnpm root-script").contains(&"cd app | pnpm root-script".to_owned()));
    assert!(rows("cd app & pnpm root-script").contains(&"cd app & pnpm root-script".to_owned()));
    assert!(
        rows("cd app && pnpm dev & pnpm root-script")
            .contains(&"cd app && pnpm dev & pnpm root-script".to_owned()),
        "the background group must use app internally and restore root afterward"
    );
    assert!(
        rows("cd app & cd app & pnpm root-script")
            .contains(&"cd app & cd app & pnpm root-script".to_owned()),
        "each consecutive background group must restore the parent cwd"
    );
    for command in [
        "cd app | pnpm dev",
        "cd app & pnpm dev",
        "cd app && pnpm dev & pnpm dev",
        "cd app & cd app & pnpm dev",
    ] {
        assert!(
            !rows(command).contains(&command.to_owned()),
            "invalid background or pipeline script leaked for {command:?}"
        );
    }
}

#[test]
fn dynamic_cd_history_fails_open_instead_of_using_the_wrong_manifest() {
    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join("package.json"), r#"{"scripts":{}}"#).expect("root manifest");
    let provider = provider_with_project(
        HistoryIndex::default(),
        &["pnpm"],
        Arc::new(ProjectCache::default()),
    );
    for command in [
        "cd - && pnpm dev",
        "cd $PROJECT && pnpm dev",
        "cd old new; pnpm dev",
    ] {
        assert!(
            provider.plausible_command(
                &context_in(root.path(), command, CompletionMode::HistoryOnly),
                command,
            ),
            "dynamic cwd was incorrectly validated for {command:?}"
        );
    }
}

#[test]
fn wrapped_manager_history_still_uses_the_current_manifest() {
    let project = tempfile::tempdir().expect("project");
    fs::write(
        project.path().join("package.json"),
        r#"{"scripts":{"dev":"vite"}}"#,
    )
    .expect("manifest");
    let provider = provider_with_project(
        HistoryIndex::default(),
        &["corepack", "pnpm", "npm", "sudo", "env"],
        Arc::new(ProjectCache::default()),
    );
    for command in [
        "corepack pnpm run dev",
        "sudo pnpm dev",
        "env -C . npm run dev",
    ] {
        assert!(
            provider.plausible_command(
                &context_in(project.path(), command, CompletionMode::HistoryOnly),
                command,
            ),
            "valid wrapped script was filtered for {command:?}"
        );
    }
    for command in [
        "corepack pnpm run missing",
        "sudo pnpm missing",
        "env -C . npm run missing",
    ] {
        assert!(
            !provider.plausible_command(
                &context_in(project.path(), command, CompletionMode::HistoryOnly),
                command,
            ),
            "invalid wrapped script leaked for {command:?}"
        );
    }
}
