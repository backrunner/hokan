use super::*;

#[test]
fn imported_history_subcommand_typos_are_filtered_by_dynamic_help() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = empty_fixture_directories();
    let bin = home.path().join("rc-bin");
    fs::create_dir(&bin).expect("rc bin");
    let executable = bin.join("history-fixture");
    fs::write(
        &executable,
        "#!/bin/sh\nif [ \"${1-}\" = \"--help\" ]; then\n  printf '%s\\n' 'Usage: history-fixture <COMMAND>' 'Commands:' '  resume    Resume a session' '  review    Review changes'\nfi\n",
    )
    .expect("fixture executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("fixture mode");
    }
    fs::write(
        home.path().join(".zshrc"),
        format!(
            "export PATH={}:$PATH\nPROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\n",
            bin.display()
        ),
    )
    .expect("fixture zshrc");
    fs::write(
        home.path().join(".zsh_history"),
        "history-fixture resume\nhistory-fixture resumx\nhistory-fixture upgrad\n",
    )
    .expect("fixture history");

    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.write(b"history-fixture ");
    terminal.wait_for_screen("history-fixture resume");
    let text = terminal.screen_text();
    assert!(!text.contains("history-fixture resumx"), "{text}");
    assert!(!text.contains("history-fixture upgrad"), "{text}");

    terminal.write(b"\x15");
    terminal.settle(Duration::from_millis(200));
    terminal.write(b"history-fixture resu");
    terminal.wait_for_screen("history-fixture resume");
    let text = terminal.screen_text();
    assert!(!text.contains("history-fixture resumx"), "{text}");

    terminal.write(b"\x15");
    terminal.settle(Duration::from_millis(200));
    terminal.write(b"history-fixture upg");
    terminal.settle(Duration::from_millis(500));
    assert_no_overlay_rows(&terminal);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn argument_position_excludes_unrelated_history_commands() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    fs::write(terminal._work.path().join("HKARG_MARKER.txt"), b"marker\n").expect("marker file");
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // Seed an unrelated history line that matches `whoami ` only as a fuzzy
    // subsequence (w…h…o…a…m…i…space). It must never surface as an argument
    // of `whoami`.
    terminal.write(b"echo who am i HKARG_JUNK\r");
    terminal.wait_for_bare_row("who am i HKARG_JUNK");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"whoami ./HKARG_");
    // Explicit path intent produces the cwd marker file without reopening
    // broad fuzzy history at this argument position.
    terminal.wait_for_screen("HKARG_MARKER.txt");
    terminal.settle(Duration::from_millis(300));
    // The first row can trigger a background `whoami` help refresh. Wait for
    // the post-refresh repaint as well instead of sampling its paint window.
    terminal.wait_for_screen("HKARG_MARKER.txt");

    let text = terminal.screen_text();
    let overlay_rows: Vec<_> = text.lines().filter(|line| line.contains('│')).collect();
    assert!(
        !overlay_rows.is_empty(),
        "expected overlay rows after an explicit whoami path:\n{text}"
    );
    assert!(
        overlay_rows.iter().all(|line| !line.contains("HKARG_JUNK")),
        "unrelated history command leaked into the argument list:\n{text}"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn deleting_to_an_empty_buffer_hides_the_overlay() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    terminal.write(b"\x7f\x7f\x7f");
    terminal.settle(Duration::from_millis(500));
    assert_no_overlay_rows(&terminal);
    let text = terminal.screen_text();
    assert!(
        text.contains("HK> "),
        "prompt must still be present:\n{text}"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn rapid_typing_and_backspacing_still_surfaces_the_overlay() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // Seed two history rows so every burst below has a prefix-matchable
    // candidate at its final buffer.
    terminal.write(b"echo HKBURST_alpha\r");
    terminal.wait_for_bare_row("HKBURST_alpha");
    terminal.write(b"echo HKBURST_beta\r");
    terminal.wait_for_bare_row("HKBURST_beta");
    terminal.settle(Duration::from_millis(300));

    // Bursts that mix fast typing and backspacing arrive as batched pty
    // input: buffer events, gates, queries and provider results race. Once
    // the burst ends on a completable buffer the overlay must reliably come
    // back — a lost gate or a swallowed repaint leaves it invisible.
    for round in 0..6 {
        // One write: type "ls -la", erase it entirely, retype "ls ".
        terminal.write(b"ls -la\x7f\x7f\x7f\x7f\x7f\x7fls ");
        terminal.wait_for_clean_overlay("HK> ls");
        let text = terminal.screen_text();
        assert!(
            text.contains("HK> ls"),
            "round {round}: edit line lost:\n{text}"
        );
        terminal.write(b"\x15");
        terminal.settle(Duration::from_millis(200));

        // Per-byte writes with no pacing: fast manual typing with a typo and
        // a backspace correction; lands on a history prefix.
        for byte in b"echo HKBURST_alx\x7fp" {
            terminal.write(std::slice::from_ref(byte));
        }
        terminal.wait_for_overlay_candidate("echo HKBURST_alpha");
        terminal.write(b"\x15");
        terminal.settle(Duration::from_millis(200));

        // Empty the buffer mid-burst, then retype in one write.
        let prefix = b"echo HKB";
        let mut burst = prefix.to_vec();
        burst.extend(std::iter::repeat_n(0x7f, prefix.len()));
        burst.extend_from_slice(b"echo HKBURST_b");
        terminal.write(&burst);
        terminal.wait_for_overlay_candidate("echo HKBURST_beta");
        terminal.write(b"\x15");
        terminal.settle(Duration::from_millis(200));

        // End the burst ON backspaces: overshoot the target, erase back to a
        // completable prefix.
        terminal.write(b"echo HKBURST_beta\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f");
        terminal.wait_for_overlay_candidate("echo HKBURST_beta");
        terminal.write(b"\x15");
        terminal.settle(Duration::from_millis(200));
    }

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn dismissed_overlay_stays_closed_across_shell_redisplays() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = fixture_directories();
    fs::write(
        home.path().join(".zshrc"),
        "PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\nbindkey -e\n\
         function fixture_redraw() { zle redisplay }\n\
         zle -N fixture_redraw\nbindkey '^X' fixture_redraw\n",
    )
    .expect("redraw fixture");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.write(b"ls ");
    terminal.wait_for_clean_overlay("HK> ls");

    for close in [b"\x1b".as_slice(), b"\x1b[Z".as_slice()] {
        terminal.write(b"\x1b[B");
        terminal.settle(Duration::from_millis(50));
        terminal.write(close);
        terminal.settle(Duration::from_millis(100));
        terminal.write(b"\x18");
        terminal.settle(Duration::from_millis(300));
        assert_no_overlay_rows(&terminal);
        assert!(terminal.screen_text().contains("HK> ls"));
        terminal.write(b"\x1b[Z");
        terminal.wait_for_clean_overlay("HK> ls");
    }
    terminal.write(b"\x1b");
    terminal.settle(Duration::from_millis(100));
    terminal.write(b"-");
    terminal.wait_for_clean_overlay("HK> ls -");
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn shift_tab_on_an_empty_buffer_does_not_open_the_overlay() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"\x1b[Z");
    terminal.settle(Duration::from_millis(500));
    assert_no_overlay_rows(&terminal);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn ctrl_r_on_an_empty_buffer_opens_the_history_view() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"echo HK_HIST_SEED\r");
    terminal.wait_for_screen("HK_HIST_SEED");
    terminal.settle(Duration::from_millis(300));
    // Explicit user intent: Ctrl-R focuses history even on an empty buffer.
    terminal.write(b"\x12");
    terminal.wait_for_screen(TAG_HIS);
    assert!(terminal.screen_text().contains("echo HK_HIST_SEED"));

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn command_list_keeps_over_one_thousand_rows_and_wraps_across_pages() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = empty_fixture_directories();
    let bin = home.path().join("rc-bin");
    fs::create_dir(&bin).expect("bin");
    for index in 0..1_005 {
        let executable = bin.join(format!("hk-list-{index:04}"));
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("executable");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("mode");
    }
    fs::write(
        home.path().join(".zshrc"),
        format!(
            "export PATH={}:$PATH\nPROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\n",
            bin.display()
        ),
    )
    .expect("zshrc");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.wait_for_sync_replies(1);
    terminal.write(b"hk-list-");
    terminal.wait_for_screen("hk-list-0000");
    terminal.wait_for_screen("1/1005");

    // PageUp from no selection opens the last page (six visible rows).
    terminal.write(b"\x1b[5~");
    terminal.wait_for_selected_history("hk-list-1002");
    terminal.write(&b"\x1b[B".repeat(2));
    terminal.wait_for_selected_history("hk-list-1004");
    terminal.write(b"\x1b[B");
    terminal.wait_for_selected_history("hk-list-0000");
    terminal.write(b"\x1b[A");
    terminal.wait_for_selected_history("hk-list-1004");
    terminal.write(b"\x1b[6~");
    terminal.wait_for_selected_history("hk-list-0005");
    terminal.write(b"\x1b[5~");
    terminal.wait_for_selected_history("hk-list-1004");
    terminal.write(b"\t");
    terminal.wait_for_bare_row("HK> hk-list-1004");
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn history_arrows_scroll_past_fifty_entries_and_fill_the_selected_command() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = fixture_directories();
    let store = hokan::history::HistoryStore::open(&home.path().join(".local/state/hokan"))
        .expect("history store");
    let cwd = work.path().canonicalize().expect("canonical cwd");
    let events: Vec<_> = (0..80)
        .map(|index| hokan::history::HistoryEventV1 {
            event_id: None,
            timestamp_ms: 1_000 + index,
            command: format!("echo HK_HISTORY_{index:03}"),
            cwd: Some(cwd.clone()),
            shell: hokan::shell::ShellKind::Zsh,
            exit_code: Some(0),
            imported: false,
            occurrences: 1,
            cwd_occurrences: Some(1),
        })
        .collect();
    store.append_many(&events).expect("seed history");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.wait_for_sync_replies(1);

    // A burst that arrives before the first provider result must retain every
    // press, wrapping once before continuing upward through older pages.
    terminal.wait_for_cpr_replies(1);
    terminal.write(&b"\x1b[A".repeat(83));
    terminal.wait_for_selected_history("echo HK_HISTORY_077");
    let text = terminal.screen_text();
    assert!(
        text.find("HK_HISTORY_077") < text.find("HK_HISTORY_079"),
        "{text}"
    );
    terminal.write(&b"\x1b[A".repeat(50));
    terminal.wait_for_selected_history("echo HK_HISTORY_027");
    terminal.write(b"\x1b[B");
    terminal.wait_for_selected_history("echo HK_HISTORY_028");
    terminal.write(b"\x1b[5~");
    terminal.wait_for_selected_history("echo HK_HISTORY_022");
    terminal.write(b"\x1b[6~");
    terminal.wait_for_selected_history("echo HK_HISTORY_028");
    terminal.write(&b"\x1b[A".repeat(28));
    terminal.wait_for_selected_history("echo HK_HISTORY_000");
    terminal.write(b"\x1b[A");
    terminal.wait_for_selected_history("echo HK_HISTORY_079");
    terminal.write(b"\x1b[B");
    terminal.wait_for_selected_history("echo HK_HISTORY_000");
    terminal.write(b"\x1b[5~");
    terminal.wait_for_selected_history("echo HK_HISTORY_074");
    terminal.write(b"\x1b[6~");
    terminal.wait_for_selected_history("echo HK_HISTORY_000");
    terminal.write(b"\t");
    terminal.wait_for_bare_row("HK> echo HK_HISTORY_000");
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn arrows_open_history_without_invoking_zsh_arrow_widgets() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = empty_fixture_directories();
    fs::write(
        home.path().join(".zshrc"),
        "PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\nbindkey -e\n\
         function fixture_native_up() { BUFFER='echo HK_NATIVE_UP'; CURSOR=${#BUFFER}; zle redisplay }\n\
         function fixture_native_down() { BUFFER='echo HK_NATIVE_DOWN'; CURSOR=${#BUFFER}; zle redisplay }\n\
         zle -N fixture_native_up\n\
         zle -N fixture_native_down\n\
         bindkey $'\\e[A' fixture_native_up\n\
         bindkey $'\\eOA' fixture_native_up\n\
         bindkey $'\\e[B' fixture_native_down\n\
         bindkey $'\\eOB' fixture_native_down\n",
    )
    .expect("zsh arrow widget fixture");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    let prompt_probes = terminal.cpr_replies;
    terminal.write(b"echo HK_ARROW_HISTORY_SEED\r");
    terminal.wait_for_bare_row("HK_ARROW_HISTORY_SEED");
    terminal.wait_for_bare_row("HK>");
    // Output from echo can precede the PROMPT control event. Wait for
    // Hokan's new prompt probe before testing keys it owns while editing.
    terminal.wait_for_cpr_replies(prompt_probes + 1);

    // Application-cursor Up must be decoded by Hokan and open its history
    // list. The deliberately conflicting zle widget must never see the key.
    terminal.write(b"\x1bOA");
    terminal.wait_for_screen(TAG_HIS);
    terminal.wait_for_screen("echo HK_ARROW_HISTORY_SEED");
    terminal.wait_for_screen("▶");
    let text = terminal.screen_text();
    assert!(!text.contains("HK_NATIVE_UP"), "zle Up widget ran:\n{text}");

    terminal.write(b"\x1b");
    terminal.settle(Duration::from_millis(300));
    assert_no_overlay_rows(&terminal);

    // Standard CSI Down follows the same entry path after the list is closed.
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen(TAG_HIS);
    terminal.wait_for_screen("▶");
    let text = terminal.screen_text();
    assert!(
        !text.contains("HK_NATIVE_DOWN"),
        "zle Down widget ran:\n{text}"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}
