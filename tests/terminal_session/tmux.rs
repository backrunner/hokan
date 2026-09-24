use super::*;

#[test]
fn tmux_36_uses_out_of_band_cursor_probe() {
    if !command_exists("zsh") || !tmux_is_36() {
        return;
    }
    let mut terminal = TerminalSession::spawn_in_tmux();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(500));
    let probe_start = terminal.transcript.len();
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
        "tmux 3.6b must not receive pane-level synchronized update frames"
    );
    assert!(
        !terminal.transcript[probe_start..]
            .windows(STANDARD_CPR_QUERY.len())
            .any(|window| window == STANDARD_CPR_QUERY),
        "Hokan must never issue an ambiguous standard CPR query under tmux"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn tmux_forwards_every_modified_f3_encoding_to_a_foreground_tui() {
    if !command_exists("zsh") || !command_exists("python3") || !command_exists("tmux") {
        return;
    }
    let mut terminal = TerminalSession::spawn_in_tmux();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    let fixture = terminal._work.path().join("tmux-f3.py");
    fs::write(
        &fixture,
        r#"import os
import sys
import termios
import tty

fd = sys.stdin.fileno()
expected = int(sys.argv[1])
saved = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    os.write(1, b"TMUX_F3_READY\r\n")
    payload = bytearray()
    while len(payload) < expected:
        payload.extend(os.read(fd, expected - len(payload)))
    os.write(1, b"TMUX_F3_INPUT=" + payload.hex().encode() + b"\r\n")
finally:
    termios.tcsetattr(fd, termios.TCSANOW, saved)
"#,
    )
    .expect("tmux F3 fixture");

    let input_chunks = [
        b"\x1b[1;1R".as_slice(),
        b"\x1b[1;2R".as_slice(),
        b"\x1b[1;3R".as_slice(),
        b"\x1b[1;4R".as_slice(),
        b"\x1b[1;5R".as_slice(),
        b"\x1b[1;6R".as_slice(),
        b"\x1b[1;7R".as_slice(),
        b"\x1b[1;8R".as_slice(),
    ];
    let input = input_chunks.concat();
    let expected_hex = input
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let command_start = terminal.transcript.len();
    terminal.write(format!("python3 ./tmux-f3.py {}\r", input.len()).as_bytes());
    terminal.wait_for_bytes_since(command_start, b"TMUX_F3_READY");
    for chunk in input_chunks {
        for byte in chunk.chunks(1) {
            terminal.write(byte);
        }
    }
    terminal.wait_for_bytes_since(
        command_start,
        format!("TMUX_F3_INPUT={expected_hex}").as_bytes(),
    );
    terminal.wait_for_bytes_since(command_start, b"HK> ");

    terminal.exit_shell();
    terminal.wait_until_exit();
}
