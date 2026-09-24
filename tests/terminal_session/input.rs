use super::*;

#[test]
fn unicode_bracketed_paste_reaches_zsh_without_visible_delimiters() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    let pasted_text = "粘贴中文🙂e\u{301}👩‍💻";
    let command = format!(
        "printf '%s' '{pasted_text}' > unicode-paste.txt && echo hk_paste_done | tr a-z A-Z"
    );
    let start = terminal.transcript.len();
    terminal.write(PASTE_START);
    terminal.write(command.as_bytes());
    terminal.write(PASTE_END);
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(start, b"HK_PASTE_DONE");
    assert_eq!(
        fs::read_to_string(terminal._work.path().join("unicode-paste.txt"))
            .expect("pasted Unicode fixture should exist"),
        pasted_text
    );
    let output = &terminal.transcript[start..];
    assert!(
        !output
            .windows(PASTE_START.len())
            .any(|window| window == PASTE_START),
        "paste start delimiter leaked into child output"
    );
    assert!(
        !output
            .windows(PASTE_END.len())
            .any(|window| window == PASTE_END),
        "paste end delimiter leaked into child output"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn bash_unicode_bracketed_paste_reaches_readline_without_visible_delimiters() {
    if !command_exists("bash") {
        return;
    }
    let mut terminal = TerminalSession::spawn_bash();
    terminal.wait_for_screen("BASH> ");
    terminal.settle(Duration::from_millis(300));

    let pasted_text = "粘贴中文🙂e\u{301}👩‍💻";
    let command = format!(
        "printf '%s' '{pasted_text}' > bash-unicode-paste.txt && echo hk_bash_paste_done | tr a-z A-Z"
    );
    let start = terminal.transcript.len();
    terminal.write(PASTE_START);
    terminal.write(command.as_bytes());
    terminal.write(PASTE_END);
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(start, b"HK_BASH_PASTE_DONE");
    assert_eq!(
        fs::read_to_string(terminal._work.path().join("bash-unicode-paste.txt"))
            .expect("Bash pasted Unicode fixture should exist"),
        pasted_text
    );
    let output = &terminal.transcript[start..];
    assert!(
        !output
            .windows(PASTE_START.len())
            .any(|window| window == PASTE_START),
        "Bash paste start delimiter leaked into child output"
    );
    assert!(
        !output
            .windows(PASTE_END.len())
            .any(|window| window == PASTE_END),
        "Bash paste end delimiter leaked into child output"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn common_zsh_editing_keys_remain_native() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"discard this");
    terminal.write(b"\x15"); // Ctrl-U
    terminal.write(b"Xecho hk_native_edit | tr a-z A-ZY");
    terminal.write(b"\x1b[H\x1b[3~"); // Home, Delete
    terminal.write(b"\x1b[F\x7f"); // End, Backspace
    terminal.write(b"\x01\x0b"); // Ctrl-A, Ctrl-K
    terminal.write(b"echo hk_native_edit | tr a-z A-Z extra");
    terminal.write(b"\x17"); // Ctrl-W
    terminal.write(b"\x01\x1b[C\x1b[D\x05"); // Ctrl-A, Right, Left, Ctrl-E
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(start, b"HK_NATIVE_EDIT");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn common_bash_editing_keys_remain_native() {
    if !command_exists("bash") {
        return;
    }
    let mut terminal = TerminalSession::spawn_bash();
    terminal.wait_for_screen("BASH> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"discard this");
    terminal.write(b"\x15"); // Ctrl-U
    terminal.write(b"Xecho hk_bash_native_edit | tr a-z A-ZY");
    terminal.write(b"\x1b[H\x1b[3~"); // Home, Delete
    terminal.write(b"\x1b[F\x7f"); // End, Backspace
    terminal.write(b"\x01\x0b"); // Ctrl-A, Ctrl-K
    terminal.write(b"echo hk_bash_native_edit | tr a-z A-Z extra");
    terminal.write(b"\x17"); // Ctrl-W
    terminal.write(b"\x01\x1b[C\x1b[D\x05"); // Ctrl-A, Right, Left, Ctrl-E
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(start, b"HK_BASH_NATIVE_EDIT");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn zsh_prompt_accepts_oversized_multiline_paste_byte_exact() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = fixture_directories();
    let state_dir = home.path().join(".local/state");
    let out_file = work.path().join("zsh-big-paste.txt");
    // Beyond MAX_PASTE_BYTES the decoder streams PasteFragments; zsh must
    // still see one coherent bracketed paste. Newlines and CJK/emoji stay
    // literal inside the single-quoted argument.
    let unit = "中文字符🙂 spaced payload line\n";
    let repeats = (2 * 1024 * 1024) / unit.len() + 1;
    let payload = unit.repeat(repeats);
    // The trailing marker lands in the same file only after the multi-megabyte
    // `print` has fully flushed, so it doubles as the completion sentinel.
    let command = format!(
        "{{ print -r -- '{}'; print -r -- 'HK_PASTE_DONE'; }} > zsh-big-paste.txt",
        payload
    );

    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    let mut paste = Vec::with_capacity(command.len() + 12);
    paste.extend_from_slice(PASTE_START);
    paste.extend_from_slice(command.as_bytes());
    paste.extend_from_slice(PASTE_END);
    for chunk in paste.chunks(64 * 1024) {
        terminal.write(chunk);
    }
    terminal.settle(Duration::from_millis(300));
    let command_start = terminal.transcript.len();
    terminal.write(b"\r");

    // The redirect creates the output file before `print` runs, so existence
    // alone cannot prove the write finished. Poll until the trailing sentinel
    // is flushed — that can only happen after the whole payload was written.
    let expected = format!("{payload}\nHK_PASTE_DONE\n");
    let deadline = Instant::now() + TIMEOUT;
    let mut received = Vec::new();
    while Instant::now() < deadline {
        terminal.receive_once(READ_POLL);
        if let Ok(content) = fs::read(&out_file) {
            received = content;
            if received.ends_with(b"HK_PASTE_DONE\n") {
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    let debug_log = state_dir.join("hokan/debug.log");
    let debug_log_text =
        fs::read_to_string(&debug_log).unwrap_or_else(|_| "<no debug log>".to_owned());
    assert!(
        received.ends_with(b"HK_PASTE_DONE\n"),
        "zsh did not finish the paste command; received_len={} transcript len={} contains_cmd={} contains_end={} tail={:?} debug_log_tail={:?}",
        received.len(),
        terminal.transcript.len(),
        terminal
            .transcript
            .windows(b"zsh-big-paste".len())
            .any(|w| w == b"zsh-big-paste"),
        terminal
            .transcript
            .windows(PASTE_END.len())
            .any(|w| w == PASTE_END),
        tail(&terminal.transcript, 2_048),
        tail(debug_log_text.as_bytes(), 4_096)
    );
    let received = fs::read_to_string(&out_file).expect("zsh paste output file");
    assert!(
        received == expected,
        "zsh buffer did not receive the paste; received_len={} expected_len={} received_head={:?} received_tail={:?} transcript_tail={:?}",
        received.len(),
        expected.len(),
        head(received.as_bytes(), 256),
        tail(received.as_bytes(), 256),
        tail(&terminal.transcript, 1_024),
    );
    // The session must still be alive and back at the editing prompt.
    terminal.wait_for_bytes_since(command_start, b"HK> ");

    terminal.exit_shell();
    terminal.wait_until_exit();
}
