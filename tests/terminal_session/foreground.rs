use super::*;

#[test]
fn overlay_recovers_after_fullscreen_apps_exit() {
    if !command_exists("zsh") {
        return;
    }
    // name, bytes emitted while the TUI runs, bytes emitted at exit — clean
    // exit, leaked sync output, leaked alternate screen (crashed TUI), and an
    // overlong control string that desynchronizes the boundary scanner.
    for (name, during_bytes, exit_bytes) in [
        ("clean", "", "\\033[?2026l\\033[?1049l"),
        ("leaked-sync", "", "\\033[?1049l"),
        ("leaked-alt", "", ""),
        (
            "desync",
            "big=$(head -c 70000 /dev/zero | tr '\\0' 'a')\nprintf '\\033]52;c;%s\\007' \"$big\"\n",
            "\\033[?2026l\\033[?1049l",
        ),
    ] {
        let mut terminal = TerminalSession::spawn();
        terminal.wait_for_screen("HK> ");
        terminal.settle(Duration::from_millis(300));
        let fixture = terminal._work.path().join("tui.sh");
        fs::write(
            &fixture,
            format!(
                "#!/bin/sh\nprintf '\\033[?1049h\\033[?2026hTUI_{name}\\r\\n'\n{during_bytes}IFS= read -r key\nprintf '{exit_bytes}'\n"
            ),
        )
        .expect("tui fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&fixture).expect("metadata").permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&fixture, permissions).expect("chmod");
        }

        terminal.write(b"sh ./tui.sh\r");
        terminal.wait_for_screen("TUI_");
        terminal.write(b"x\r");
        terminal.wait_for_screen("HK> ");
        terminal.settle(Duration::from_millis(300));

        // Completions must come back after the app exits, whatever state it
        // left the terminal in.
        terminal.write(b"ls ");
        terminal.wait_for_screen(TAG_SPEC);

        terminal.exit_shell();
        terminal.wait_until_exit();
    }
}

