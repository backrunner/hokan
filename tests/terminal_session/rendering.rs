use super::*;

#[test]
fn literal_slash_arguments_clear_stale_directory_errors() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_bare_row("HK>");
    fs::create_dir_all(terminal._work.path().join("anomalyco/tap")).expect("collision directory");
    fs::write(
        terminal._work.path().join("anomalyco/tap/private-file"),
        b"fixture",
    )
    .expect("file");
    terminal.write(b"grep pattern hokan-missing-parent/file");
    terminal.wait_for_screen("找不到目录");
    for text in [
        "unmodelled-command anomalyco/tap/",
        "unmodelled-command ./anomalyco/tap/",
        "brew uninstall anomalyco/tap/",
        "npm exec @scope/pkg",
        "grep src/part",
        "sed s/old/new/",
        "awk /pattern/",
        "jq .items/2",
        "cat $HOKAN_UNKNOWN_DIRECTORY/file",
        "cat src/*/file",
        "open https://example.com/path",
    ] {
        terminal.write(b"\x15");
        terminal.write(text.as_bytes());
        terminal.settle(Duration::from_millis(350));
        let screen = terminal.screen_text();
        assert!(
            !screen.contains("找不到目录")
                && !screen.contains("修正目录")
                && !screen.contains("HK-FS-001")
                && !screen.contains("private-file"),
            "stale or speculative directory error for {text}:\n{screen}"
        );
    }
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn intermittent_repeated_preexec_does_not_leave_a_protocol_error_at_the_next_prompt() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = fixture_directories();
    fs::OpenOptions::new()
        .append(true)
        .open(home.path().join(".zshrc"))
        .expect("zshrc")
        .write_all(
            b"\nautoload -Uz add-zsh-hook\n\
              typeset -gi fixture_preexec_count=0\n\
              function fixture_duplicate_preexec() {\n\
                (( fixture_preexec_count += 1 ))\n\
                if (( fixture_preexec_count % 3 == 1 )); then\n\
                  __hokan_preexec \"$@\"\n\
                fi\n\
                return 0\n\
              }\n\
              add-zsh-hook preexec fixture_duplicate_preexec\n",
        )
        .expect("duplicate hook fixture");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_sync_replies(1);
    terminal.wait_for_bare_row("HK>");
    // Ordinary commands alternate success and failure. Only some executions
    // repeat the hook, matching an intermittent prompt/plugin interaction.
    for command in [
        "false",
        "printf 'HOOK_1\\n'",
        "true",
        "printf 'HOOK_2\\n'",
        "false",
        "printf 'HOOK_OK\\n'",
    ] {
        let start = terminal.transcript.len();
        terminal.write(format!("{command}\r").as_bytes());
        terminal.wait_for_bytes_since(start, b"HK> ");
        terminal.wait_for_bare_row("HK>");
        terminal.settle(Duration::from_millis(150));
        let screen = terminal.screen_text();
        assert!(!screen.contains("HK-SHL-013"), "{screen}");
        assert!(
            !screen.contains('╭'),
            "unexpected notice at empty prompt:\n{screen}"
        );
    }
    terminal.wait_for_bare_row("HOOK_OK");
    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn config_error_is_a_compact_notice_that_can_be_dismissed_and_recovers() {
    if !command_exists("zsh") {
        return;
    }
    for sync_status in [0, 2] {
        let mut terminal = TerminalSession::spawn_with_sync_status(sync_status);
        terminal.wait_for_bare_row("HK>");
        terminal.wait_for_cpr_replies(1);
        terminal.settle(Duration::from_millis(300));
        let config = terminal._home.path().join(".config/hokan/config.toml");
        fs::write(&config, "[ui]\nmax_rows = 'invalid'\n").expect("invalid config fixture");
        terminal.wait_for_screen("配置有误");
        terminal.wait_for_screen("Esc 关闭");
        let screen = terminal.screen_text();
        let top = terminal.screen_rows_containing("╭");
        let bottom = terminal.screen_rows_containing("╰");
        assert_eq!(top.len(), 1, "{screen}");
        assert_eq!(bottom.len(), 1, "{screen}");
        assert!(
            bottom[0] - top[0] <= 4,
            "notice left empty list rows:\n{screen}"
        );
        assert!(
            screen
                .lines()
                .nth(usize::from(top[0]))
                .expect("heading")
                .contains("提示")
        );
        assert!(
            screen
                .lines()
                .nth(usize::from(top[0]))
                .expect("heading")
                .contains("HK-CFG-RELOAD")
        );
        assert!(!screen.contains("Tab 回填"));
        assert!(!screen.contains("Enter 执行"));
        assert!(!screen.contains('▶'));
        terminal.write(b"\x1b");
        terminal.wait_for_screen_absent("Esc 关闭");
        fs::write(&config, "[ui]\nmax_rows = 8\n").expect("repaired config");
        terminal.settle(Duration::from_millis(500));
        terminal.wait_for_screen_absent("配置有误");
        terminal.write(b"ls ");
        terminal.wait_for_screen(TAG_SPEC);
        terminal.wait_for_screen("Tab 回填");
        terminal.exit_shell();
        terminal.wait_until_exit();
    }
}

