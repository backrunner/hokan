use super::*;

#[test]
fn drops_candidates_identical_to_the_current_buffer() {
    let context = buffer_context("git status");
    let ranked = rank_and_dedupe(
        &context,
        vec![
            history_candidate(&context, "git status", 10),
            history_candidate(&context, "git status ", 10),
            history_candidate(&context, "git status --short", 10),
        ],
        10,
    );
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].display.primary, "git status --short");
}

#[test]
fn preserves_candidates_that_change_meaningful_shell_whitespace() {
    for (typed, completed) in [
        ("printf a\\", "printf a\\ "),
        ("printf 'a", "printf 'a "),
        ("echo", "echo\u{00a0}"),
    ] {
        let context = buffer_context(typed);
        let ranked = rank_and_dedupe(
            &context,
            vec![history_candidate(&context, completed, typed.len())],
            10,
        );
        assert_eq!(ranked.len(), 1, "lost completion {completed:?}");
        assert_eq!(
            ranked[0].edit.as_ref().expect("edit").replacement,
            completed
        );
    }
}

#[test]
fn drops_editless_candidates_matching_the_current_buffer() {
    let context = buffer_context("make build");
    let editless = Candidate::new(
        context.query_id,
        "make build",
        "project",
        None,
        CandidateAction::None,
        CandidateSource::Project,
        CandidateKind::Recipe,
        Completeness::Runnable,
        RiskLevel::Low,
        "project",
    );
    let different = Candidate::new(
        context.query_id,
        "make build release",
        "project",
        None,
        CandidateAction::None,
        CandidateSource::Project,
        CandidateKind::Recipe,
        Completeness::Runnable,
        RiskLevel::Low,
        "project-x",
    );
    let ranked = rank_and_dedupe(&context, vec![editless, different], 10);
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].display.primary, "make build release");
}

