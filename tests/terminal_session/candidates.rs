use super::*;

#[test]
fn man_derived_help_rows_appear_and_file_rows_follow_help_suppression() {
    if !command_exists("zsh") || !command_exists("man") {
        return;
    }
    let (home, work) = fixture_directories();
    fs::write(work.path().join("help-target.txt"), b"fixture").expect("work file");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(100));

    // `cp -`: `cp` has no spec coverage, so the man-derived flag list shows
    // HELP rows; the dashed active word suppresses FILE rows entirely.
    terminal.write(b"cp -");
    terminal.wait_for_screen(TAG_HELP);
    let text = terminal.screen_text();
    assert!(text.contains(TAG_HELP), "HELP rows missing:\n{text}");
    assert!(
        !text.contains(TAG_FILE),
        "FILE rows leaked into the flag position:\n{text}"
    );

    // `cp ` (no dash; the cp man page documents no subcommands): file
    // completion still works.
    terminal.write(b"\x03");
    terminal.wait_for_bare_row("HK>");
    terminal.settle(Duration::from_millis(100));
    terminal.write(b"cp ");
    terminal.wait_for_screen(TAG_FILE);
    let text = terminal.screen_text();
    assert!(
        text.contains("help-target.txt"),
        "file row missing after `cp `:\n{text}"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn pnpm_offers_package_json_scripts() {
    if !command_exists("zsh") || !command_exists("pnpm") {
        return;
    }
    let (home, work) = fixture_directories();
    fs::write(
        work.path().join("package.json"),
        r#"{"scripts":{"hkdev":"vite dev","hkbuild":"vite build"}}"#,
    )
    .expect("package.json");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // Bare `pnpm ` mixes package.json scripts into the list, using pnpm's
    // native direct-script form (no `run` keyword needed).
    terminal.write(b"pnpm hk");
    terminal.wait_for_screen("pnpm hkdev");
    let text = terminal.screen_text();
    assert!(text.contains("pnpm hkbuild"), "rows:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn aliases_from_zshrc_are_offered_at_the_command_position() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = fixture_directories();
    fs::write(
        home.path().join(".zshrc"),
        "PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\nalias hkll='ls -lah'\n",
    )
    .expect("zshrc with alias");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // The alias defined in .zshrc completes like a command, with its
    // expansion as the row description.
    terminal.write(b"hkl");
    terminal.wait_for_screen("hkll");
    let text = terminal.screen_text();
    assert!(text.contains("ls -lah"), "alias expansion missing:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn custom_function_argument_completes_the_inferred_slot() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = fixture_directories();
    fs::create_dir_all(home.path().join("projects/api")).expect("api dir");
    fs::create_dir_all(home.path().join("projects/web")).expect("web dir");
    fs::write(
        home.path().join(".zshrc"),
        "PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\nproj() {\n  if [ -n \"$1\" ]; then\n    cd \"$HOME/projects/$1\"\n  else\n    cd \"$HOME/projects\"\n  fi\n}\n",
    )
    .expect("zshrc with function");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // `proj ` completes directories under the base inferred from the
    // function body (`cd $HOME/projects/$1`).
    terminal.write(b"proj ");
    terminal.wait_for_screen("api");
    let text = terminal.screen_text();
    assert!(text.contains("web"), "rows:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn overlay_opens_without_a_default_selection() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"ls ");
    terminal.wait_for_screen("Tab 回填 · Enter 执行 · Esc 关闭");
    let text = terminal.screen_text();
    assert!(text.contains(TAG_SPEC), "overlay rows missing:\n{text}");
    assert!(!text.contains('▶'), "no row may be pre-selected:\n{text}");

    // Down selects the first row without ever touching the edit buffer.
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");
    let text = terminal.screen_text();
    assert!(text.contains("HK> ls"), "buffer changed:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn candidate_identical_to_the_typed_buffer_is_not_listed() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    terminal.write(b"echo HKIDENT_CMD\r");
    terminal.wait_for_bare_row("HKIDENT_CMD");
    terminal.settle(Duration::from_millis(300));

    // Retyping the exact seeded command: the history candidate would rewrite
    // the buffer to itself, so it is filtered out of the list.
    terminal.write(b"echo HKIDENT_CMD");
    terminal.settle(Duration::from_millis(500));
    let text = terminal.screen_text();
    assert!(
        text.contains("HK> echo HKIDENT_CMD"),
        "buffer missing:\n{text}"
    );
    assert!(
        !text.contains(TAG_HIS),
        "buffer-identical history candidate was listed:\n{text}"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn fresh_prompt_shows_no_overlay() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(500));
    assert_no_overlay_rows(&terminal);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn complete_executable_shows_recommendations_without_implicit_selection() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // A complete executable still opens the recommendation list immediately.
    terminal.write(b"ls");
    terminal.wait_for_screen(TAG_SPEC);
    terminal.settle(Duration::from_millis(200));
    let text = terminal.screen_text();
    assert!(
        text.contains(TAG_SPEC),
        "complete executable did not produce recommendation rows:\n{text}"
    );
    assert!(
        !text.contains('▶'),
        "recommendations must start unselected:\n{text}"
    );

    // Navigation is the explicit action that enters the list and selects one.
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn exact_executable_shows_dynamic_help_without_waiting_for_space() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = empty_fixture_directories();
    let bin = home.path().join("rc-bin");
    fs::create_dir(&bin).expect("rc bin");
    for (name, body) in [
        (
            "codex-fixture",
            "#!/bin/sh\nif [ \"${1-}\" = \"--help\" ]; then\n  printf '%s\\n' 'Usage: codex-fixture <COMMAND>' 'Commands:' '  resume    Resume a session' '  review    Review changes'\nfi\n",
        ),
        ("codex-fixture-helper", "#!/bin/sh\nexit 0\n"),
    ] {
        let executable = bin.join(name);
        fs::write(&executable, body).expect("fixture executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
                .expect("fixture mode");
        }
    }
    fs::write(
        home.path().join(".zshrc"),
        format!(
            "export PATH={}:$PATH\nPROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\n",
            bin.display()
        ),
    )
    .expect("fixture zshrc");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");

    // The exact runnable executable is still eligible for recommendations even
    // though a longer executable shares its prefix.
    terminal.write(b"codex-fixture");
    terminal.wait_for_screen("codex-fixture resume");
    let text = terminal.screen_text();
    assert!(
        text.contains("Resume a session"),
        "dynamic help row missing:\n{text}"
    );
    assert!(
        !text.contains('▶'),
        "dynamic-help rows must start unselected:\n{text}"
    );

    // Only navigation selects a row; it does not alter the typed buffer.
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");

    terminal.write(b"\x15");
    terminal.settle(Duration::from_millis(200));

    // Before an exact token, ordinary executable-name completion still works.
    terminal.write(b"codex-fixture-h");
    terminal.wait_for_screen("codex-fixture-helper");
    terminal.write(b"elper");
    terminal.settle(Duration::from_millis(500));
    assert_no_overlay_rows(&terminal);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn git_suggests_immediately_and_uses_repository_state() {
    if !command_exists("zsh") || !command_exists("git") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // `git` cannot run standalone, so suggestions appear without waiting for
    // a space — and outside a repository the top rows are init/clone, not
    // push/commit.
    terminal.write(b"git");
    terminal.wait_for_screen("git init");
    let text = terminal.screen_text();
    assert!(text.contains("git clone"), "clone row missing:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}