#[test]
fn real_session_keeps_overlay_and_terminal_lifecycle_stable() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.wait_for_sync_replies(1);
    terminal.settle(Duration::from_millis(300));
    let initial_disable = terminal
        .transcript
        .windows(DISABLE_BRACKETED_PASTE.len())
        .position(|window| window == DISABLE_BRACKETED_PASTE)
        .expect("Hokan should normalize inherited bracketed-paste mode");
    let child_enable = terminal
        .transcript
        .windows(ENABLE_BRACKETED_PASTE.len())
        .position(|window| window == ENABLE_BRACKETED_PASTE)
        .expect("zsh should enable bracketed paste for its line editor");
    assert!(
        initial_disable < child_enable,
        "inherited mode must be cleared before child output is forwarded"
    );

    terminal.write(b"ls ");
    // Wait for the hint footer (the bottom edge of the box) rather than the
    // first SPEC label: early frames show item rows before the full box —
    // including its bottom edge — has been painted.
    terminal.wait_for_screen("Tab 回填 · Enter 执行 · Esc 关闭");
    terminal.wait_for_screen("HK> ls");
    let text = terminal.screen_text();
    assert!(text.contains('╭'), "overlay top border missing:\n{text}");
    assert!(
        text.contains(TAG_SPEC),
        "overlay spec rows missing:\n{text}"
    );

    terminal.write(b"\x1b[B\x1b[A\x1b[6~\x1b[5~");
    terminal.settle(Duration::from_millis(100));
    assert!(terminal.screen_text().contains("HK> ls"));
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.write(b"\x0c");
    terminal.wait_for_screen("HK> ls");
    let before_resize = terminal.transcript.len();
    terminal.resize(30, 100);
    terminal.wait_for_bytes_since(before_resize, b"HK> ls");
    terminal.wait_for_screen(TAG_SPEC);

    let pid = terminal.pid();
    let suspend_start = terminal.transcript.len();
    signal::kill(Pid::from_raw(pid), Signal::SIGTSTP).expect("suspend hokan");
    terminal.wait_for_bytes_since(suspend_start, DISABLE_BRACKETED_PASTE);
    wait_until_stopped(pid);
    let continue_start = terminal.transcript.len();
    signal::kill(Pid::from_raw(pid), Signal::SIGCONT).expect("continue hokan");
    terminal.wait_for_bytes_since(continue_start, ENABLE_BRACKETED_PASTE);
    terminal.wait_for_screen("HK> ls");
    terminal.wait_for_cpr_replies(1);

    terminal.write(b"\x03");
    terminal.settle(Duration::from_millis(100));
    terminal.write("printf '中🙂'".as_bytes());
    terminal.settle(Duration::from_millis(100));
    assert!(terminal.screen_text().contains("中🙂"));
    terminal.write(b"\x03");
    terminal.settle(Duration::from_millis(100));

    let fixture = terminal._work.path().join("alternate.sh");
    write_alternate_fixture(&fixture);
    // Stop one character short of the full path: a candidate that would
    // rewrite the buffer to itself is filtered out, so the FILE row only
    // appears while the typed text is still a proper prefix.
    terminal.write(b"sh ./alternate.s");
    terminal.wait_for_screen(TAG_FILE);
    terminal.wait_for_clean_overlay("HK> sh ./alternate.s");
    terminal.write(b"\x1b");
    // Wait for the standalone Escape to be consumed, even on a busy runner.
    // A fixed sleep can merge it with `h` into zsh's Alt-h/run-help binding.
    terminal.wait_for_screen_absent("Esc 关闭");
    terminal.write(b"h");
    terminal.wait_for_screen("HK> sh ./alternate.sh");
    let alternate_start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(alternate_start, b"ALT_READY");
    terminal.write(b"ok\r");
    terminal.wait_for_bytes_since(alternate_start, b"ALT_KEY=ok");
    terminal.wait_for_bytes_since(alternate_start, b"\x1b[?1049l");
    let alternate_end = terminal.transcript.len();
    let alternate_output = &terminal.transcript[alternate_start..alternate_end];
    assert!(
        !alternate_output
            .windows(TAG_SPEC.len())
            .any(|window| window == TAG_SPEC.as_bytes())
    );
    assert!(
        !alternate_output
            .windows("│".len())
            .any(|window| window == "│".as_bytes())
    );
    terminal.wait_for_screen("HK> ");

    terminal.exit_shell();
    terminal.wait_until_exit();
    assert!(
        terminal.transcript.ends_with(RESTORE_PRESENTATION),
        "terminal presentation was not restored; tail={:?}",
        tail(&terminal.transcript, 256)
    );
}

