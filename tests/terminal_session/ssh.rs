use super::*;

#[test]
fn ssh_pty_preserves_queries_unicode_resize_ctrl_c_and_escape() {
    if !command_exists("zsh")
        || !command_exists("ssh")
        || !command_exists("sshd")
        || !command_exists("ssh-keygen")
        || !command_exists("python3")
    {
        return;
    }

    let server = SshServer::start();
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_secs(1));

    let fixture = terminal._work.path().join("ssh-pty.py");
    fs::write(
        &fixture,
        r#"import fcntl
import os
import select
import signal
import struct
import sys
import termios
import time
import tty

fd = sys.stdin.fileno()

def write(data):
    os.write(1, data)

def dimensions():
    raw = fcntl.ioctl(fd, termios.TIOCGWINSZ, b"\0" * 8)
    return struct.unpack("HHHH", raw)[:2]

saved = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    write(b"\x1b[?1004h\x1b[?2004lSSH_READY\r\n\x1b[6n")
    reply = bytearray()
    deadline = time.monotonic() + 5
    while not reply.endswith(b"R"):
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
            raise RuntimeError("CPR timeout")
        reply.extend(os.read(fd, 1))
    if not (reply.startswith(b"\x1b[") and b";" in reply):
        raise RuntimeError("invalid CPR")
    write(b"\r\nSSH_CPR_OK\r\n")
    rows, cols = dimensions()
    write((f"SSH_SIZE={rows}x{cols}\r\n").encode())

    deadline = time.monotonic() + 5
    while True:
        rows, cols = dimensions()
        if (rows, cols) == (30, 100):
            write(b"SSH_RESIZE_OK=30x100\r\n")
            break
        if time.monotonic() >= deadline:
            raise RuntimeError(f"resize timeout: {rows}x{cols}")
        select.select([fd], [], [], 0.05)

    payload = bytearray()
    while True:
        payload.extend(os.read(fd, 4096))
        if b"\r" in payload or b"\n" in payload:
            break
    end = min(
        [index for index in (payload.find(b"\r"), payload.find(b"\n")) if index >= 0]
    )
    write(b"SSH_INPUT=" + bytes(payload[:end]).hex().encode() + b"\r\n")
    write(b"SSH_CTRL_C_READY\r\n")

    def interrupted(_signal, _frame):
        write(b"\r\nSSH_CTRL_C_OK\r\n")
        raise SystemExit(42)

    signal.signal(signal.SIGINT, interrupted)
    while True:
        if select.select([fd], [], [], 1)[0]:
            extra = os.read(fd, 64)
            if b"\x03" in extra:
                write(b"\r\nSSH_CTRL_C_OK\r\n")
                raise SystemExit(42)
finally:
    termios.tcsetattr(fd, termios.TCSANOW, saved)
"#,
    )
    .expect("SSH PTY fixture");
    let input = "\x1b[200~远端🙂e\u{301}👩‍💻\x1b[201~";
    let expected_hex = input
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // IdentitiesOnly still contacts SSH_AUTH_SOCK. Bypass both the user's
    // agent and config so a stalled agent cannot hang this private fixture.
    let command = format!(
        "ssh -F /dev/null -tt -o LogLevel=ERROR -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o IdentityAgent=none -i {} -p {} {}@127.0.0.1 python3 -u {}",
        shell_quote(&server.identity_key),
        server.port,
        shell_quote_text(&server.username),
        shell_quote(&fixture),
    );
    let command_start = terminal.transcript.len();
    terminal.write(format!("{command}\r").as_bytes());
    terminal.wait_for_bytes_since(command_start, b"SSH_READY");
    terminal.wait_for_bytes_since(command_start, b"SSH_CPR_OK");
    terminal.wait_for_bytes_since(command_start, b"SSH_SIZE=24x80");

    terminal.resize(30, 100);
    terminal.wait_for_bytes_since(command_start, b"SSH_RESIZE_OK=30x100");
    terminal.write(input.as_bytes());
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(
        command_start,
        format!("SSH_INPUT={expected_hex}").as_bytes(),
    );
    terminal.wait_for_bytes_since(command_start, b"SSH_CTRL_C_READY");
    terminal.write(b"\x03");
    terminal.wait_for_bytes_since(command_start, b"SSH_CTRL_C_OK");
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    assert!(
        terminal.cpr_replies >= 1,
        "the outer terminal did not answer the remote CPR query"
    );

    let escape_fixture = terminal._work.path().join("ssh-escape.py");
    fs::write(
        &escape_fixture,
        r#"import os
import termios
import time
import tty

fd = 0
saved = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    os.write(1, b"SSH_ESCAPE_READY\r\n")
    while True:
        time.sleep(1)
finally:
    termios.tcsetattr(fd, termios.TCSANOW, saved)
"#,
    )
    .expect("SSH escape fixture");
    let escape_command = format!(
        "ssh -F /dev/null -tt -o LogLevel=ERROR -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o IdentityAgent=none -i {} -p {} {}@127.0.0.1 python3 -u {}",
        shell_quote(&server.identity_key),
        server.port,
        shell_quote_text(&server.username),
        shell_quote(&escape_fixture),
    );
    let escape_start = terminal.transcript.len();
    terminal.write(format!("{escape_command}\r").as_bytes());
    terminal.wait_for_bytes_since(escape_start, b"SSH_ESCAPE_READY");
    terminal.write(b"\r~.");
    terminal.wait_for_bytes_since(escape_start, b"HK> ");

    terminal.write(b"true\r");
    terminal.wait_for_bare_row("HK>");
    terminal.exit_shell();
    terminal.wait_until_exit();
    assert_eq!(
        terminal.try_wait().map(|status| status.exit_code()),
        Some(0)
    );
    drop(server);
}