#[test]
fn strong_prefix_beats_highly_personalized_fuzzy_history() {
    let context = buffer_context("cod");
    let mut fuzzy = history_candidate(&context, "cargo doc", 3);
    fuzzy.score.cwd_affinity = 100;
    fuzzy.score.frecency = 200;
    fuzzy.score.transition = 200;
    let direct = Candidate::new(
        context.query_id,
        "codex",
        "PATH command",
        Some(TextEdit {
            range: 0..3,
            replacement: "codex".into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::PathCommand,
        CandidateKind::Command,
        Completeness::Runnable,
        RiskLevel::Unknown,
        "path:codex",
    );

    let ranked = rank_and_dedupe(&context, vec![fuzzy, direct], 10);
    assert_eq!(ranked[0].display.primary, "codex");
    assert!(ranked[0].score.match_priority > ranked[1].score.match_priority);
}

#[test]
fn direct_command_prefix_removes_fuzzy_rows_from_other_command_sources() {
    let context = buffer_context("cod");
    let command = |name: &str, provenance: &str| {
        Candidate::new(
            context.query_id,
            name,
            "command",
            Some(TextEdit {
                range: 0..3,
                replacement: name.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::PathCommand,
            CandidateKind::Command,
            Completeness::Runnable,
            RiskLevel::Unknown,
            provenance,
        )
    };

    let ranked = rank_and_dedupe(
        &context,
        vec![
            command("code", "path:code"),
            command("codex", "path:codex"),
            command("cargo-doc", "alias:cargo-doc"),
        ],
        10,
    );
    let names: Vec<_> = ranked
        .iter()
        .map(|candidate| candidate.display.primary.as_str())
        .collect();
    assert_eq!(names, ["code", "codex"]);
}

#[test]
fn exact_path_command_keeps_longer_executable_siblings() {
    let context = buffer_context("git");
    let command = |name: &str| {
        Candidate::new(
            context.query_id,
            name,
            "command",
            Some(TextEdit {
                range: 0..3,
                replacement: name.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::PathCommand,
            CandidateKind::Command,
            Completeness::Runnable,
            RiskLevel::Low,
            format!("path:{name}"),
        )
    };
    let help = Candidate::new(
        context.query_id,
        "git status",
        "subcommand",
        Some(TextEdit {
            range: 0..3,
            replacement: "git status".into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::InsertAndContinue {
            next_slot: crate::completion::SlotKind::Path,
        },
        CandidateSource::CommandHelp,
        CandidateKind::Command,
        Completeness::NeedsInput {
            slot: crate::completion::SlotKind::Path,
        },
        RiskLevel::Low,
        "help:git",
    );

    let ranked = rank_and_dedupe(
        &context,
        vec![command("git"), command("git-helper"), help],
        10,
    );
    assert_eq!(
        ranked
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        ["git status", "git-helper"]
    );
}

#[test]
fn command_rows_require_a_prefix_even_without_a_direct_sibling() {
    let context = buffer_context("cgd");
    let fuzzy = Candidate::new(
        context.query_id,
        "cargo-doc",
        "command",
        Some(TextEdit {
            range: 0..3,
            replacement: "cargo-doc".into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::PathCommand,
        CandidateKind::Command,
        Completeness::Runnable,
        RiskLevel::Unknown,
        "path:cargo-doc",
    );
    assert!(rank_and_dedupe(&context, vec![fuzzy], 10).is_empty());
}

#[test]
fn exact_explicit_executable_keeps_longer_prefixes_but_removes_fuzzy_rows() {
    let context = buffer_context("./run");
    let path = |name: &str| {
        Candidate::new(
            context.query_id,
            name,
            "filesystem",
            Some(TextEdit {
                range: context.parsed.replacement.clone(),
                replacement: name.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::Filesystem,
            CandidateKind::File,
            Completeness::Runnable,
            RiskLevel::Low,
            format!("filesystem:{name}"),
        )
    };

    let ranked = rank_and_dedupe(
        &context,
        vec![path("./run"), path("./runner"), path("./around")],
        10,
    );
    let rows: Vec<_> = ranked
        .iter()
        .map(|candidate| candidate.display.primary.as_str())
        .collect();
    assert_eq!(rows, ["./runner"]);
}

#[test]
fn exact_project_token_suppresses_substring_hosts() {
    let context = buffer_context("ssh dev");
    let host = |name: &str| {
        Candidate::new(
            context.query_id,
            name,
            "SSH host",
            Some(TextEdit {
                range: context.parsed.replacement.clone(),
                replacement: name.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::Project,
            CandidateKind::Command,
            Completeness::Runnable,
            RiskLevel::Low,
            format!("ssh:{name}"),
        )
    };

    let ranked = rank_and_dedupe(
        &context,
        vec![host("dev"), host("developer"), host("prod-dev")],
        10,
    );
    let rows: Vec<_> = ranked
        .iter()
        .map(|candidate| candidate.display.primary.as_str())
        .collect();
    assert_eq!(rows, ["developer"]);
}

#[test]
fn workspace_bonus_rewards_commands_matching_the_markers() {
    let git = WorkspaceMarkers {
        git: true,
        ..WorkspaceMarkers::default()
    };
    let node = WorkspaceMarkers {
        package_json: true,
        ..WorkspaceMarkers::default()
    };
    let rust = WorkspaceMarkers {
        cargo_toml: true,
        ..WorkspaceMarkers::default()
    };
    let make = WorkspaceMarkers {
        makefile: true,
        ..WorkspaceMarkers::default()
    };
    let just = WorkspaceMarkers {
        justfile: true,
        ..WorkspaceMarkers::default()
    };

    assert_eq!(workspace_bonus(git, "git status"), 40);
    assert_eq!(workspace_bonus(git, "cargo build"), 0);
    assert_eq!(workspace_bonus(git, "git"), 0, "bare command gets nothing");
    assert_eq!(workspace_bonus(node, "npm run build"), 40);
    assert_eq!(workspace_bonus(node, "pnpm test"), 40);
    assert_eq!(workspace_bonus(node, "yarn dev"), 40);
    assert_eq!(workspace_bonus(node, "bun start"), 40);
    assert_eq!(workspace_bonus(node, "npm install"), 0);
    assert_eq!(workspace_bonus(rust, "cargo build"), 40);
    assert_eq!(workspace_bonus(make, "make install"), 40);
    assert_eq!(workspace_bonus(just, "just build"), 40);
    assert_eq!(
        workspace_bonus(WorkspaceMarkers::default(), "git status"),
        0
    );
    assert!(
        workspace_bonus(
            WorkspaceMarkers {
                git: true,
                package_json: true,
                cargo_toml: true,
                makefile: true,
                justfile: true,
            },
            "git status"
        ) <= 100,
        "bonus stays clamped"
    );
}

#[test]
fn workspace_bonus_matches_display_primary_for_short_replacements() {
    let markers = WorkspaceMarkers {
        git: true,
        ..WorkspaceMarkers::default()
    };
    let context = buffer_context("st").with_workspace(markers);
    // Man-page style row: the edit inserts only the subcommand, but the
    // display shows the full command line.
    let row = Candidate::new(
        context.query_id,
        "git status",
        "man",
        Some(TextEdit {
            range: 0..2,
            replacement: "status".into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::CommandHelp,
        CandidateKind::Command,
        Completeness::Runnable,
        RiskLevel::ReadOnly,
        "man",
    );
    let ranked = rank_and_dedupe(&context, vec![row], 10);
    assert_eq!(
        ranked[0].score.context, 40,
        "git bonus must apply via display.primary"
    );
}