#[test]
fn real_session_uses_non_destructive_fallback_when_mode_2026_is_unavailable() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_sync_status(0);
    terminal.wait_for_screen("HK> ");
    terminal.wait_for_sync_replies(1);
    terminal.settle(Duration::from_millis(300));
    assert_eq!(terminal.sync_replies, 1);

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    terminal.write(b"\x1b[B\x1b[A");
    terminal.settle(Duration::from_millis(100));
    assert!(terminal.screen_text().contains("HK> ls"));
    assert!(
        !terminal
            .transcript
            .windows(b"\x1b[?2026h".len())
            .any(|window| window == b"\x1b[?2026h"),
        "fallback session unexpectedly began a synchronized update"
    );
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn directory_errors_offer_compact_parent_recovery_and_recover() {
    use std::os::unix::fs::PermissionsExt;

    if !command_exists("zsh") {
        return;
    }
    struct RestorePermissions(PathBuf);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }

    for sync_status in [0, 2] {
        let mut terminal = TerminalSession::spawn_with_sync_status(sync_status);
        let blocked = terminal._work.path().join("blocked");
        fs::create_dir(&blocked).expect("blocked directory");
        fs::create_dir(terminal._work.path().join("readable")).expect("readable directory");
        fs::write(terminal._work.path().join("file"), b"file").expect("file");
        let _restore = RestorePermissions(blocked.clone());
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).expect("deny access");
        terminal.wait_for_screen("HK> ");
        terminal.write(b"cd ./");
        terminal.wait_for_overlay_candidate("readable/");

        let mut failures = vec![("missing", "找不到目录"), ("file", "不是目录")];
        if !nix::unistd::geteuid().is_root() {
            failures.push(("blocked", "无权访问目录"));
        }
        for (path, cause) in failures {
            terminal.write(format!("\x15cd ./{path}/").as_bytes());
            terminal.wait_for_screen(cause);
            terminal.wait_for_overlay_candidate("返回可用目录");
            terminal.wait_for_clean_overlay(&format!("HK> cd ./{path}/"));
            let text = terminal.screen_text();
            let top = terminal.screen_rows_containing("╭")[0];
            let bottom = terminal.screen_rows_containing("╰")[0];
            assert!(
                (3..=5).contains(&(bottom - top + 1)),
                "notice is too tall:\n{text}"
            );
            assert!(text.contains("Esc 关闭"), "{text}");
            assert!(text.contains("Tab 修正"), "{text}");
            for row in top + 1..bottom {
                assert!(
                    !terminal
                        .screen_line(row)
                        .trim_matches([' ', '│'])
                        .is_empty(),
                    "empty notice row:\n{text}"
                );
            }
            for absent in ["▶", "readable/", "HK-FS-001"] {
                assert!(
                    !text.contains(absent),
                    "stale candidate UI {absent}:\n{text}"
                );
            }

            // Closing a notice must preserve the command being edited.
            terminal.write(b"\x1b");
            terminal.wait_for_screen_absent("╭");
            assert!(
                terminal
                    .screen_text()
                    .contains(&format!("HK> cd ./{path}/"))
            );
            // Editing the path brings normal completion back at its full height.
            terminal.write(b"\x15cd ./");
            terminal.wait_for_overlay_candidate("readable/");
            let top = terminal.screen_rows_containing("╭")[0];
            let bottom = terminal.screen_rows_containing("╰")[0];
            assert_eq!(bottom - top + 1, 8);
            assert!(!terminal.screen_text().contains(cause));
        }
        terminal.exit_shell();
        terminal.wait_until_exit();
    }
}

