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

// Generous budget: under full-suite parallel load (many concurrent real-PTY
// tests) the 10s default was occasionally exhausted even though every test
// passes in isolation.
const TIMEOUT: Duration = Duration::from_secs(30);
const READ_POLL: Duration = Duration::from_millis(5);
const SYNC_QUERY: &[u8] = b"\x1b[?2026$p";
const CPR_QUERY: &[u8] = b"\x1b[6n";
const RESTORE_PRESENTATION: &[u8] = b"\x18\x1b[0m\x1b[?25h";

/// Source tag glyphs as rendered with the default `nerd_fonts = true`.
const TAG_SPEC: &str = "\u{f02d}";
const TAG_HELP: &str = "\u{f059}";
const TAG_HIS: &str = "\u{f1da}";
const TAG_FILE: &str = "\u{f15b}";
const TAG_EXEC: &str = "\u{f071}";

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
        Self::spawn_hokan(home, work, sync_status)
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
        Self::spawn_hokan(home, work, 2)
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
        Self::spawn_hokan(home, work, 2)
    }

    fn spawn_with_pua_prompt() -> Self {
        let (home, work) = empty_fixture_directories();
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT=$'\u{e0b0}\u{f000} HK> '\nRPROMPT=''\nsetopt no_beep\n",
        )
        .expect("PUA prompt fixture");
        Self::spawn_hokan(home, work, 2)
    }

    fn spawn_with_rotating_rprompt() -> Self {
        let (home, work) = empty_fixture_directories();
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT='HK> '\nRPROMPT='RIGHT1'\nsetopt no_beep\n\
             setopt transient_rprompt\n\
             typeset -gi FIXTURE_RPROMPT_N=1\n\
             autoload -Uz add-zsh-hook\n\
             function fixture_rotate_rprompt() {\n\
             \x20 (( FIXTURE_RPROMPT_N++ ))\n\
             \x20 RPROMPT=\"RIGHT${FIXTURE_RPROMPT_N}\"\n\
             }\n\
             add-zsh-hook precmd fixture_rotate_rprompt\n",
        )
        .expect("rotating RPROMPT fixture");
        Self::spawn_hokan(home, work, 2)
    }

    fn spawn_with_multiline_prompt() -> Self {
        let (home, work) = empty_fixture_directories();
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT=$'META\\nHK> '\nRPROMPT=''\nsetopt no_beep\n",
        )
        .expect("multiline prompt fixture");
        Self::spawn_hokan(home, work, 2)
    }

    fn spawn_with_instant_prompt_churn() -> Self {
        let (home, work) = empty_fixture_directories();
        // Emulates p10k instant prompt: a cached prompt block is printed and
        // then erased at the very top of .zshrc, exactly where p10k runs it,
        // before hokan's init hook is sourced.
        fs::write(
            home.path().join(".zshrc"),
            "print -r -- 'CACHED> '\n\
             print -r -- 'cached segment row'\n\
             print -rn -- $'\\e[2A\\r\\e[J'\n\
             PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\n",
        )
        .expect("instant prompt fixture");
        Self::spawn_hokan(home, work, 2)
    }

    fn spawn_with_transient_prompt() -> Self {
        let (home, work) = empty_fixture_directories();
        // Emulates p10k transient prompt: on preexec the accepted line is
        // rewritten to a shorter prompt while the command runs.
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\n\
             autoload -Uz add-zsh-hook\n\
             function fixture_transient_prompt() {\n\
             \x20 print -rn -- $'\\e[1A\\r\\e[KTR> '\n\
             \x20 print -r -- \"$1\"\n\
             }\n\
             add-zsh-hook preexec fixture_transient_prompt\n",
        )
        .expect("transient prompt fixture");
        Self::spawn_hokan(home, work, 2)
    }

    fn spawn_via_zsh_setup() -> Self {
        let (home, work) = fixture_directories();
        let rc_path = home.path().join(".zshrc");
        fs::write(
            &rc_path,
            "typeset -gi HOKAN_RC_LOADS=${HOKAN_RC_LOADS:-0}\n\
             (( HOKAN_RC_LOADS++ ))\n\
             export HOKAN_RC_LOADS\n\
             PROMPT=\"HK${HOKAN_RC_LOADS}> \"\n\
             RPROMPT=''\n\
             setopt no_beep\n",
        )
        .expect("auto-start zshrc fixture");
        let output = Command::new(hokan_test_bin())
            .arg("--shell")
            .arg("zsh")
            .arg("setup")
            .arg("--rc-file")
            .arg(&rc_path)
            .env("HOME", home.path())
            .env_remove("HOKAN_ACTIVE")
            .env_remove("HOKAN_BIN")
            .output()
            .expect("run hokan setup");
        assert!(
            output.status.success(),
            "setup failed: stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let installed = fs::read_to_string(&rc_path).expect("installed zshrc");
        assert!(installed.contains("HOKAN_AUTO_START"));

        let mut command = CommandBuilder::new("zsh");
        command.arg("-i");
        configure_command(&mut command, &home, &work);
        Self::spawn_command(home, work, command, 2)
    }

    fn spawn_hokan(home: TempDir, work: TempDir, sync_status: u8) -> Self {
        let mut command = CommandBuilder::new(hokan_test_bin());
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
             \"$HOKAN_TEST_BIN\" --shell zsh\n\
             status=$?\n\
             after=$(stty -g) || exit 91\n\
             if [ \"$before\" = \"$after\" ]; then restored=yes; else restored=no; fi\n\
             printf '\\r\\nHK_TERMIO:%s:EXIT=%s\\r\\n' \"$restored\" \"$status\"",
        );
        command.env("HOKAN_TEST_BIN", hokan_test_bin());
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
        let socket = format!("hokan-test-{}", std::process::id());
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
        command.arg(hokan_test_bin());
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
        let child = pair.slave.spawn_command(command).expect("hokan child");
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

    fn wait_for_bare_row(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.screen_text().lines().any(|line| line.trim() == needle) {
                return;
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "screen had no bare row {needle:?}; screen=\n{}\ntranscript tail={:?}",
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

    /// Waits until the edit line is present AND every border glyph on screen
    /// belongs to exactly one rectangular overlay box. Retries ride out
    /// mid-paint transients; a persistent smear never satisfies this.
    fn wait_for_clean_overlay(&mut self, edit_line: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.screen_text().contains(edit_line) {
                let strays = self.border_strays();
                if strays.is_empty() {
                    return;
                }
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "overlay did not settle cleanly for {edit_line:?}; strays={:?}; screen=\n{}\ntranscript tail={:?}",
            self.border_strays(),
            self.screen_text(),
            tail(&self.transcript, 2_048)
        );
    }

    /// Describes every border glyph that cannot belong to a single current
    /// overlay box. An empty result means the screen holds exactly one box:
    /// one top edge, one bottom edge below it, matching corners, and side
    /// pipes only on the box's left/right columns between the edges.
    fn border_strays(&self) -> Vec<String> {
        let screen = self.terminal.screen();
        let mut tops = Vec::new();
        let mut top_rights = Vec::new();
        let mut bottoms = Vec::new();
        let mut bottom_rights = Vec::new();
        let mut sides = Vec::new();
        for row in 0..self.rows {
            for col in 0..self.cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                match cell.contents() {
                    "╭" => tops.push((row, col)),
                    "╮" => top_rights.push((row, col)),
                    "╰" => bottoms.push((row, col)),
                    "╯" => bottom_rights.push((row, col)),
                    "│" => sides.push((row, col)),
                    _ => {}
                }
            }
        }
        let mut strays = Vec::new();
        if tops.len() != 1
            || top_rights.len() != 1
            || bottoms.len() != 1
            || bottom_rights.len() != 1
        {
            strays.push(format!(
                "corner count ╭={} ╮={} ╰={} ╯={} (tops={tops:?} top_rights={top_rights:?} bottoms={bottoms:?} bottom_rights={bottom_rights:?} sides={sides:?})",
                tops.len(),
                top_rights.len(),
                bottoms.len(),
                bottom_rights.len()
            ));
            return strays;
        }
        let (top, left) = tops[0];
        let (bottom, _) = bottoms[0];
        let right = top_rights[0].1;
        if top_rights[0].0 != top {
            strays.push(format!(
                "top-right corner at {:?}, expected row {top}",
                top_rights[0]
            ));
        }
        if bottoms[0].1 != left {
            strays.push(format!(
                "bottom-left corner at {:?}, expected ({bottom}, {left})",
                bottoms[0]
            ));
        }
        if bottom_rights[0] != (bottom, right) {
            strays.push(format!(
                "bottom-right corner at {:?}, expected ({bottom}, {right})",
                bottom_rights[0]
            ));
        }
        if bottom <= top + 1 {
            strays.push(format!("box edges too close: top={top} bottom={bottom}"));
        }
        for (row, col) in sides {
            if row <= top || row >= bottom || (col != left && col != right) {
                strays.push(format!("stray side border at ({row}, {col})"));
            }
        }
        strays
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

    fn screen_rows_containing(&self, needle: &str) -> Vec<u16> {
        self.screen_text()
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(needle))
            .map(|(index, _)| index as u16)
            .collect()
    }

    fn screen_line(&self, row: u16) -> String {
        self.screen_text()
            .lines()
            .nth(row as usize)
            .expect("row index within screen")
            .to_owned()
    }

    fn pid(&self) -> i32 {
        i32::try_from(self.child.process_id().expect("hokan pid")).expect("pid fits")
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

    terminal.write(b"ls ");
    // Wait for the hint footer (the bottom edge of the box) rather than the
    // first SPEC label: early frames show item rows before the full box —
    // including its bottom edge — has been painted.
    terminal.wait_for_screen("Tab 回填 · Enter 执行 · Esc 关闭");
    assert!(terminal.screen_text().contains("HK> ls"));
    let text = terminal.screen_text();
    assert!(text.contains('╭'), "overlay top border missing:\n{text}");
    assert!(
        text.contains(TAG_SPEC),
        "overlay spec rows missing:\n{text}"
    );

    terminal.write(b"\x1b[B\x1b[A\x1b[6~\x1b[5~");
    terminal.settle(Duration::from_millis(100));
    assert!(terminal.screen_text().contains("HK> ls"));
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.write(b"\x0c");
    terminal.wait_for_screen("HK> ls");
    let before_resize = terminal.transcript.len();
    terminal.resize(30, 100);
    terminal.wait_for_bytes_since(before_resize, b"HK> ls");
    terminal.wait_for_screen(TAG_SPEC);

    let pid = terminal.pid();
    signal::kill(Pid::from_raw(pid), Signal::SIGTSTP).expect("suspend hokan");
    thread::sleep(Duration::from_millis(100));
    signal::kill(Pid::from_raw(pid), Signal::SIGCONT).expect("continue hokan");
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
    // Stop one character short of the full path: a candidate that would
    // rewrite the buffer to itself is filtered out, so the FILE row only
    // appears while the typed text is still a proper prefix.
    terminal.write(b"sh ./alternate.s");
    terminal.wait_for_screen(TAG_FILE);
    terminal.write(b"\x1b");
    terminal.settle(Duration::from_millis(80));
    terminal.write(b"h");
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
            .windows(TAG_SPEC.len())
            .any(|window| window == TAG_SPEC.as_bytes())
    );
    assert!(
        !alternate_output
            .windows("│".len())
            .any(|window| window == "│".as_bytes())
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
        "fallback session unexpectedly began a synchronized update"
    );
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn man_derived_help_rows_appear_and_file_rows_follow_help_suppression() {
    if !command_exists("zsh") || !command_exists("man") {
        return;
    }
    let (home, work) = fixture_directories();
    fs::write(work.path().join("help-target.txt"), b"fixture").expect("work file");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(100));

    // `cp -`: `cp` has no spec coverage, so the man-derived flag list shows
    // HELP rows; the dashed active word suppresses FILE rows entirely.
    terminal.write(b"cp -");
    terminal.wait_for_screen(TAG_HELP);
    let text = terminal.screen_text();
    assert!(text.contains(TAG_HELP), "HELP rows missing:\n{text}");
    assert!(
        !text.contains(TAG_FILE),
        "FILE rows leaked into the flag position:\n{text}"
    );

    // `cp ` (no dash; the cp man page documents no subcommands): file
    // completion still works.
    terminal.write(b"\x03");
    terminal.wait_for_screen("HK> ");
    terminal.write(b"cp ");
    terminal.wait_for_screen(TAG_FILE);
    let text = terminal.screen_text();
    assert!(
        text.contains("help-target.txt"),
        "file row missing after `cp `:\n{text}"
    );

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
    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
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
    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("ZDOT> ls"));
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn powerline_pua_glyphs_pass_through_and_overlay_still_anchors() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_pua_prompt();
    terminal.wait_for_screen("\u{e0b0}\u{f000} HK> ");
    terminal.settle(Duration::from_millis(300));
    let glyphs = "\u{e0b0}\u{f000}".as_bytes();
    assert!(
        terminal
            .transcript
            .windows(glyphs.len())
            .any(|window| window == glyphs),
        "Nerd Font PUA glyphs were not passed through byte-identically"
    );

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("\u{e0b0}\u{f000} HK> ls"));
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn rotating_rprompt_redraws_stay_consistent_and_overlay_still_anchors() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_rotating_rprompt();
    terminal.wait_for_screen("RIGHT2");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"true\r");
    terminal.wait_for_screen("RIGHT3");
    terminal.settle(Duration::from_millis(300));
    let text = terminal.screen_text();
    assert!(
        !text.contains("RIGHT2"),
        "stale RPROMPT survived a redraw:\n{text}"
    );
    let prompt_row = *terminal
        .screen_rows_containing("HK> ")
        .last()
        .expect("current prompt row");
    let rprompt_row = *terminal
        .screen_rows_containing("RIGHT3")
        .last()
        .expect("current RPROMPT row");
    assert_eq!(
        rprompt_row, prompt_row,
        "RPROMPT must stay on the current prompt row"
    );
    assert!(
        terminal
            .screen_line(prompt_row)
            .trim_end()
            .ends_with("RIGHT3"),
        "RPROMPT must stay right-aligned on the current prompt row:\n{}",
        terminal.screen_text()
    );

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("HK> ls"));
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn multiline_prompt_anchors_overlay_below_the_edit_line() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_multiline_prompt();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    assert!(terminal.screen_text().contains("META"));

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    let edit_row = *terminal
        .screen_rows_containing("HK> ls")
        .last()
        .expect("edit line row");
    let meta_row = *terminal
        .screen_rows_containing("META")
        .last()
        .expect("first prompt line row");
    assert_eq!(
        meta_row + 1,
        edit_row,
        "META must sit directly above the edit line"
    );
    let overlay_row = edit_row + 1;
    assert!(
        terminal.screen_line(overlay_row).contains('╭'),
        "overlay top border must anchor below the edit line, not below the first prompt line:\n{}",
        terminal.screen_text()
    );
    assert!(
        terminal.screen_line(overlay_row + 1).contains(TAG_SPEC),
        "overlay items must sit below the top border:\n{}",
        terminal.screen_text()
    );
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn instant_prompt_churn_passes_through_and_overlay_works_at_first_prompt() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_instant_prompt_churn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    assert!(
        terminal
            .transcript
            .windows(b"cached segment row".len())
            .any(|window| window == b"cached segment row"),
        "instant prompt churn did not pass through to the outer terminal"
    );
    let text = terminal.screen_text();
    assert!(
        !text.contains("CACHED") && !text.contains("cached segment"),
        "cached prompt block was not erased before the real prompt:\n{text}"
    );

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("HK> ls"));
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn transient_prompt_rewrite_reanchors_overlay_on_return_to_prompt() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_transient_prompt();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"true\r");
    terminal.wait_for_screen("TR> true");
    terminal.settle(Duration::from_millis(300));
    let transient_row = *terminal
        .screen_rows_containing("TR> true")
        .last()
        .expect("transient prompt row");
    let prompt_row = *terminal
        .screen_rows_containing("HK> ")
        .last()
        .expect("fresh prompt row");
    assert!(
        prompt_row > transient_row,
        "a fresh prompt must follow the transient rewrite:\n{}",
        terminal.screen_text()
    );

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    let edit_row = *terminal
        .screen_rows_containing("HK> ls")
        .last()
        .expect("edit line row");
    let overlay_row = edit_row + 1;
    assert!(
        terminal.screen_line(overlay_row).contains('╭'),
        "overlay top border must re-anchor below the new edit line:\n{}",
        terminal.screen_text()
    );
    assert!(
        terminal.screen_line(overlay_row + 1).contains(TAG_SPEC),
        "overlay items must sit below the top border:\n{}",
        terminal.screen_text()
    );
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn zsh_setup_auto_starts_hokan_and_restores_the_terminal() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_via_zsh_setup();
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

