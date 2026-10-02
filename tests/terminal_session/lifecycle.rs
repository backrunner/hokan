use super::*;

#[test]
fn delayed_cursor_reply_does_not_type_29r_at_the_prompt() {
    if !command_exists("zsh") {
        return;
    }
    for private_cpr_supported in [true, false] {
        let (home, work) = fixture_directories();
        let prompt = format!("{}> ", "S".repeat(26));
        fs::write(
            home.path().join(".zshrc"),
            format!("PROMPT='{prompt}'\nRPROMPT=''\nsetopt no_beep\n"),
        )
        .expect("SSH-style prompt");
        let mut command = CommandBuilder::new(hokan_test_bin());
        command.arg("--shell");
        command.arg("zsh");
        configure_command(&mut command, &home, &work);
        let mut terminal = TerminalSession::spawn_command_with_private_cpr(
            home,
            work,
            command,
            2,
            private_cpr_supported,
        );
        terminal.cpr_reply_pause = Some(Duration::from_millis(350));
        terminal.wait_for_screen(&prompt);
        terminal.wait_for_bytes_since(0, HOKAN_CPR_QUERY);
        if !private_cpr_supported {
            // An unsupported private probe invalidates the initial anchor;
            // wait from the actual query, not from prompt display (the sync
            // query may still be pending under parallel load), then redraw
            // the empty prompt to make the standard fallback eligible.
            terminal.settle(Duration::from_millis(350));
            terminal.write(b"\x0c");
        }
        let deadline = Instant::now() + TIMEOUT;
        while terminal.cpr_reply_pause.is_some() && Instant::now() < deadline {
            terminal.receive_once(READ_POLL);
        }
        assert!(
            terminal.cpr_reply_pause.is_none(),
            "cursor probe was not answered (private_cpr_supported={private_cpr_supported}); tail={:?}",
            tail(&terminal.transcript, 1024)
        );
        terminal.settle(Duration::from_millis(200));
        assert!(
            !terminal.screen_text().contains("29R"),
            "cursor reply leaked into the prompt:\n{}",
            terminal.screen_text()
        );

        // Do not clear the buffer: the first command must run without any
        // injected cursor-report suffix, and literal user text must survive.
        let start = terminal.transcript.len();
        terminal.write(b"printf '%s' '29R' > reply-input.txt; echo hk_reply_clean | tr a-z A-Z\r");
        terminal.wait_for_bytes_since(start, b"HK_REPLY_CLEAN");
        assert_eq!(
            fs::read_to_string(terminal._work.path().join("reply-input.txt"))
                .expect("first command output"),
            "29R"
        );
        terminal.exit_shell();
        terminal.wait_until_exit();
    }
}

#[test]
fn terminal_without_private_cpr_uses_guarded_standard_fallback() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_without_private_cpr();
    terminal.wait_for_screen("HK> ");
    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(
        terminal
            .transcript
            .windows(b"\x1b[5n\x1b[6n".len())
            .any(|window| window == b"\x1b[5n\x1b[6n"),
        "guarded standard cursor probe was not emitted"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn terminal_fixture_does_not_answer_buffered_queries_after_process_exit() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    let replies = terminal.cpr_replies;
    // Simulate a descheduled terminal emulator: a child emits a query and
    // exits before the queued output is processed. Answering it now feeds
    // bytes into the restored outer PTY's canonical echo, not into Hokan.
    terminal.write(b"printf '\\033[?6n'; exit\r");
    let deadline = Instant::now() + TIMEOUT;
    while terminal.try_wait().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(terminal.try_wait().is_some(), "fixture child did not exit");
    terminal.settle(Duration::from_millis(100));
    assert_eq!(terminal.cpr_replies, replies, "answered a query after exit");
    assert!(terminal.transcript.ends_with(RESTORE_PRESENTATION));
}

#[test]
fn zsh_install_auto_starts_hokan_and_restores_the_terminal() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_via_zsh_install();
    terminal.wait_for_screen("HK1> ");
    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("HK1> ls"));
    terminal.exit_shell();
    terminal.wait_until_exit();
    assert!(
        terminal.transcript.ends_with(RESTORE_PRESENTATION),
        "terminal presentation was not restored; tail={:?}",
        tail(&terminal.transcript, 256)
    );
}

#[test]
fn hokan_leave_restores_the_unwrapped_auto_start_shell() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_via_zsh_install();
    terminal.wait_for_screen("HK1> ");
    let leave_start = terminal.transcript.len();
    terminal.write(b"hokan-leave\r");
    terminal.wait_for_bytes_since(leave_start, RESTORE_PRESENTATION);
    terminal.settle(Duration::from_millis(200));

    let start = terminal.transcript.len();
    terminal.write(b"printf 'HK_UNWRAPPED:%s\\n' \"${HOKAN_ACTIVE:-no}\"\r");
    terminal.wait_for_bytes_since(start, b"HK_UNWRAPPED:no");
    assert!(
        terminal.transcript[leave_start..start]
            .windows(RESTORE_PRESENTATION.len())
            .any(|window| window == RESTORE_PRESENTATION),
        "Hokan did not restore terminal presentation before the outer shell resumed"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn hokan_leave_exits_a_direct_session_successfully() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.write(b"hokan-leave\r");
    terminal.wait_until_exit();
    assert_eq!(
        terminal.try_wait().map(|status| status.exit_code()),
        Some(0)
    );
    assert!(terminal.transcript.ends_with(RESTORE_PRESENTATION));
}

#[test]
fn matching_child_exit_code_does_not_trigger_hokan_leave() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_via_zsh_install();
    terminal.wait_for_screen("HK1> ");
    terminal.write(b"exit 85\r");
    terminal.wait_until_exit();
    assert_eq!(
        terminal.try_wait().map(|status| status.exit_code()),
        Some(85)
    );
    assert!(terminal.transcript.ends_with(RESTORE_PRESENTATION));
}

#[test]
fn real_session_restores_canonical_and_echo_termios() {
    if !command_exists("zsh") || !command_exists("stty") {
        return;
    }
    let mut terminal = TerminalSession::spawn_termios_wrapper();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    terminal.write(b"\x04");
    terminal.wait_for_bytes_since(0, b"HK_TERMIO:yes:EXIT=0");
    terminal.wait_until_exit();
    assert!(
        terminal
            .transcript
            .windows(RESTORE_PRESENTATION.len())
            .any(|window| window == RESTORE_PRESENTATION),
        "presentation restore was not emitted before the wrapper resumed"
    );
}

#[test]
fn termination_signals_restore_the_terminal_after_an_active_overlay() {
    if !command_exists("zsh") {
        return;
    }
    for signal in [Signal::SIGTERM, Signal::SIGHUP] {
        let mut terminal = TerminalSession::spawn();
        terminal.wait_for_screen("HK> ");
        terminal.settle(Duration::from_millis(300));
        terminal.write(b"ls ");
        terminal.wait_for_screen(TAG_SPEC);

        signal::kill(Pid::from_raw(terminal.pid()), signal).expect("signal hokan");
        terminal.wait_until_exit();
        assert!(
            terminal.transcript.ends_with(RESTORE_PRESENTATION),
            "terminal presentation was not restored after {signal:?}; tail={:?}",
            tail(&terminal.transcript, 256)
        );
    }
}
