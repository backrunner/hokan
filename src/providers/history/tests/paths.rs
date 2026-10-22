use super::*;

#[test]
fn deleted_single_path_commands_disappear_and_return_when_recreated() {
    let root = tempfile::tempdir().expect("root");
    let target = root.path().join("old project");
    fs::create_dir(&target).expect("target");
    let commands = [
        "rm -rf 'old project'",
        "rm --recursive --force -- 'old project'",
        "rmdir -- 'old project'",
        "cd 'old project'",
        "pushd 'old project'",
        "ls -lah 'old project'",
        "ls --color=auto -- 'old project'",
        "du -sh old\\ project",
        "tree -a 'old project'",
        "find -L 'old project'",
        "stat 'old project'",
        "file 'old project'",
        "open -R 'old project'",
        "xdg-open 'old project'",
        "sudo -u root rm -rf 'old project'",
        "command rm -rf 'old project'",
        "env -C . rm -rf 'old project'",
    ];
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for command in commands {
        index.ingest(
            command,
            1_000,
            ShellKind::Zsh,
            Some(root.path()),
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_executables(
        index,
        &[
            "rm", "rmdir", "ls", "du", "tree", "find", "stat", "file", "open", "xdg-open",
        ],
    );
    let check = |expected| {
        for mode in [
            CompletionMode::Normal,
            CompletionMode::HistoryOnly,
            CompletionMode::HistoryNavigation,
        ] {
            for command in commands {
                let rows = provider
                    .complete(&context_in(root.path(), command, mode))
                    .candidates;
                assert_eq!(
                    rows.iter().any(|row| row.display.primary == command),
                    expected,
                    "{mode:?}: {command:?}"
                );
            }
        }
    };
    check(true);
    assert!(
        std::process::Command::new("/bin/rm")
            .arg("-rf")
            .arg(&target)
            .status()
            .expect("remove fixture")
            .success()
    );
    assert!(!target.exists());
    check(false);
    fs::create_dir(&target).expect("recreate target");
    check(true);
}

#[test]
fn stale_paths_are_filtered_before_the_candidate_limit() {
    let root = tempfile::tempdir().expect("root");
    fs::create_dir(root.path().join("keep")).expect("valid target");
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    index.ingest(
        "rm -rf keep",
        1,
        ShellKind::Zsh,
        Some(root.path()),
        Some(0),
        &policy,
    );
    for number in 0..75 {
        index.ingest_weighted(
            &format!("rm -rf gone-{number}"),
            10_000 + number,
            ShellKind::Zsh,
            Some(root.path()),
            50,
            Some(0),
            &policy,
        );
    }
    let provider = provider_with_executables(index, &["rm"]).with_navigation_limit(1);
    for mode in [
        CompletionMode::Normal,
        CompletionMode::HistoryOnly,
        CompletionMode::HistoryNavigation,
    ] {
        let rows = provider
            .complete(&context_in(root.path(), "rm ", mode))
            .candidates;
        assert_eq!(rows.len(), 1, "{mode:?}");
        assert_eq!(rows[0].display.primary, "rm -rf keep");
    }
}

#[test]
fn single_path_checks_use_current_cwd_and_wrapper_cwd() {
    let root = tempfile::tempdir().expect("root");
    fs::create_dir(root.path().join("app")).expect("app");
    fs::create_dir(root.path().join("app/keep")).expect("target");
    let provider = provider_with_executables(HistoryIndex::default(), &["rm"]);
    for shell in [ShellKind::Bash, ShellKind::Zsh, ShellKind::Fish] {
        let mut ctx = context_for_shell("rm ", shell);
        ctx.cwd = Arc::new(root.path().to_owned());
        for (command, expected) in [
            ("rm -rf keep".to_owned(), false),
            ("rm -rf ./app/keep".to_owned(), true),
            ("rm -rf ./app/missing".to_owned(), false),
            ("env -C app rm -rf keep".to_owned(), true),
            ("env --chdir=app rm -rf missing".to_owned(), false),
            ("sudo -D app rm -rf keep".to_owned(), true),
            ("sudo -D app rm -rf missing".to_owned(), false),
            (
                format!("/bin/rm -rf {}/app/keep", root.path().display()),
                true,
            ),
            (
                format!("/bin/rm -rf {}/app/gone", root.path().display()),
                false,
            ),
        ] {
            assert_eq!(
                provider.plausible_command(&ctx, &command),
                expected,
                "{shell:?}: {command}"
            );
        }
    }
}

#[test]
fn new_paths_and_uncertain_path_semantics_are_kept() {
    let root = tempfile::tempdir().expect("root");
    let provider = provider_with_executables(
        HistoryIndex::default(),
        &[
            "rm", "mkdir", "touch", "cp", "mv", "ln", "git", "du", "ls", "tree", "find", "open",
            "realpath", "readlink",
        ],
    );
    for command in [
        "mkdir -p missing",
        "touch missing",
        "cp existing missing",
        "mv existing missing",
        "ln -s missing link",
        "git clone https://example.com/repo missing",
        "realpath -m missing",
        "readlink -m missing",
        "readlink -f missing",
        "rm -rf one two",
        "rm -rf $TARGET",
        "rm -rf ${TARGET}",
        "rm -rf =ls",
        "rm -rf ~/cache/*",
        "rm -rf $(get_target)",
        "rm -rf ~another_user/cache",
        "rm -rf 'unterminated",
        "rm -rf missing > missing",
        "mkdir -p missing && rm -rf missing",
        "rm -rf missing || mkdir missing",
        "env -C \"$PROJECT\" rm -rf missing",
        "env -C '~/project' rm -rf missing",
        "du -d 2 missing",
        "tree -L 2 missing",
        "tree -H https://example.com",
        "ls -I pattern missing",
        "find missing -name '*.rs'",
        "rm --help missing",
        "rm --unknown missing",
        "open https://example.com",
        "cd -",
        "pushd +1",
        "pushd -1",
        "rm",
        "ls -la",
    ] {
        let ctx = context_in(root.path(), command, CompletionMode::Normal);
        assert!(
            provider.plausible_command(&ctx, command),
            "incorrectly filtered {command:?}"
        );
    }
}

#[test]
fn literal_paths_handle_dash_names_unicode_tildes_and_dangling_symlinks() {
    let root = tempfile::tempdir().expect("root");
    fs::create_dir_all(root.path().join("~/keep")).expect("literal tilde");
    fs::create_dir(root.path().join("-cache")).expect("dash path");
    fs::create_dir(root.path().join("旧 缓存")).expect("unicode path");
    fs::write(root.path().join("file"), b"fixture").expect("file");
    std::os::unix::fs::symlink("missing", root.path().join("link")).expect("dangling symlink");
    let provider = provider_with_executables(HistoryIndex::default(), &["rm", "readlink"]);
    for (command, expected) in [
        ("rm -rf '~/keep'", true),
        ("rm -rf '~/gone'", false),
        ("rm -rf \\~/keep", true),
        ("rm -rf -- -cache", true),
        ("rm -rf -- -missing", false),
        ("rm -rf '旧 缓存'", true),
        ("rm -rf '已删除 缓存'", false),
        ("rm -rf file/child", false),
        ("rm -rf file", true),
        ("rm -rf link", true),
        ("readlink link", true),
        ("readlink gone", false),
        ("rm -rf ~", true),
    ] {
        assert_eq!(
            provider.plausible_command(
                &context_in(root.path(), command, CompletionMode::Normal),
                command
            ),
            expected,
            "{command}"
        );
    }
}

#[test]
fn shell_overrides_are_not_assumed_to_use_builtin_path_semantics() {
    let root = tempfile::tempdir().expect("root");
    let mut aliases = crate::shell::ShellAliases::default();
    crate::shell::parse_rc_text(
        ShellKind::Zsh,
        "alias rm='custom-remove'\nls() { echo \"$1\"; }\n",
        &mut aliases,
    );
    let provider = provider_with_executables(HistoryIndex::default(), &["rm", "ls"]);
    for (command, expected) in [
        ("rm missing", true),
        ("ls missing", true),
        ("command rm missing", false),
        ("sudo rm missing", false),
    ] {
        assert_eq!(
            provider.plausible_command_with_aliases(
                &context_in(root.path(), command, CompletionMode::Normal),
                command,
                &aliases
            ),
            expected,
            "{command}"
        );
    }
}
