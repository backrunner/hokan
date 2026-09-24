#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nix::{
    sys::{
        signal,
        signal::Signal,
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
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
const HOKAN_CPR_QUERY: &[u8] = b"\x1b[?6n";
const STATUS_QUERY: &[u8] = b"\x1b[5n";
const STANDARD_CPR_QUERY: &[u8] = b"\x1b[6n";
const KITTY_KEYBOARD_QUERY: &[u8] = b"\x1b[?u";
const DEVICE_ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
const RESTORE_PRESENTATION: &[u8] = b"\x1b[?2004l\x1b[0m\x1b[?25h";
static TMUX_SOCKET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    private_cpr_supported: bool,
    sync_replies: usize,
    cpr_replies: usize,
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

fn check_cd_completion_key(preference: Option<&str>, key: &[u8], executes: bool) {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = empty_fixture_directories();
    fs::write(
        home.path().join(".zshrc"),
        "PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\n",
    )
    .expect("zshrc");
    fs::create_dir_all(work.path().join("chosen folder/child-dir")).expect("directory tree");
    if let Some(preference) = preference {
        fs::write(
            home.path().join(".config/hokan/config.toml"),
            format!("[completion]\ncd_enter_behavior = \"{preference}\"\n"),
        )
        .expect("cd preference");
    }
    let original_name = work
        .path()
        .file_name()
        .expect("work basename")
        .to_str()
        .expect("UTF-8 basename")
        .to_owned();
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_sync_replies(1);
    terminal.wait_for_screen("HK> ");
    terminal.write(b"cd cho");
    terminal.wait_for_screen("chosen folder/");
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");
    terminal.write(key);
    if executes {
        terminal.wait_for_bare_row("HK>");
    } else {
        terminal.wait_for_screen("child-dir/");
        terminal.write(b"\x15");
        terminal.wait_for_bare_row("HK>");
    }
    // Read the shell's actual cwd; a filled command line alone is not proof
    // that Enter ran cd (or that Tab/continue left the cwd unchanged).
    terminal.write(b"print -r -- $PWD:t\r");
    terminal.wait_for_bare_row(if executes {
        "chosen folder"
    } else {
        &original_name
    });
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
    fs::write(home.path().join(".zshenv"), "unsetopt GLOBAL_RCS\n").expect("fixture zshenv");
    fs::create_dir_all(home.path().join(".config/hokan")).expect("config directory");
    fs::create_dir_all(home.path().join(".local/state/hokan")).expect("state directory");
    fs::create_dir_all(home.path().join(".cache/hokan")).expect("cache directory");
    (home, work)
}

struct SshServer {
    _directory: TempDir,
    child: Option<std::process::Child>,
    identity_key: PathBuf,
    port: u16,
    username: String,
}

impl SshServer {
    fn start() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("hokan-ssh-")
            .tempdir()
            .expect("SSH fixture directory");
        let host_key = directory.path().join("host_ed25519");
        let identity_key = directory.path().join("client_ed25519");
        generate_ssh_key(&host_key, "host key");
        generate_ssh_key(&identity_key, "client key");
        let public_key =
            fs::read_to_string(identity_key.with_extension("pub")).expect("client public key");
        let authorized_keys = directory.path().join("authorized_keys");
        fs::write(&authorized_keys, public_key).expect("authorized keys");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&authorized_keys, fs::Permissions::from_mode(0o600))
                .expect("authorized keys permissions");
        }

        let port = TcpListener::bind(("127.0.0.1", 0))
            .expect("reserve SSH port")
            .local_addr()
            .expect("SSH listener address")
            .port();
        let pid_file = directory.path().join("sshd.pid");
        let config = directory.path().join("sshd_config");
        let username = std::env::var("USER").expect("USER is required for SSH fixture");
        let config_text = format!(
            "Port {port}\nListenAddress 127.0.0.1\nAddressFamily inet\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nPermitRootLogin no\nPermitTTY yes\nX11Forwarding no\nAllowTcpForwarding no\nPrintMotd no\nUseDNS no\nLogLevel ERROR\n",
            host_key.display(),
            pid_file.display(),
            authorized_keys.display(),
        );
        fs::write(&config, config_text).expect("sshd config");

        let mut child = Command::new(command_path("sshd"))
            .arg("-D")
            .arg("-e")
            .arg("-f")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start sshd");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = child.try_wait().expect("poll sshd") {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("sshd exited before becoming ready ({status}); stderr={stderr}");
            }
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "sshd did not become ready");
            thread::sleep(Duration::from_millis(20));
        }

        Self {
            _directory: directory,
            child: Some(child),
            identity_key,
            port,
            username,
        }
    }
}

impl Drop for SshServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn generate_ssh_key(path: &Path, description: &str) {
    let output = Command::new(command_path("ssh-keygen"))
        .args(["-q", "-t", "ed25519", "-N", ""])
        .arg("-f")
        .arg(path)
        .output()
        .unwrap_or_else(|error| panic!("generate {description}: {error}"));
    assert!(
        output.status.success(),
        "generate {description} failed: stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn shell_quote(path: &Path) -> String {
    shell_quote_text(&path.to_string_lossy())
}

fn shell_quote_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn command_path(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} was not found in PATH"))
}

fn configure_command(command: &mut CommandBuilder, home: &TempDir, work: &TempDir) {
    // Terminal fixtures must never contact GitHub or replace their test binary.
    // Update behavior has dedicated loopback tests.
    command.env("HOKAN_NO_AUTO_UPDATE", "1");
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

fn wait_until_stopped(pid: i32) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match waitpid(
            Pid::from_raw(pid),
            Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG),
        ) {
            Ok(WaitStatus::Stopped(_, _)) => return,
            Ok(WaitStatus::StillAlive | WaitStatus::Continued(_)) => {}
            Ok(status) => panic!("Hokan exited instead of suspending: {status:?}"),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => panic!("failed to observe suspended Hokan process: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "Hokan did not enter stopped state"
        );
        thread::sleep(READ_POLL);
    }
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

fn head(bytes: &[u8], max: usize) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(max)]).into_owned()
}

mod activation;
mod candidates;
mod foreground;
mod input;

mod lifecycle;
mod navigation;
mod rendering;
mod ssh;
mod themes;
mod titles;
mod tmux;

mod io;
mod spawn;