#[test]
fn directory_spelling_repairs_fill_without_executing_or_selecting_a_child() {
    if !command_exists("zsh") {
        return;
    }
    for key in [b"\t".as_slice(), b"\r".as_slice()] {
        let mut terminal = TerminalSession::spawn();
        fs::create_dir_all(terminal._work.path().join("projects/child")).expect("directory tree");
        let original_name = terminal
            ._work
            .path()
            .file_name()
            .expect("basename")
            .to_string_lossy()
            .into_owned();
        terminal.wait_for_screen("HK> ");
        terminal.write(b"cd projcets/");
        // `projects/` also appears while the shell reports an intermediate
        // prefix such as `cd pro`. Wait for the final path-repair result,
        // not just the candidate name or the shell's already-updated line.
        terminal.wait_for_screen("找不到目录");
        terminal.wait_for_screen("Tab 修正");
        terminal.wait_for_overlay_candidate("projects/");
        terminal.wait_for_clean_overlay("HK> cd projcets/");
        let before = terminal.screen_text();
        assert!(before.contains("找不到目录"), "{before}");
        assert!(before.contains("Tab 修正"), "{before}");
        assert!(
            !before.contains('▶'),
            "repair must not be preselected: {before}"
        );
        let top = terminal.screen_rows_containing("╭")[0];
        let bottom = terminal.screen_rows_containing("╰")[0];
        assert_eq!(
            bottom - top + 1,
            4,
            "two recovery rows should be compact: {before}"
        );
        terminal.write(b"\x1b[B");
        terminal.wait_for_screen("▶");
        terminal.write(key);
        terminal.wait_for_overlay_candidate("child/");
        terminal.wait_for_clean_overlay("HK> cd projects/");
        assert!(
            !terminal.screen_text().contains('▶'),
            "a repair must not select the next child"
        );
        terminal.write(b"\x15printf '%s\\n' $PWD:t\r");
        terminal.wait_for_bare_row(&original_name);
        terminal.exit_shell();
        terminal.wait_until_exit();
    }
}

#[test]
fn wrapping_edit_line_survives_scroll_to_make_room_and_box_moves() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // Seed a long history command: typing a proper prefix of it keeps a HIS
    // candidate (and therefore the overlay) open for the whole scenario.
    let seed =
        "echo HKWRAP_seed_aaaaaaaaaabbbbbbbbbbccccccccccddddddddddeeeeeeeeeeffffffffffgggggggggg";
    terminal.write(seed.as_bytes());
    terminal.write(b"\r");
    terminal.wait_for_screen("HKWRAP_seed_aaaaaaaaaa");
    terminal.settle(Duration::from_millis(300));

    // Fill the screen so the fresh prompt sits on the terminal's last row.
    terminal.write(b"seq 1 40\r");
    terminal.wait_for_bare_row("40");
    terminal.settle(Duration::from_millis(300));

    // Type a proper prefix long enough to wrap the 80-column edit line. The
    // overlay makes room by scrolling while the shell cursor is mid-screen,
    // and the box's left edge follows the cursor across the wrap.
    let typed = &seed[..78];
    assert!(typed.len() + "HK> ".len() > terminal.cols as usize);
    for chunk in typed.as_bytes().chunks(6) {
        terminal.write(chunk);
        terminal.settle(Duration::from_millis(25));
    }

    // (a) the edit-line start must survive the mid-screen scroll, and (b) no
    // stale border glyphs may remain outside the current overlay box.
    terminal.wait_for_clean_overlay("HK> echo HKWRAP_seed_");
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

