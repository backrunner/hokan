#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nix::{
    sys::{signal, signal::Signal},
    unistd::Pid,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(10);
const READ_POLL: Duration = Duration::from_millis(5);
const SYNC_QUERY: &[u8] = b"\x1b[?2026$p";
const CPR_QUERY: &[u8] = b"\x1b[6n";
const RESTORE_PRESENTATION: &[u8] = b"\x18\x1b[0m\x1b[?25h";

struct TerminalSession {
    _home: TempDir,
    _work: TempDir,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
    chunks: Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
    terminal: vt100::Parser,
    rows: u16,
    cols: u16,
    transcript: Vec<u8>,
    probe_tail: Vec<u8>,
    sync_status: u8,
    sync_replies: usize,
    cpr_replies: usize,
}

impl TerminalSession {
    fn spawn() -> Self {
        Self::spawn_with_sync_status(2)
    }

    fn spawn_with_sync_status(sync_status: u8) -> Self {
        let (home, work) = fixture_directories();
        Self::spawn_hokann(home, work, sync_status)
    }

    fn spawn_with_dynamic_prompt() -> Self {
        let (home, work) = empty_fixture_directories();
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT='BASE> '\nRPROMPT=''\nsetopt no_beep\n\
             autoload -Uz add-zsh-hook\n\
             function fixture_dynamic_prompt() { PROMPT='DYN> ' }\n\
             add-zsh-hook precmd fixture_dynamic_prompt\n",
        )
        .expect("dynamic prompt fixture");
        Self::spawn_hokann(home, work, 2)
    }

    fn spawn_with_user_zdotdir() -> Self {
        let (home, work) = empty_fixture_directories();
        let user_zdotdir = home.path().join("custom-zdotdir");
        fs::create_dir(&user_zdotdir).expect("custom ZDOTDIR");
        fs::write(
            home.path().join(".zshenv"),
            "export ZDOTDIR=\"$HOME/custom-zdotdir\"\n",
        )
        .expect("zshenv fixture");
        fs::write(
            user_zdotdir.join(".zshrc"),
            "PROMPT='ZDOT> '\nRPROMPT=''\nsetopt no_beep\n",
        )
        .expect("custom zshrc fixture");
        Self::spawn_hokann(home, work, 2)
    }

    fn spawn_via_zsh_setup() -> Self {
        let (home, work) = fixture_directories();
        let rc_path = home.path().join(".zshrc");
        fs::write(
            &rc_path,
            "typeset -gi HOKANN_RC_LOADS=${HOKANN_RC_LOADS:-0}\n\
             (( HOKANN_RC_LOADS++ ))\n\
             export HOKANN_RC_LOADS\n\
             PROMPT=\"HK${HOKANN_RC_LOADS}> \"\n\
             RPROMPT=''\n\
             setopt no_beep\n",
        )
        .expect("auto-start zshrc fixture");
        let output = Command::new(hokann_test_bin())
            .arg("--shell")
            .arg("zsh")
            .arg("setup")
            .arg("--rc-file")
            .arg(&rc_path)
            .env("HOME", home.path())
            .env_remove("HOKANN_ACTIVE")
            .env_remove("HOKANN_BIN")
            .output()
            .expect("run hokann setup");
        assert!(
            output.status.success(),
            "setup failed: stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let installed = fs::read_to_string(&rc_path).expect("installed zshrc");
        assert!(installed.contains("HOKANN_AUTO_START"));

        let mut command = CommandBuilder::new("zsh");
        command.arg("-i");
        configure_command(&mut command, &home, &work);
        Self::spawn_command(home, work, command, 2)
    }

    fn spawn_hokann(home: TempDir, work: TempDir, sync_status: u8) -> Self {
        let mut command = CommandBuilder::new(hokann_test_bin());
        command.arg("--shell");
        command.arg("zsh");
        configure_command(&mut command, &home, &work);
        Self::spawn_command(home, work, command, sync_status)
    }

    fn spawn_termios_wrapper() -> Self {
        let (home, work) = fixture_directories();
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg(
            "before=$(stty -g) || exit 90\n\
             \"$HOKANN_TEST_BIN\" --shell zsh\n\
             status=$?\n\
             after=$(stty -g) || exit 91\n\
             if [ \"$before\" = \"$after\" ]; then restored=yes; else restored=no; fi\n\
             printf '\\r\\nHK_TERMIO:%s:EXIT=%s\\r\\n' \"$restored\" \"$status\"",
        );
        command.env("HOKANN_TEST_BIN", hokann_test_bin());
        configure_command(&mut command, &home, &work);
        Self::spawn_command(home, work, command, 2)
    }

    fn spawn_in_tmux_36() -> Self {
        let (home, work) = fixture_directories();
        let tmux_config = home.path().join("tmux.conf");
        fs::write(
            &tmux_config,
            "set -g status off\nset -g destroy-unattached on\nset -g exit-empty on\n",
        )
        .expect("tmux fixture config");
        let socket = format!("hokann-test-{}", std::process::id());
        let mut command = CommandBuilder::new("tmux");
        command.arg("-L");
        command.arg(socket);
        command.arg("-f");
        command.arg(tmux_config);
        command.arg("new-session");
        command.arg("-x");
        command.arg("80");
        command.arg("-y");
        command.arg("24");
        command.arg(hokann_test_bin());
        command.arg("--shell");
        command.arg("zsh");
        configure_command(&mut command, &home, &work);
        Self::spawn_command(home, work, command, 2)
    }

    fn spawn_command(
        home: TempDir,
        work: TempDir,
        command: CommandBuilder,
        sync_status: u8,
    ) -> Self {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("outer PTY");
        let child = pair.slave.spawn_command(command).expect("hokann child");
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().expect("PTY reader");
        let writer = pair.master.take_writer().expect("PTY writer");
        let (sender, chunks) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = reader;
            let mut bytes = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        if sender.send(bytes[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                        ) =>
                    {
                        continue;
                    }
                    Err(error) if error.raw_os_error() == Some(5) => break,
                    Err(_) => break,
                }
            }
        });

        Self {
            _home: home,
            _work: work,
            master: Some(pair.master),
            writer: Some(writer),
            child,
            chunks,
            reader: Some(reader),
            terminal: vt100::Parser::new(24, 80, 0),
            rows: 24,
            cols: 80,
            transcript: Vec::new(),
            probe_tail: Vec::new(),
            sync_status,
            sync_replies: 0,
            cpr_replies: 0,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let writer = self.writer.as_mut().expect("PTY writer");
        writer.write_all(bytes).expect("write terminal input");
        writer.flush().expect("flush terminal input");
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .as_ref()
            .expect("PTY master")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize outer PTY");
        self.rows = rows;
        self.cols = cols;
        self.terminal.screen_mut().set_size(rows, cols);
    }

    fn process_chunk(&mut self, chunk: &[u8]) {
        self.transcript.extend_from_slice(chunk);
        for &byte in chunk {
            self.terminal.process(std::slice::from_ref(&byte));
            self.probe_tail.push(byte);
            if self.probe_tail.len() > SYNC_QUERY.len().max(CPR_QUERY.len()) {
                let trim = self.probe_tail.len() - SYNC_QUERY.len().max(CPR_QUERY.len());
                self.probe_tail.drain(..trim);
            }
            if self.probe_tail.ends_with(SYNC_QUERY) {
                let reply = format!("\x1b[?2026;{}$y", self.sync_status);
                self.write(reply.as_bytes());
                self.sync_replies += 1;
                self.probe_tail.clear();
            } else if self.probe_tail.ends_with(CPR_QUERY) {
                let (row, col) = self.terminal.screen().cursor_position();
                let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                self.write(reply.as_bytes());
                self.cpr_replies += 1;
                self.probe_tail.clear();
            }
        }
    }

    fn receive_once(&mut self, timeout: Duration) {
        if let Ok(chunk) = self.chunks.recv_timeout(timeout) {
            self.process_chunk(&chunk);
        }
    }

    fn wait_for_screen(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.screen_text().contains(needle) {
                return;
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "screen did not contain {needle:?}; screen=\n{}\ntranscript tail={:?}",
            self.screen_text(),
            tail(&self.transcript, 2_048)
        );
    }

    fn wait_for_bytes_since(&mut self, start: usize, needle: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.transcript[start..]
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return;
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "PTY output did not contain {:?}; transcript tail={:?}",
            needle,
            tail(&self.transcript, 2_048)
        );
    }

    fn settle(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            self.receive_once(READ_POLL);
        }
    }

    fn screen_text(&self) -> String {
        let mut text = String::new();
        for row in 0..self.rows {
            for col in 0..self.cols {
                if let Some(cell) = self.terminal.screen().cell(row, col) {
                    text.push_str(cell.contents());
                } else {
                    text.push(' ');
                }
            }
            text.push('\n');
        }
        text
    }

    fn pid(&self) -> i32 {
        i32::try_from(self.child.process_id().expect("hokann pid")).expect("pid fits")
    }

    fn try_wait(&mut self) -> Option<portable_pty::ExitStatus> {
        self.child.try_wait().expect("child status")
    }

    fn wait_until_exit(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline && self.try_wait().is_none() {
            self.receive_once(READ_POLL);
        }
        assert!(
            self.try_wait().is_some(),
            "PTY child did not exit; transcript tail={:?}",
            tail(&self.transcript, 2_048)
        );
        self.settle(Duration::from_millis(50));
    }

    fn exit_shell(&mut self) {
        self.write(b"\x15exit\r");
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.try_wait().is_none() {
            let _ = self.child.kill();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && self.try_wait().is_none() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        self.writer.take();
        self.master.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn real_session_keeps_overlay_and_terminal_lifecycle_stable() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    assert!(terminal.sync_replies >= 1, "DECRQM probe was not answered");

    terminal.write(b"ls");
    terminal.wait_for_screen("[SPEC]");
    assert!(terminal.screen_text().contains("HK> ls"));

    terminal.write(b"\x1b[B\x1b[A\x1b[6~\x1b[5~");
    terminal.settle(Duration::from_millis(100));
    assert!(terminal.screen_text().contains("HK> ls"));
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.write(b"\x0c");
    terminal.wait_for_screen("HK> ls");
    let before_resize = terminal.transcript.len();
    terminal.resize(30, 100);
    terminal.wait_for_bytes_since(before_resize, b"HK> ls");
    terminal.wait_for_screen("[SPEC]");

    let pid = terminal.pid();
    signal::kill(Pid::from_raw(pid), Signal::SIGTSTP).expect("suspend hokann");
    thread::sleep(Duration::from_millis(100));
    signal::kill(Pid::from_raw(pid), Signal::SIGCONT).expect("continue hokann");
    terminal.wait_for_screen("HK> ls");
    assert!(terminal.cpr_replies >= 1, "CPR anchor was not probed");

    terminal.write(b"\x03");
    terminal.settle(Duration::from_millis(100));
    terminal.write("printf '中🙂'".as_bytes());
    terminal.settle(Duration::from_millis(100));
    assert!(terminal.screen_text().contains("中🙂"));
    terminal.write(b"\x03");
    terminal.settle(Duration::from_millis(100));

    let fixture = terminal._work.path().join("alternate.sh");
    write_alternate_fixture(&fixture);
    terminal.write(b"sh ./alternate.sh");
    terminal.wait_for_screen("[FILE]");
    terminal.write(b"\x1b");
    terminal.settle(Duration::from_millis(80));
    terminal.write(b"\r");
    let alternate_start = terminal.transcript.len();
    terminal.wait_for_bytes_since(alternate_start, b"ALT_READY");
    terminal.write(b"ok\r");
    terminal.wait_for_bytes_since(alternate_start, b"ALT_KEY=ok");
    terminal.wait_for_bytes_since(alternate_start, b"\x1b[?1049l");
    let alternate_end = terminal.transcript.len();
    let alternate_output = &terminal.transcript[alternate_start..alternate_end];
    assert!(
        !alternate_output
            .windows(6)
            .any(|window| window == b"[SPEC]")
    );
    terminal.wait_for_screen("HK> ");

    terminal.exit_shell();
    terminal.wait_until_exit();
    assert!(
        terminal.transcript.ends_with(RESTORE_PRESENTATION),
        "terminal presentation was not restored; tail={:?}",
        tail(&terminal.transcript, 256)
    );
}

#[test]
fn real_session_uses_non_destructive_fallback_when_mode_2026_is_unavailable() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_sync_status(0);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    assert_eq!(terminal.sync_replies, 1);

    terminal.write(b"ls");
    terminal.wait_for_screen("[SPEC]");
    terminal.write(b"\x1b[B\x1b[A");
    terminal.settle(Duration::from_millis(100));
    assert!(terminal.screen_text().contains("HK> ls"));
    assert!(
        !terminal
            .transcript
            .windows(b"\x1b[?2026h".len())
            .any(|window| window == b"\x1b[?2026h"),
        "fallback session unexpectedly began a synchronized update"
    );
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn real_session_keeps_overlay_when_precmd_rewrites_prompt() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_dynamic_prompt();
    terminal.wait_for_screen("DYN> ");
    terminal.write(b"ls");
    terminal.wait_for_screen("[SPEC]");
    assert!(terminal.screen_text().contains("DYN> ls"));
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn real_session_follows_zdotdir_set_by_user_zshenv() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_user_zdotdir();
    terminal.wait_for_screen("ZDOT> ");
    terminal.write(b"ls");
    terminal.wait_for_screen("[SPEC]");
    assert!(terminal.screen_text().contains("ZDOT> ls"));
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn zsh_setup_auto_starts_hokann_and_restores_the_terminal() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_via_zsh_setup();
    terminal.wait_for_screen("HK1> ");
    terminal.write(b"ls");
    terminal.wait_for_screen("[SPEC]");
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
        terminal.write(b"ls");
        terminal.wait_for_screen("[SPEC]");

        signal::kill(Pid::from_raw(terminal.pid()), signal).expect("signal hokann");
        terminal.wait_until_exit();
        assert!(
            terminal.transcript.ends_with(RESTORE_PRESENTATION),
            "terminal presentation was not restored after {signal:?}; tail={:?}",
            tail(&terminal.transcript, 256)
        );
    }
}

