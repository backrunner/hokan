use std::process::{Command, Stdio};

#[test]
fn terminal_session_failure_prints_an_actionable_recovery_hint() {
    let output = Command::new(env!("CARGO_BIN_EXE_hokann"))
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run hokann without a TTY");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("requires terminal stdin and stdout"));
    assert!(stderr.contains("stty sane"));
}
