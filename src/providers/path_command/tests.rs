use std::{ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf};

use super::*;
use crate::{
    completion::{BufferSnapshot, SyncQuality},
    shell::ShellKind,
    terminal::{BufferRevision, QueryId, RiskLevel},
};

fn context(text: &str) -> CompletionContext {
    context_for_shell(text, ShellKind::Zsh)
}

fn context_for_shell(text: &str, shell: ShellKind) -> CompletionContext {
    CompletionContext::new(
        QueryId::new(1),
        shell,
        PathBuf::from("/tmp"),
        BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
            .expect("buffer"),
    )
    .expect("context")
}

#[test]
fn keeps_every_matching_executable_past_one_thousand() {
    let directory = tempfile::tempdir().expect("bin");
    for index in 0..1_205 {
        let executable = directory.path().join(format!("hk-command-{index:04}"));
        fs::write(&executable, b"#!/bin/sh\n").expect("executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("mode");
    }
    let path = OsString::from(directory.path());
    let mut engine = crate::completion::CompletionEngine::new(0, 8);
    engine.register(PathCommandProvider::new(Arc::new(
        CommandPathCache::from_path(Some(&path)),
    )));
    let output = engine.complete(&context("hk-command-"));
    assert_eq!(output.candidates.len(), 1_205);
    assert_eq!(output.candidates[0].display.primary, "hk-command-0000");
    assert_eq!(output.candidates[1_204].display.primary, "hk-command-1204");
}

#[test]
fn fires_only_at_the_open_command_position() {
    let directory = tempfile::tempdir().expect("bin");
    for name in [
        "ls",
        "echo",
        "pnpm",
        "rm",
        "code",
        "codex",
        "corepack",
        "uv",
        "poetry",
        "pipenv",
        "bundle",
        "rustup",
        "find",
        "xargs",
        "setsid",
        "npm",
        "npx",
        "yarn",
        "bun",
        "cargo-doc",
        "claude",
        "claude tool",
    ] {
        let executable = directory.path().join(name);
        fs::write(&executable, b"#!/bin/sh\n").expect("fake executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("executable mode");
    }
    let path = OsString::from(directory.path());
    let provider = PathCommandProvider::new(Arc::new(CommandPathCache::from_path(Some(&path))));

    // On the (effective) command word — including right after wrappers
    // and assignments, where the command name is still being chosen.
    for buffer in [
        "",
        "l",
        "ls",
        "sudo ",
        "sudo l",
        "sudo -u root l",
        "sudo -nE l",
        "env -i l",
        "env -a custom l",
        "watch -n 1 l",
        "watch -dx l",
        "watch -q 2 l",
        "time l",
        "nocorrect l",
        "FOO=bar ",
        "which l",
        "command -v l",
        "command -pv l",
        "command -V ls ",
        "command c",
        "command type i",
        "command command c",
        "command exec c",
        "command builtin l",
        "builtin l",
        "builtin ",
        "builtin type i",
        "builtin command c",
        "builtin exec c",
        "type i",
        "pnpm exec l",
        "npm exec l",
        "npm exec -- l",
        "npm exec --package foo l",
        "npm exec --package=foo -- l",
        "npm exec --yes=false l",
        "npm exec --workspace foo l",
        "npm exec --workspace=foo l",
        "npm exec --ignore-scripts l",
        "npm exec --workspaces=false l",
        "npx --package foo l",
        "npx -w foo l",
        "corepack p",
        "find . -exec l",
        "find . -execdir l",
        "find . -ok l",
        "find . -okdir l",
        "find . -exec sudo -u root l",
        "find . -exec env -C app l",
        "find . -exec env FOO=bar l",
        "find . -exec time -o report.txt l",
        "xargs -0r l",
        "setsid -fw l",
        "uv run l",
        "uv run -- l",
        "uv --directory app run l",
        "uv run --python 3.12 l",
        "poetry run l",
        "poetry -C app run l",
        "pipenv run l",
        "bundle exec l",
        "rustup run stable l",
        "rustup run --install nightly l",
    ] {
        assert!(provider.applies(&context(buffer)), "{buffer:?} must fire");
    }
    // At an argument position PATH rows are a flood, not a completion.
    for buffer in [
        "ls ",
        "sudo ls ",
        "sudo -u root ls ",
        "FOO=bar ls ",
        "git checkout ",
        "pnpm install l",
        "pnpm exec ls ",
        "sudo builtin l",
        "time -p l",
        "pnpm dlx l",
        "yarn dlx l",
        "bun x l",
        "npx --call echo ",
        "npm exec --call echo ",
        "npm exec --package foo ls ",
        "sudo type c",
        "sudo command -v c",
        "exec type c",
        "find . -exec ls ",
        "find . -ok ls ",
        "find . -exec command ",
        "find . -exec builtin ",
        "find . -exec ! ",
        "find . -exec FOO=bar ",
        "sudo -nl l",
        r"find . -exec ls {} \; l",
        "uv run --module l",
        "uv run -m l",
        "uv run --script l",
        "uv run --python l",
        "uv run ls ",
        "poetry run ls ",
        "pipenv run ls ",
        "bundle exec ls ",
        "pipenv run -v l",
        "bundle exec -v l",
        "rustup run stable ls ",
        "rustup run --install nightly ls ",
    ] {
        assert!(
            !provider.applies(&context(buffer)),
            "{buffer:?} must not fire"
        );
    }

    for (shell, buffer) in [
        (ShellKind::Fish, "not cod"),
        (ShellKind::Fish, "and cod"),
        (ShellKind::Zsh, "nocorrect cod"),
        (ShellKind::Zsh, "whence cod"),
        (ShellKind::Zsh, "where cod"),
        (ShellKind::Bash, "time -p cod"),
    ] {
        assert!(
            provider.applies(&context_for_shell(buffer, shell)),
            "{buffer:?} must expose the nested command in {shell:?}"
        );
    }
    for (shell, buffer) in [
        (ShellKind::Bash, "not cod"),
        (ShellKind::Bash, "whence cod"),
        (ShellKind::Bash, "where cod"),
        (ShellKind::Zsh, "time -p cod"),
        (ShellKind::Fish, "sudo not cod"),
        (ShellKind::Zsh, "sudo nocorrect cod"),
    ] {
        assert!(
            !provider.applies(&context_for_shell(buffer, shell)),
            "{buffer:?} must remain an argument in {shell:?}"
        );
    }

    // Command rows still complete at the wrapper slot.
    let output = provider.complete(&context("sudo l"));
    assert!(
        output
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "ls")
    );
    let ls = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "ls")
        .expect("ls candidate");
    assert_eq!(ls.risk, RiskLevel::Medium);

    let output = provider.complete(&context("sudo -u root l"));
    let ls = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "ls")
        .expect("ls behind valued wrapper option");
    assert_eq!(ls.edit.as_ref().expect("edit").range, 13..14);

    let output = provider.complete(&context("corepack "));
    let names: Vec<_> = output
        .candidates
        .iter()
        .map(|candidate| candidate.display.primary.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "corepack npm",
            "corepack npx",
            "corepack pnpm",
            "corepack yarn"
        ]
    );

    let output = provider.complete(&context("command -v r"));
    let rm = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "command -v rm")
        .expect("command query candidate");
    assert_eq!(rm.edit.as_ref().expect("edit").replacement, "rm");
    assert_eq!(rm.risk, RiskLevel::Low);

    let builtins = provider.complete(&context("builtin l"));
    assert!(
        builtins
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "local")
    );
    assert!(
        builtins
            .candidates
            .iter()
            .all(|candidate| candidate.display.primary != "ls")
    );
    assert!(
        provider
            .complete(&context("builtin if"))
            .candidates
            .is_empty(),
        "reserved words are not shell builtins"
    );
    assert!(
        provider
            .complete(&context("compd"))
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "compdef"),
        "standard shell functions remain normal command candidates"
    );
    assert!(
        provider
            .complete(&context("builtin compd"))
            .candidates
            .iter()
            .all(|candidate| candidate.display.primary != "compdef"),
        "shell functions must not enter the builtin-only domain"
    );
    assert!(
        provider
            .complete(&context("builtin nog"))
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "noglob"),
        "zsh's callable noglob builtin was misclassified as a keyword"
    );

    let command = provider.complete(&context("command c"));
    assert!(
        command
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "cd")
    );
    assert!(
        command
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "codex")
    );
    assert!(
        provider
            .complete(&context("sudo c"))
            .candidates
            .iter()
            .all(|candidate| candidate.display.primary != "cd"),
        "external wrappers must not offer shell builtins"
    );

    let symbols = provider.complete(&context("type i"));
    assert!(
        symbols
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "type if")
    );
    assert!(
        provider
            .complete(&context("which c"))
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "which cd"),
        "zsh which must include shell builtins"
    );
    assert!(
        provider
            .complete(&context_for_shell("which c", ShellKind::Bash))
            .candidates
            .iter()
            .all(|candidate| candidate.display.primary != "which cd"),
        "external which in Bash must stay in the PATH domain"
    );

    for (prefix, expected) in [("cod", "codex"), ("clau", "claude")] {
        let output = provider.complete(&context(prefix));
        let candidate = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == expected)
            .unwrap_or_else(|| panic!("{expected} missing for {prefix:?}"));
        assert_eq!(candidate.edit.as_ref().expect("edit").replacement, expected);
    }
    let output = provider.complete(&context("clau"));
    let spaced = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "claude tool")
        .expect("spaced executable");
    assert_eq!(
        spaced.edit.as_ref().expect("edit").replacement,
        "'claude tool'"
    );
    let direct_names: Vec<_> = provider
        .complete(&context("cod"))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert_eq!(direct_names, ["code", "codex"]);

    let non_prefix_names: Vec<_> = provider
        .complete(&context("cgd"))
        .candidates
        .into_iter()
        .map(|candidate| candidate.display.primary)
        .collect();
    assert!(non_prefix_names.is_empty());

    let manager_only = PathCommandProvider::new(Arc::new(CommandPathCache::from_path(Some(
        &OsString::from(directory.path().join("missing")),
    ))));
    assert!(
        manager_only
            .complete(&context("pn"))
            .candidates
            .iter()
            .all(|candidate| candidate.display.primary != "pnpm"),
        "an unavailable manager must not be presented as an executable"
    );
    assert!(
        manager_only
            .complete(&context("corepack p"))
            .candidates
            .iter()
            .all(|candidate| !candidate.display.primary.starts_with("corepack ")),
        "an unavailable corepack cannot dispatch a virtual manager"
    );
    assert!(
        manager_only.complete(&context("pm")).candidates.is_empty(),
        "virtual managers must not enter broad fuzzy fallback"
    );
    assert!(
        !manager_only.applies(&context("uv run c")),
        "a missing outer executable must not open a nested PATH slot"
    );

    let output = provider.complete(&context("find . -exec clau"));
    let claude = output
        .candidates
        .iter()
        .find(|candidate| candidate.display.primary == "find . -exec claude")
        .expect("find executable candidate");
    assert_eq!(claude.edit.as_ref().expect("edit").replacement, "claude");

    let delegated = provider.complete(&context("uv run cod"));
    assert!(
        delegated
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "uv run codex")
    );
    assert!(
        provider
            .complete(&context("bundle exec clau"))
            .candidates
            .iter()
            .any(|candidate| candidate.display.primary == "bundle exec claude")
    );
}