#[test]
fn tmux_36_uses_fallback_even_when_the_outer_terminal_supports_mode_2026() {
    if !command_exists("zsh") || !tmux_is_36() {
        return;
    }
    let mut terminal = TerminalSession::spawn_in_tmux_36();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(500));
    terminal.write(b"ls");
    terminal.wait_for_screen("[SPEC]");
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

    terminal.exit_shell();
    terminal.wait_until_exit();
}

fn fixture_directories() -> (TempDir, TempDir) {
    let (home, work) = empty_fixture_directories();
    fs::write(
        home.path().join(".zshrc"),
        "PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\n",
    )
    .expect("fixture zshrc");
    (home, work)
}

fn empty_fixture_directories() -> (TempDir, TempDir) {
    let home = tempfile::tempdir().expect("temporary HOME");
    let work = tempfile::tempdir().expect("temporary CWD");
    fs::create_dir_all(home.path().join(".config/hokann")).expect("config directory");
    fs::create_dir_all(home.path().join(".local/state/hokann")).expect("state directory");
    fs::create_dir_all(home.path().join(".cache/hokann")).expect("cache directory");
    (home, work)
}

fn configure_command(command: &mut CommandBuilder, home: &TempDir, work: &TempDir) {
    command.env_remove("HOKANN_ACTIVE");
    command.env_remove("HOKANN_AUTO_START");
    command.env_remove("HOKANN_BIN");
    command.env_remove("ZSH_EXECUTION_STRING");
    command.env("HOME", home.path());
    command.env("SHELL", "/bin/zsh");
    command.env("TERM", "xterm-256color");
    command.env("LANG", "en_US.UTF-8");
    command.env("LC_CTYPE", "en_US.UTF-8");
    command.env("XDG_CONFIG_HOME", home.path().join(".config"));
    command.env("XDG_STATE_HOME", home.path().join(".local/state"));
    command.env("XDG_CACHE_HOME", home.path().join(".cache"));
    command.env("NO_COLOR", "1");
    command.cwd(work.path());
}

