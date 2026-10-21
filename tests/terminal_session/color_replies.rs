use super::*;

#[test]
fn late_color_replies_do_not_type_hex_after_program_exit() {
    if !command_exists("zsh") || !command_exists("python3") {
        return;
    }
    for (prefix, suffix) in [
        (b"".as_slice(), b"\x1b]11;#ffffff\x07".as_slice()),
        (b"\x1b]11;".as_slice(), b"#ffffff\x1b\\".as_slice()),
        (b"\x1b]11;rgb:ffff/".as_slice(), b"ffff/ffff\x07".as_slice()),
    ] {
        let (home, work) = fixture_directories();
        fs::write(
            work.path().join("query.py"),
            r#"import os, sys, termios, tty
fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    os.write(1, b"\x1b]11;?\x07")
    count = int(sys.argv[1])
    prefix = bytearray()
    while len(prefix) < count:
        prefix.extend(os.read(fd, count-len(prefix)))
finally:
    termios.tcsetattr(fd, termios.TCSANOW, saved)
os.write(1, b"\r\nQUERY_CHILD_EXIT\r\n")
"#,
        )
        .expect("color query fixture");
        let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
        terminal.wait_for_sync_replies(1);
        terminal.wait_for_screen("HK> ");
        let start = terminal.transcript.len();
        terminal.write(format!("python3 ./query.py {}\r", prefix.len()).as_bytes());
        terminal.wait_for_bytes_since(start, b"\x1b]11;?\x07");
        if !prefix.is_empty() {
            terminal.write(prefix);
        }
        terminal.wait_for_bytes_since(start, b"QUERY_CHILD_EXIT");
        terminal.wait_for_bare_row("HK>");
        terminal.settle(Duration::from_millis(350));
        if prefix.is_empty() {
            // Delay before the owner key is available, beyond the bare-Esc
            // timeout. The OSC header must stay buffered across runtime ticks.
            terminal.write(&suffix[..2]);
            terminal.settle(Duration::from_millis(350));
            terminal.write(&suffix[2..]);
        } else {
            terminal.write(suffix);
        }
        terminal.settle(Duration::from_millis(150));
        let screen = terminal.screen_text();
        assert!(
            !screen.contains("ffffff") && !screen.contains("ffff/"),
            "color reply leaked:\n{screen}"
        );
        terminal.write(b"printf '%s\\n' '#ffffff'\r");
        terminal.wait_for_bare_row("#ffffff");
        terminal.exit_shell();
        terminal.wait_until_exit();
    }
}

#[test]
fn foreground_color_queries_still_receive_every_reply_byte() {
    if !command_exists("zsh") || !command_exists("python3") {
        return;
    }
    let (home, work) = fixture_directories();
    fs::write(
        work.path().join("query.py"),
        r#"import os, termios, tty
saved = termios.tcgetattr(0)
tty.setraw(0)
try:
    os.write(1, b"\x1b]4;15;?\x1b\\")
    reply = bytearray()
    while not reply.endswith(b"\x1b\\"):
        reply.extend(os.read(0, 1))
    assert reply == b"\x1b]4;15;rgb:ffff/ffff/ffff\x1b\\", repr(reply)
    os.write(1, b"\r\nCOLOR_PROTOCOL_OK\r\n")
finally:
    termios.tcsetattr(0, termios.TCSANOW, saved)
"#,
    )
    .expect("live query fixture");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    // Exercise a command pasted before startup has reached its first prompt.
    terminal.write(b"python3 ./query.py\r");
    terminal.wait_for_bytes_since(0, b"\x1b]4;15;?\x1b\\");
    for byte in b"\x1b]4;15;rgb:ffff/ffff/ffff\x1b\\" {
        terminal.write(&[*byte]);
    }
    terminal.wait_for_bare_row("COLOR_PROTOCOL_OK");
    terminal.wait_for_bare_row("HK>");
    terminal.exit_shell();
    terminal.wait_until_exit();
}
