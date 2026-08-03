use std::{
    fs,
    io::Read,
    path::PathBuf,
    time::{Duration, Instant},
};

use crossbeam_channel::Receiver;
use hokan::{
    pty::PtyChild,
    shell::{ControlMessage, ShellEvent, ShellKind, ShellSession, replacement_sequence},
    terminal::TerminalSize,
};

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn zsh_exact_snapshot_and_native_replacement_work_in_a_real_pty() {
    if !command_exists("zsh") {
        return;
    }
    exercise_shell(ShellKind::Zsh, true);
}

#[test]
fn bash_32_mirrored_replacement_works_in_a_real_pty() {
    if !command_exists("bash") {
        return;
    }
    exercise_shell(ShellKind::Bash, false);
}

fn exercise_shell(shell: ShellKind, expect_exact_snapshot: bool) {
    let session = ShellSession::new(shell).expect("shell session");
    let cwd_fixture = tempfile::tempdir().expect("working directory fixture");
    let expected_cwd = cwd_fixture.path().join("cwd\twith-tab");
    fs::create_dir(&expected_cwd).expect("working directory with tab");
    let expected_cwd = fs::canonicalize(expected_cwd).expect("canonical working directory");
    let mut command = session
        .command_builder_isolated(false)
        .expect("isolated command");
    command.env("HOKAN_BIN", env!("CARGO_BIN_EXE_hokan"));
    command.env("TERM", "xterm-256color");
    command.env("PS1", "HK> ");
    command.cwd(&expected_cwd);

    let (sender, receiver) = crossbeam_channel::unbounded();
    let control = session
        .start_control_reader(sender)
        .expect("control reader");
    let size = TerminalSize::new(24, 80).expect("terminal size");
    let mut child = PtyChild::spawn(command, size).expect("spawn shell");
    child.enable_nonblocking_reads().expect("nonblocking PTY");
    let mut reader = child.take_reader().expect("PTY reader");

    assert_eq!(wait_for_prompt(&receiver, &mut *reader), expected_cwd);
    let replacement = "printf 'HK_REPLACED\\n'";
    if shell == ShellKind::Bash {
        child.write_all(b"\x01\x0b").expect("clear Readline buffer");
        child
            .write_all(replacement.as_bytes())
            .expect("write mirrored replacement");
    } else {
        session
            .write_edit(replacement, replacement.len())
            .expect("replacement payload");
        child
            .write_all(replacement_sequence(shell))
            .expect("replacement key");
    }

    if expect_exact_snapshot {
        wait_for_buffer(&receiver, &mut *reader, replacement);
    }
    child.write_all(b"\r").expect("submit replacement");
    let output = read_until(&mut reader, b"HK_REPLACED\r\n", TIMEOUT);
    assert!(
        output
            .windows(b"HK_REPLACED\r\n".len())
            .any(|part| part == b"HK_REPLACED\r\n"),
        "replacement did not execute; output={}",
        String::from_utf8_lossy(&output)
    );

    assert_eq!(wait_for_command_end(&receiver, replacement), expected_cwd);
    wait_for_prompt(&receiver, &mut *reader);
    let _ = read_until(&mut *reader, b"HK> ", Duration::from_secs(1));
    child.write_all(b"\x04").expect("exit shell");
    let deadline = Instant::now() + TIMEOUT;
    let mut exit_output = Vec::new();
    let mut exit_buffer = [0_u8; 4096];
    while Instant::now() < deadline {
        if child.try_wait().expect("child status").is_some() {
            drop(control);
            return;
        }
        match reader.read(&mut exit_buffer) {
            Ok(count) => exit_output.extend_from_slice(&exit_buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) || error.raw_os_error() == Some(5) => {}
            Err(error) => panic!("PTY exit read failed: {error}"),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child.kill().expect("kill timed out shell");
    panic!(
        "{shell} did not exit after the PTY fixture; output={}",
        String::from_utf8_lossy(&exit_output)
    );
}

fn wait_for_prompt(receiver: &Receiver<ControlMessage>, reader: &mut dyn Read) -> PathBuf {
    let deadline = Instant::now() + TIMEOUT;
    let mut messages = Vec::new();
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    while Instant::now() < deadline {
        while let Ok(message) = receiver.try_recv() {
            if let ControlMessage::Event(ShellEvent::Prompt { cwd, .. }) = message {
                return cwd;
            }
            messages.push(message);
        }
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => panic!("PTY read failed while waiting for prompt: {error}"),
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!(
        "shell did not emit a prompt event; messages={messages:#?}; output={}",
        String::from_utf8_lossy(&output)
    );
}

fn wait_for_buffer(receiver: &Receiver<ControlMessage>, reader: &mut dyn Read, expected: &str) {
    let deadline = Instant::now() + TIMEOUT;
    let mut messages = Vec::new();
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    while Instant::now() < deadline {
        while let Ok(message) = receiver.try_recv() {
            if matches!(
                message,
                ControlMessage::Event(ShellEvent::Buffer { ref text, .. }) if text == expected
            ) {
                return;
            }
            messages.push(message);
        }
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => panic!("PTY read failed while waiting for zsh buffer: {error}"),
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!(
        "zsh did not report the native replacement buffer; messages={messages:#?}; output={}",
        String::from_utf8_lossy(&output)
    );
}

fn wait_for_command_end(receiver: &Receiver<ControlMessage>, expected: &str) -> PathBuf {
    let deadline = Instant::now() + TIMEOUT;
    let mut messages = Vec::new();
    while let Ok(message) = receiver.recv_deadline(deadline) {
        if let ControlMessage::Event(ShellEvent::CommandEnd {
            exit_code: 0,
            ref cwd,
            ref command,
        }) = message
            && command == expected
        {
            return cwd.clone();
        }
        messages.push(message);
    }
    panic!("shell did not emit the expected command end event: {messages:#?}");
}

fn read_until(reader: &mut dyn Read, needle: &[u8], timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    while Instant::now() < deadline {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                output.extend_from_slice(&buffer[..count]);
                if output.windows(needle.len()).any(|part| part == needle) {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => panic!("PTY read failed: {error}"),
        }
    }
    output
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(name).is_file())
    })
}