fn write_alternate_fixture(path: &Path) {
    fs::write(
        path,
        "#!/bin/sh\nprintf '\\033[?1049hALT_READY\\r\\n'\nIFS= read -r key\nprintf 'ALT_KEY=%s\\r\\n' \"$key\"\nprintf '\\033[?1049l'\n",
    )
    .expect("alternate fixture");
    let mut permissions = fs::metadata(path).expect("fixture metadata").permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).expect("fixture executable");
    }
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(name).is_file())
    })
}

fn hokann_test_bin() -> PathBuf {
    std::env::var_os("HOKANN_TEST_BIN_OVERRIDE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hokann")))
}

fn tmux_is_36() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|version| version.trim().starts_with("tmux 3.6"))
}

fn assert_forbidden_overlay_sequences_absent(bytes: &[u8]) {
    for forbidden in [
        b"\x1b[2J".as_slice(),
        b"\x1b[3J".as_slice(),
        b"\x1b[?1049h".as_slice(),
        b"\x1b[?1049l".as_slice(),
        b"\x1b7".as_slice(),
        b"\x1b8".as_slice(),
    ] {
        assert!(
            !bytes
                .windows(forbidden.len())
                .any(|window| window == forbidden),
            "forbidden overlay sequence in transcript: {forbidden:?}"
        );
    }
}

fn tail(bytes: &[u8], max: usize) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(max)..]).into_owned()
}