#[test]
fn tmux_36_uses_fallback_even_when_the_outer_terminal_supports_mode_2026() {
    if !command_exists("zsh") || !tmux_is_36() {
        return;
    }
    let mut terminal = TerminalSession::spawn_in_tmux_36();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(500));
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

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn enter_executes_typed_command_with_one_press_while_overlay_is_open() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    // Seed hokan's history so the candidate list opens while the real command
    // is typed below.
    terminal.write(b"echo HI_DONE_SEED\r");
    terminal.wait_for_screen("HI_DONE_SEED");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"echo HI_DONE");
    terminal.wait_for_screen(TAG_HIS);
    assert!(terminal.screen_text().contains("HK> echo HI_DONE"));

    // ONE Enter must execute exactly what was typed — with no explicit
    // selection it never touches the candidate list or the buffer.
    terminal.write(b"\r");
    terminal.wait_for_bare_row("HI_DONE");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn enter_runs_ls_with_one_press_while_overlay_is_open() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    fs::write(terminal._work.path().join("HKLS_MARKER.txt"), b"marker\n").expect("marker file");
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    // ONE Enter runs the typed command: nothing is selected by default, so
    // Enter passes the buffer through to the shell unchanged.
    terminal.write(b"\r");
    terminal.wait_for_screen("HKLS_MARKER.txt");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn tab_fills_back_the_selected_candidate_without_executing() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"tar ");
    terminal.wait_for_screen(TAG_SPEC);
    // No row is pre-selected: Down selects the first candidate, then Tab —
    // the fill edit-back path — rewrites the buffer to the candidate text…
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");
    terminal.write(b"\t");
    terminal.wait_for_screen("HK> tar -czf");
    terminal.settle(Duration::from_millis(300));
    // …but nothing was executed — the line is still being edited.
    let text = terminal.screen_text();
    assert!(!text.contains("tar: "), "Tab must not execute:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn tab_without_a_selection_fills_the_top_candidate_and_selects_first_row() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"tar ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(
        !terminal.screen_text().contains('▶'),
        "no row may be pre-selected:\n{}",
        terminal.screen_text()
    );

    // Tab with no explicit selection fills the top-ranked candidate…
    terminal.write(b"\t");
    terminal.wait_for_screen("HK> tar -czf");
    // …and the refreshed list selects its first row automatically.
    terminal.wait_for_screen("▶");
    terminal.settle(Duration::from_millis(300));
    let text = terminal.screen_text();
    assert!(!text.contains("tar: "), "Tab must not execute:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn overlay_opens_without_a_default_selection() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"ls ");
    terminal.wait_for_screen("Tab 回填 · Enter 执行 · Esc 关闭");
    let text = terminal.screen_text();
    assert!(text.contains(TAG_SPEC), "overlay rows missing:\n{text}");
    assert!(!text.contains('▶'), "no row may be pre-selected:\n{text}");

    // Down selects the first row without ever touching the edit buffer.
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");
    let text = terminal.screen_text();
    assert!(text.contains("HK> ls"), "buffer changed:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn enter_executes_the_selected_history_candidate() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    // Seed a unique history entry whose OUTPUT differs from its command text
    // (lowercase) so execution is distinguishable from overlay echoes.
    terminal.write(b"echo HKSEL_HIDDEN | tr A-Z a-z\r");
    terminal.wait_for_bare_row("hksel_hidden");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"echo HKSEL_H");
    terminal.wait_for_screen(TAG_HIS);
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");

    // ONE Enter on the explicit selection executes the candidate outright.
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(start, b"hksel_hidden");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn enter_on_a_dangerous_candidate_requires_confirmation() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    terminal.write(b"rm -rf /tmp/hokan-danger-x && echo HKDANGER_X | tr A-Z a-z\r");
    terminal.wait_for_bare_row("hkdanger_x");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"rm -rf /tmp/hokan-dan");
    terminal.wait_for_screen(TAG_HIS);
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");

    // First Enter only opens the danger confirmation — nothing executes.
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_screen("Enter 确认执行 · Esc 取消");
    let text = terminal.screen_text();
    assert!(text.contains(TAG_EXEC), "EXEC row missing:\n{text}");
    assert!(
        text.contains("rm -rf /tmp/hokan-danger-x"),
        "final command missing from the EXEC row:\n{text}"
    );
    assert!(
        !terminal.transcript[start..]
            .windows(b"hkdanger_x".len())
            .any(|window| window == b"hkdanger_x"),
        "the dangerous command executed before confirmation"
    );

    // Second Enter proceeds with the execution.
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(start, b"hkdanger_x");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn escape_cancels_the_danger_confirmation() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    terminal.write(b"rm -rf /tmp/hokan-danger-y && echo HKCANCEL_Y | tr A-Z a-z\r");
    terminal.wait_for_bare_row("hkcancel_y");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"rm -rf /tmp/hokan-dan");
    terminal.wait_for_screen(TAG_HIS);
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_screen("Enter 确认执行 · Esc 取消");

    // Esc drops the confirmation and brings the normal candidate list back.
    terminal.write(b"\x1b");
    terminal.wait_for_screen(TAG_HIS);
    terminal.settle(Duration::from_millis(200));
    let text = terminal.screen_text();
    assert!(
        !text.contains("确认执行"),
        "confirmation hint survived Esc:\n{text}"
    );
    terminal.settle(Duration::from_millis(300));
    assert!(
        !terminal.transcript[start..]
            .windows(b"hkcancel_y".len())
            .any(|window| window == b"hkcancel_y"),
        "the dangerous command executed despite Esc"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn candidate_identical_to_the_typed_buffer_is_not_listed() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    terminal.write(b"echo HKIDENT_CMD\r");
    terminal.wait_for_bare_row("HKIDENT_CMD");
    terminal.settle(Duration::from_millis(300));

    // Retyping the exact seeded command: the history candidate would rewrite
    // the buffer to itself, so it is filtered out of the list.
    terminal.write(b"echo HKIDENT_CMD");
    terminal.settle(Duration::from_millis(500));
    let text = terminal.screen_text();
    assert!(
        text.contains("HK> echo HKIDENT_CMD"),
        "buffer missing:\n{text}"
    );
    assert!(
        !text.contains(TAG_HIS),
        "buffer-identical history candidate was listed:\n{text}"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn fresh_prompt_shows_no_overlay() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(500));
    assert_no_overlay_rows(&terminal);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn bare_executable_waits_for_space_before_suggesting() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // A bare executable word is already runnable: suggestions hold off until
    // the user commits to typing arguments with a space.
    terminal.write(b"ls");
    terminal.settle(Duration::from_millis(500));
    assert_no_overlay_rows(&terminal);

    terminal.write(b" ");
    terminal.wait_for_screen(TAG_SPEC);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn git_suggests_immediately_and_uses_repository_state() {
    if !command_exists("zsh") || !command_exists("git") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // `git` cannot run standalone, so suggestions appear without waiting for
    // a space — and outside a repository the top rows are init/clone, not
    // push/commit.
    terminal.write(b"git");
    terminal.wait_for_screen("git init");
    let text = terminal.screen_text();
    assert!(text.contains("git clone"), "clone row missing:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn deleting_to_an_empty_buffer_hides_the_overlay() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    terminal.write(b"\x7f\x7f\x7f");
    terminal.settle(Duration::from_millis(500));
    assert_no_overlay_rows(&terminal);
    let text = terminal.screen_text();
    assert!(
        text.contains("HK> "),
        "prompt must still be present:\n{text}"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn shift_tab_on_an_empty_buffer_does_not_open_the_overlay() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"\x1b[Z");
    terminal.settle(Duration::from_millis(500));
    assert_no_overlay_rows(&terminal);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn ctrl_r_on_an_empty_buffer_opens_the_history_view() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"echo HK_HIST_SEED\r");
    terminal.wait_for_screen("HK_HIST_SEED");
    terminal.settle(Duration::from_millis(300));
    // Explicit user intent: Ctrl-R focuses history even on an empty buffer.
    terminal.write(b"\x12");
    terminal.wait_for_screen(TAG_HIS);
    assert!(terminal.screen_text().contains("echo HK_HIST_SEED"));

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn wrapping_edit_line_survives_scroll_to_make_room_and_box_moves() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    // Seed a long history command: typing a proper prefix of it keeps a HIS
    // candidate (and therefore the overlay) open for the whole scenario.
    let seed =
        "echo HKWRAP_seed_aaaaaaaaaabbbbbbbbbbccccccccccddddddddddeeeeeeeeeeffffffffffgggggggggg";
    terminal.write(seed.as_bytes());
    terminal.write(b"\r");
    terminal.wait_for_screen("HKWRAP_seed_aaaaaaaaaa");
    terminal.settle(Duration::from_millis(300));

    // Fill the screen so the fresh prompt sits on the terminal's last row.
    terminal.write(b"seq 1 40\r");
    terminal.wait_for_bare_row("40");
    terminal.settle(Duration::from_millis(300));

    // Type a proper prefix long enough to wrap the 80-column edit line. The
    // overlay makes room by scrolling while the shell cursor is mid-screen,
    // and the box's left edge follows the cursor across the wrap.
    let typed = &seed[..78];
    assert!(typed.len() + "HK> ".len() > terminal.cols as usize);
    for chunk in typed.as_bytes().chunks(6) {
        terminal.write(chunk);
        terminal.settle(Duration::from_millis(25));
    }

    // (a) the edit-line start must survive the mid-screen scroll, and (b) no
    // stale border glyphs may remain outside the current overlay box.
    terminal.wait_for_clean_overlay("HK> echo HKWRAP_seed_");
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

fn assert_no_overlay_rows(terminal: &TerminalSession) {
    let text = terminal.screen_text();
    // The bordered overlay leaves unmistakable glyphs behind: rounded corners,
    // side pipes, and the footer hint text.
    for marker in ["╭", "╰", "│", "回填"] {
        assert!(
            !text.contains(marker),
            "unexpected overlay marker {marker}:\n{text}"
        );
    }
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
    fs::create_dir_all(home.path().join(".config/hokan")).expect("config directory");
    fs::create_dir_all(home.path().join(".local/state/hokan")).expect("state directory");
    fs::create_dir_all(home.path().join(".cache/hokan")).expect("cache directory");
    (home, work)
}

fn configure_command(command: &mut CommandBuilder, home: &TempDir, work: &TempDir) {
    command.env_remove("HOKAN_ACTIVE");
    command.env_remove("HOKAN_AUTO_START");
    command.env_remove("HOKAN_BIN");
    command.env_remove("ZSH_EXECUTION_STRING");
    // Keep fixtures hermetic: an exported outer ZDOTDIR would make the inner
    // shell resolve the user's real rc files instead of the fixture HOME.
    command.env_remove("ZDOTDIR");
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

fn hokan_test_bin() -> PathBuf {
    std::env::var_os("HOKAN_TEST_BIN_OVERRIDE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hokan")))
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
