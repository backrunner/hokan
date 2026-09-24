use super::*;

#[test]
fn real_session_title_tracks_foreground_and_returns_to_shell() {
    for shell in ["zsh", "bash", "fish"] {
        if !command_exists(shell) {
            continue;
        }
        let (home, work) = fixture_directories();
        fs::write(home.path().join(".bashrc"), "PS1='HK> '\n").expect("bash fixture");
        fs::create_dir_all(home.path().join(".config/fish")).expect("fish directory");
        fs::write(
            home.path().join(".config/fish/config.fish"),
            "function fish_prompt; printf 'HK> '; end\nfunction fish_title; end\n",
        )
        .expect("fish fixture");
        let mut command = CommandBuilder::new(hokan_test_bin());
        command.args(["--shell", shell]);
        configure_command(&mut command, &home, &work);
        let mut terminal = TerminalSession::spawn_command(home, work, command, 2);
        terminal.wait_for_screen("HK> ");
        terminal.wait_for_sync_replies(1);
        let start = terminal.transcript.len();
        terminal.write(b"sleep 30\r");
        terminal.wait_for_bytes_since(start, b"\x1b]0;sleep\x07");
        let start = terminal.transcript.len();
        terminal.write(b"\x03");
        terminal.wait_for_bytes_since(start, format!("\x1b]0;{shell}\x07").as_bytes());
        terminal.exit_shell();
        terminal.wait_until_exit();
    }
}

#[test]
fn real_session_title_preserves_application_osc_until_prompt() {
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    let start = terminal.transcript.len();
    terminal.write(b"printf '\\033]0;APP_CUSTOM\\007'; sleep 30\r");
    terminal.wait_for_bytes_since(start, b"\x1b]0;APP_CUSTOM\x07");
    let custom = terminal.transcript.len();
    terminal.settle(Duration::from_millis(1200));
    assert!(
        !terminal.transcript[custom..]
            .windows(4)
            .any(|b| b == b"\x1b]0;")
    );
    terminal.write(b"\x03");
    terminal.wait_for_bytes_since(custom, b"\x1b]0;zsh\x07");
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn real_session_title_tracks_exec_replacing_the_shell() {
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    let start = terminal.transcript.len();
    terminal.write(b"exec /bin/sleep 30\r");
    terminal.wait_for_bytes_since(start, b"\x1b]0;sleep\x07");
    terminal.write(b"\x03");
    terminal.wait_until_exit();
}

#[test]
fn real_session_title_crosses_tmux_when_window_titles_are_enabled() {
    if !command_exists("tmux") {
        return;
    }
    let mut terminal = TerminalSession::spawn_in_tmux();
    terminal.wait_for_screen("HK> ");
    let start = terminal.transcript.len();
    terminal.write(b"tmux set -g set-titles-string '#{pane_title}'; tmux set -g set-titles on\r");
    terminal.wait_for_bytes_since(start, b";zsh\x07");
    let start = terminal.transcript.len();
    terminal.write(b"sleep 30\r");
    terminal.wait_for_bytes_since(start, b";sleep\x07");
    let start = terminal.transcript.len();
    terminal.write(b"\x03");
    terminal.wait_for_bytes_since(start, b";zsh\x07");
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn real_session_title_preserves_theme_prompt_and_can_be_disabled() {
    for enabled in [true, false] {
        let (home, work) = fixture_directories();
        fs::write(
            home.path().join(".config/hokan/config.toml"),
            format!("version = 1\n[ui]\nsync_title = {enabled}\n"),
        )
        .expect("title config");
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT='HK> '\nRPROMPT=''\nprecmd() { printf '\\033]0;THEME\\007'; }\n",
        )
        .expect("theme fixture");
        let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
        terminal.wait_for_screen("HK> ");
        terminal.wait_for_sync_replies(1);
        let start = terminal.transcript.len();
        terminal.write(b"sleep 30\r");
        if enabled {
            terminal.wait_for_bytes_since(start, b"\x1b]0;sleep\x07");
        } else {
            terminal.settle(Duration::from_millis(700));
        }
        let start = terminal.transcript.len();
        terminal.write(b"\x03");
        terminal.wait_for_bytes_since(start, b"\x1b]0;THEME\x07");
        terminal.settle(Duration::from_millis(150));
        assert!(
            !terminal.transcript[start..]
                .windows(8)
                .any(|b| b == b"\x1b]0;zsh\x07")
        );
        if !enabled {
            assert!(
                !terminal
                    .transcript
                    .windows(10)
                    .any(|b| b == b"\x1b]0;sleep\x07")
            );
        }
        terminal.exit_shell();
        terminal.wait_until_exit();
    }
}