/// Regression test: child output that scrolls the screen while the overlay
/// is painted must not carry painted cells into the terminal's scrollback,
/// where they could never be erased. Before the fix, a background job
/// printing a screenful of text scrolled the whole box (border glyphs plus
/// their panel background) into scrollback and it stayed there forever.
#[test]
fn background_output_cannot_scroll_painted_overlay_into_scrollback() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_colored_with_scrollback(200);
    terminal.wait_for_screen("HK> ");
    terminal.wait_for_sync_replies(1);
    // Seed history so typing `seq` offers candidates and paints the box.
    terminal.write(b"seq 1 5\r");
    terminal.wait_for_screen("HK> seq 1 5");
    terminal.wait_for_screen("HK> ");

    // A background job that prints a full screen one second from now.
    terminal.write(b"{ sleep 1; seq 100 140 } &\r");
    terminal.wait_for_screen("HK> ");

    // Open the overlay before the job fires.
    terminal.write(b"seq");
    terminal.wait_for_clean_overlay("seq");
    assert!(
        !terminal.painted_screen_cells().is_empty(),
        "colored overlay should be painted before the scroll"
    );

    // The job output scrolls the screen while the box is (or was) painted.
    terminal.settle(Duration::from_millis(2500));

    // Whatever repaint happened, dismissal must leave nothing behind —
    // neither on the live screen nor in scrollback.
    terminal.write(b"\x1b");
    terminal.settle(Duration::from_millis(500));

    let text = terminal.screen_text();
    for glyph in ['╭', '╮', '╰', '╯', '│', '─'] {
        assert!(
            !text.contains(glyph),
            "overlay glyph {glyph:?} left on screen:\n{text}"
        );
    }
    let painted = terminal.painted_screen_cells();
    assert!(
        painted.is_empty(),
        "painted cells left on screen: {painted:?}"
    );
    let scrollback = terminal.painted_scrollback_cells();
    assert!(
        scrollback.is_empty(),
        "painted cells scrolled into scrollback: {} cells, first={:?}",
        scrollback.len(),
        scrollback.first()
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

/// A partial scroll is the same hazard with less margin: the overlay's top
/// rows are pushed into scrollback while the rest stays on screen.
#[test]
fn small_background_output_leaves_no_overlay_residue() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_colored_with_scrollback(200);
    terminal.wait_for_screen("HK> ");
    terminal.wait_for_sync_replies(1);
    // Fill the screen so the next prompt — and the overlay under it — sits
    // at the bottom, where even a small burst scrolls the box's top rows
    // into scrollback.
    terminal.write(b"seq 1 20\r");
    terminal.wait_for_screen("HK> seq 1 20");
    terminal.wait_for_screen("HK> ");

    terminal.write(b"{ sleep 1; seq 200 225 } &\r");
    terminal.wait_for_screen("HK> ");
    terminal.write(b"seq");
    terminal.wait_for_clean_overlay("seq");

    terminal.settle(Duration::from_millis(2500));
    terminal.write(b"\x1b");
    terminal.settle(Duration::from_millis(500));

    let painted = terminal.painted_screen_cells();
    assert!(
        painted.is_empty(),
        "painted cells left on screen: {painted:?}"
    );
    let scrollback = terminal.painted_scrollback_cells();
    assert!(
        scrollback.is_empty(),
        "painted cells in scrollback: {scrollback:?}"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}