#[test]
fn kimi_style_tui_queries_input_and_crash_recovery_are_byte_exact() {
    if !command_exists("zsh") || !command_exists("python3") {
        return;
    }
    let (home, work) = fixture_directories();
    let fixture = work.path().join("tui.py");
    fs::write(
        &fixture,
        r#"import os
import select
import sys
import termios
import time
import tty

fd = sys.stdin.fileno()
expected = int(sys.argv[1])
saved = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    os.write(1, b"\x1b[?2026h\x1b[?25l\x1b[?2004h\x1b[>7u\x1b[?u\x1b[c")
    os.write(1, b"\x1b[?1003h\x1b[?1004h\x1b[?1006h\x1b[?2031h")
    os.write(1, b"\x1b[>4;2m\x1b]11;?\x07\x1b[?996n\x1b[6n\x1b[?6n\x1b[?2026$p")
    reply = bytearray()
    deadline = time.monotonic() + 5
    while reply.count(b"R") < 2 or not reply.endswith(b"$y"):
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
            raise RuntimeError("terminal reply timeout")
        reply.extend(os.read(fd, 1))
    if not (reply.startswith(b"\x1b[?7u\x1b[?1;2c\x1b[") and b"R\x1b[?" in reply):
        raise RuntimeError("invalid terminal replies")
    os.write(1, b"\r\nTUI_PROTOCOL_OK\r\nTUI_INPUT_READY\r\n")
    payload = bytearray()
    while len(payload) < expected:
        payload.extend(os.read(fd, expected - len(payload)))
    os.write(1, b"\r\nTUI_INPUT=" + payload.hex().encode() + b"\r\n")
finally:
    termios.tcsetattr(fd, termios.TCSANOW, saved)
"#,
    )
    .expect("Kimi-style TUI fixture");

    // Codex/Grok-style TUIs receive shortcuts through classic control bytes,
    // Kitty CSI-u, or xterm modifyOtherKeys depending on the outer terminal.
    // Keep every encoding byte-exact across the foreground hand-off, including
    // when escape sequences arrive in separate writes.
    let mut input_chunks: Vec<Vec<u8>> = (0_u8..=0x1f).map(|byte| vec![byte]).collect();
    input_chunks.extend(
        [
            b"\x7f".as_slice(),
            b"\x80".as_slice(),
            b"\x9b".as_slice(),
            b"\xff".as_slice(),
            b"\x1bx".as_slice(),
            b"\x1bOP".as_slice(),
            b"\x1bOQ".as_slice(),
            b"\x1bOR".as_slice(),
            b"\x1bOS".as_slice(),
            b"\x1b[15~".as_slice(),
            b"\x1b[17~".as_slice(),
            b"\x1b[18~".as_slice(),
            b"\x1b[19~".as_slice(),
            b"\x1b[20~".as_slice(),
            b"\x1b[21~".as_slice(),
            b"\x1b[23~".as_slice(),
            b"\x1b[24~".as_slice(),
            b"\x1b[1;5P".as_slice(),
            b"\x1b[15;2~".as_slice(),
            b"\x1b[Z".as_slice(),
            b"\x1bOA".as_slice(),
            b"\x1bOB".as_slice(),
            b"\x1bOC".as_slice(),
            b"\x1bOD".as_slice(),
            b"\x1bOp".as_slice(),
            b"\x1bOq".as_slice(),
            b"\x1b[1;5A".as_slice(),
            b"\x1b[1;3D".as_slice(),
            b"\x1b[1;2H".as_slice(),
            b"\x1b[1;6F".as_slice(),
            b"\x1b[1;1R".as_slice(),
            b"\x1b[1;2R".as_slice(),
            b"\x1b[1;3R".as_slice(),
            b"\x1b[1;4R".as_slice(),
            b"\x1b[1;5R".as_slice(),
            b"\x1b[1;6R".as_slice(),
            b"\x1b[1;7R".as_slice(),
            b"\x1b[1;8R".as_slice(),
            b"\x1b[13;2u".as_slice(),
            b"\x1b[32;5u".as_slice(),
            b"\x1b[97;1:2u".as_slice(),
            b"\x1b[97;1:3u".as_slice(),
            b"\x1b[99;5u".as_slice(),
            b"\x1b[46;5u".as_slice(),
            b"\x1b[27;5;99~".as_slice(),
            b"\x1b[I".as_slice(),
            b"\x1b[O".as_slice(),
            b"\x1b[<0;10;5M".as_slice(),
            b"\x1b[<0;10;5m".as_slice(),
            b"\x1b[M !!".as_slice(),
            b"\x1b[97;3u".as_slice(),
            "\x1b[200~粘贴中文🙂e\u{301}👩‍💻\x1b[201~".as_bytes(),
        ]
        .into_iter()
        .map(<[u8]>::to_vec),
    );
    let input = input_chunks.concat();
    let expected_hex = input
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // Queue the command before the first prompt event is consumed. This
    // exercises the startup handoff path used by a fast paste-and-Enter.
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    let command_start = terminal.transcript.len();
    terminal.write(format!("python3 ./tui.py {}\r", input.len()).as_bytes());
    terminal.wait_for_bytes_since(command_start, b"TUI_PROTOCOL_OK");
    terminal.wait_for_bytes_since(command_start, b"TUI_INPUT_READY");
    for chunk in &input_chunks {
        for byte in chunk.chunks(1) {
            terminal.write(byte);
        }
    }
    terminal.wait_for_bytes_since(
        command_start,
        format!("TUI_INPUT={expected_hex}").as_bytes(),
    );
    for reset in [
        b"\x1b[?1003l".as_slice(),
        b"\x1b[?1004l".as_slice(),
        b"\x1b[?1006l".as_slice(),
        b"\x1b[?2031l".as_slice(),
        b"\x1b[>4;0m".as_slice(),
        b"\x1b[<u".as_slice(),
        b"\x1b[?2026l".as_slice(),
    ] {
        // The prompt bytes can beat the PROMPT control message. Wait for
        // recovery itself instead of treating a fixed delay as readiness.
        terminal.wait_for_bytes_since(command_start, reset);
    }

    terminal.write("printf '终端恢复🙂e\u{301}👩‍💻\\n'\r".as_bytes());
    terminal.wait_for_screen("终端恢复🙂e\u{301}👩‍💻");
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn foreground_job_control_handles_ctrl_z_fg_and_ctrl_c() {
    if !command_exists("zsh") || !command_exists("sleep") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    let fixture = terminal._work.path().join("job-control.sh");
    fs::write(
        &fixture,
        "#!/bin/sh\nprintf 'JOB_RUNNING\\n'\nexec sleep 30\n",
    )
    .expect("job-control fixture");

    let command_start = terminal.transcript.len();
    terminal.write(b"sh ./job-control.sh\r");
    terminal.wait_for_bytes_since(command_start, b"JOB_RUNNING");

    let suspend_start = terminal.transcript.len();
    terminal.write(b"\x1a");
    terminal.wait_for_bytes_since(suspend_start, b"HK> ");
    terminal.settle(Duration::from_millis(100));

    let jobs_start = terminal.transcript.len();
    terminal.write(b"jobs -s\r");
    terminal.wait_for_bytes_since(jobs_start, b"job-control.sh");
    terminal.wait_for_bytes_since(jobs_start, b"HK> ");

    terminal.write(b"fg\r");
    terminal.settle(Duration::from_millis(200));
    let interrupt_start = terminal.transcript.len();
    terminal.write(b"\x03");
    terminal.wait_for_bytes_since(interrupt_start, b"HK> ");

    terminal.write(b"printf 'JOB_CONTROL_OK\\n'\r");
    terminal.wait_for_screen("JOB_CONTROL_OK");
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn foreground_tui_receives_unicode_multiline_paste_incrementally() {
    if !command_exists("zsh") || !command_exists("python3") {
        return;
    }
    let (home, work) = fixture_directories();
    let fixture = work.path().join("incremental-paste-tui.py");
    fs::write(
        &fixture,
        r#"import os
import sys
import termios
import tty

fd = sys.stdin.fileno()
payload_size = int(sys.argv[1])
saved = termios.tcgetattr(fd)

def read_exact(size):
    data = bytearray()
    while len(data) < size:
        chunk = os.read(fd, size - len(data))
        if not chunk:
            raise RuntimeError("unexpected EOF")
        data.extend(chunk)
    return bytes(data)

tty.setraw(fd)
try:
    os.write(1, b"\x1b[?2004h\r\nTUI_PASTE_READY\r\n")
    start = read_exact(6)
    if start != b"\x1b[200~":
        raise RuntimeError("invalid paste start: " + start.hex())
    os.write(1, b"TUI_PASTE_STARTED\r\n")

    payload = read_exact(payload_size)
    os.write(1, b"TUI_PASTE_PAYLOAD=" + payload.hex().encode() + b"\r\n")

    end = read_exact(6)
    if end != b"\x1b[201~":
        raise RuntimeError("invalid paste end: " + end.hex())
    os.write(1, b"TUI_PASTE_ENDED\r\n")
finally:
    termios.tcsetattr(fd, termios.TCSANOW, saved)
"#,
    )
    .expect("incremental paste TUI fixture");

    let payload = "first line\n第二行🙂e\u{301}👩‍💻\nthird line".as_bytes();
    let payload_hex = payload
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    let command_start = terminal.transcript.len();
    terminal.write(format!("python3 ./incremental-paste-tui.py {}\r", payload.len()).as_bytes());
    terminal.wait_for_bytes_since(command_start, b"TUI_PASTE_READY");

    // Each write waits for an acknowledgement that cannot be emitted until
    // the TUI receives that stage. A decoder that buffers until PASTE_END will
    // therefore fail at TUI_PASTE_STARTED instead of passing on final bytes.
    terminal.write(PASTE_START);
    terminal.wait_for_bytes_since(command_start, b"TUI_PASTE_STARTED");
    terminal.write(payload);
    terminal.wait_for_bytes_since(
        command_start,
        format!("TUI_PASTE_PAYLOAD={payload_hex}").as_bytes(),
    );
    terminal.write(PASTE_END);
    terminal.wait_for_bytes_since(command_start, b"TUI_PASTE_ENDED");
    terminal.wait_for_bytes_since(command_start, b"HK> ");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn foreground_tui_receives_multi_mib_binary_paste_byte_exact() {
    if !command_exists("zsh") || !command_exists("python3") {
        return;
    }
    let (home, work) = fixture_directories();
    let fixture = work.path().join("big-paste-tui.py");
    fs::write(
        &fixture,
        r#"import os
import sys
import termios
import time
import tty

fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    os.write(1, b"\x1b[?2004h\r\nBIG_PASTE_READY\r\n")
    # Simulate a TUI (codex/kimi style) that is briefly busy before it starts
    # draining a paste: the bytes must stay queued, not crash the session.
    time.sleep(0.3)
    data = bytearray()
    while not data.endswith(b"\x1b[201~"):
        chunk = os.read(fd, 65536)
        if not chunk:
            raise RuntimeError("unexpected EOF")
        data.extend(chunk)
    if not data.startswith(b"\x1b[200~"):
        raise RuntimeError("invalid paste start: " + data[:16].hex())
    payload = bytes(data[6:-6])
    with open("big-paste-out.bin", "wb") as output:
        output.write(payload)
    os.write(1, ("BIG_PASTE_LEN=%d\r\n" % len(payload)).encode())
finally:
    termios.tcsetattr(fd, termios.TCSANOW, saved)
"#,
    )
    .expect("big paste TUI fixture");

    // Every possible byte value, plus reply-shaped escape text: a binary
    // image paste in miniature. The i % 256 pattern can never contain
    // PASTE_END (0x1b is only ever followed by 0x1c).
    let mut payload: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
    let midpoint = payload.len() / 2;
    payload.splice(
        midpoint..midpoint,
        b"\x1b[?12;34R\x1b[?2026;2$y\x1b[6n\x1b[c".iter().copied(),
    );
    assert!(
        !payload
            .windows(PASTE_END.len())
            .any(|window| window == PASTE_END),
        "fixture payload must not contain the paste terminator"
    );
    let mut paste = Vec::with_capacity(payload.len() + 12);
    paste.extend_from_slice(PASTE_START);
    paste.extend_from_slice(&payload);
    paste.extend_from_slice(PASTE_END);

    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    let command_start = terminal.transcript.len();
    terminal.write(b"python3 ./big-paste-tui.py\r");
    terminal.wait_for_bytes_since(command_start, b"BIG_PASTE_READY");

    for chunk in paste.chunks(64 * 1024) {
        terminal.write(chunk);
    }
    terminal.wait_for_bytes_since(
        command_start,
        format!("BIG_PASTE_LEN={}", payload.len()).as_bytes(),
    );
    terminal.wait_for_bytes_since(command_start, b"HK> ");

    let received =
        fs::read(terminal._work.path().join("big-paste-out.bin")).expect("payload capture file");
    assert_eq!(
        received.len(),
        payload.len(),
        "TUI received a truncated paste"
    );
    assert_eq!(
        received, payload,
        "TUI did not receive the paste byte-exact"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn background_process_holding_pty_does_not_block_shutdown() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    // A detached grandchild inherits the PTY slave, so the master never sees
    // EOF after zsh exits. Teardown must cancel the read pump instead of
    // waiting on it — otherwise `wait_until_exit` would outlive the sleep.
    terminal.write(b"(sleep 300 &) ; exit\r");
    terminal.wait_until_exit();
}
