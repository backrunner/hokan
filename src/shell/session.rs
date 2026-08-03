use std::{
    env, fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
};

use crossbeam_channel::Sender;
use nix::{
    fcntl::{OFlag, open},
    sys::stat::Mode,
    unistd::mkfifo,
};
use nix_compat::poll::{PollFd, PollFlags, poll};
use portable_pty::CommandBuilder;
use tempfile::TempDir;

use super::{ProtocolDiagnostic, ShellEvent, ShellKind, ShellProtocolDecoder, init_script};
use crate::terminal::{SessionToken, render_boundary::marker_checksum};

const EDIT_MAX_BYTES: usize = 64 * 1024;
const EDIT_PAYLOAD_MAX_BYTES: usize = EDIT_MAX_BYTES + 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlMessage {
    Event(ShellEvent),
    Diagnostic(ProtocolDiagnostic),
    BufferUncertain,
}

pub struct ShellSession {
    directory: TempDir,
    fifo_path: PathBuf,
    init_path: PathBuf,
    edit_path: PathBuf,
    token: SessionToken,
    shell: ShellKind,
}

impl ShellSession {
    pub fn new(shell: ShellKind) -> crate::Result<Self> {
        let directory = tempfile::Builder::new().prefix("hokan-").tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let fifo_path = directory.path().join("control.fifo");
        mkfifo(&fifo_path, Mode::S_IRUSR | Mode::S_IWUSR).map_err(nix_io)?;
        let init_path = directory.path().join(match shell {
            ShellKind::Zsh => "hokan.zsh",
            ShellKind::Bash => "hokan.bash",
            ShellKind::Fish => "hokan.fish",
        });
        write_private(&init_path, init_script(shell).as_bytes())?;
        let token = SessionToken::generate()?;
        Ok(Self {
            edit_path: directory.path().join("edit.next"),
            directory,
            fifo_path,
            init_path,
            token,
            shell,
        })
    }

    #[must_use]
    pub fn token(&self) -> SessionToken {
        self.token.clone()
    }

    #[must_use]
    pub fn shell(&self) -> ShellKind {
        self.shell
    }

    pub fn command_builder(&self, login: bool) -> crate::Result<CommandBuilder> {
        self.command_builder_with_user_config(login, true)
    }

    #[doc(hidden)]
    pub fn command_builder_isolated(&self, login: bool) -> crate::Result<CommandBuilder> {
        self.command_builder_with_user_config(login, false)
    }

    fn command_builder_with_user_config(
        &self,
        login: bool,
        source_user_config: bool,
    ) -> crate::Result<CommandBuilder> {
        let executable = configured_executable(self.shell);
        let mut command = CommandBuilder::new(executable);
        command.env("HOKAN_ACTIVE", "1");
        command.env("HOKAN_HOOK_OWNER_PID", "");
        command.env(
            "HOKAN_PROTOCOL_VERSION",
            super::PROTOCOL_VERSION.to_string(),
        );
        command.env("HOKAN_SESSION_TOKEN", self.token.as_str());
        command.env("HOKAN_SESSION_DIR", self.directory.path());
        command.env("HOKAN_CONTROL_FIFO", &self.fifo_path);
        command.env(
            "HOKAN_PROMPT_CRC",
            format!("{:08x}", marker_checksum(&self.token, "prompt")),
        );
        command.env(
            "HOKAN_REDISPLAY_CRC",
            format!("{:08x}", marker_checksum(&self.token, "redisplay")),
        );
        command.env("HOKAN_BIN", env::current_exe()?);
        command.cwd(env::current_dir()?);

        match self.shell {
            ShellKind::Zsh => self.configure_zsh(&mut command, login, source_user_config)?,
            ShellKind::Bash => self.configure_bash(&mut command, login, source_user_config)?,
            ShellKind::Fish => self.configure_fish(&mut command, login, source_user_config),
        }
        Ok(command)
    }

    pub fn write_edit(&self, text: &str, cursor: usize) -> crate::Result<()> {
        if text.len() > EDIT_MAX_BYTES || text.chars().any(char::is_control) {
            return Err(crate::Error::Shell(
                "replacement must be a control-free single line no larger than 64 KiB".into(),
            ));
        }
        if cursor > text.len() || !text.is_char_boundary(cursor) {
            return Err(crate::Error::Shell(
                "replacement cursor is not a valid UTF-8 boundary".into(),
            ));
        }
        let cursor = text[..cursor].chars().count();
        let payload = format!("{cursor}\t{text}");
        let temporary = self.directory.path().join("edit.pending");
        write_private(&temporary, payload.as_bytes())?;
        fs::rename(temporary, &self.edit_path)?;
        Ok(())
    }

    pub fn start_control_reader(
        &self,
        sender: Sender<ControlMessage>,
    ) -> crate::Result<ControlReader> {
        ControlReader::start(self.fifo_path.clone(), self.shell, sender)
    }

    fn configure_zsh(
        &self,
        command: &mut CommandBuilder,
        login: bool,
        source_user_config: bool,
    ) -> crate::Result<()> {
        let zdotdir = self.directory.path().join("zsh");
        fs::create_dir(&zdotdir)?;
        let original = env::var_os("ZDOTDIR")
            .map(PathBuf::from)
            .or_else(home_directory)
            .ok_or_else(|| crate::Error::Shell("cannot locate the zsh config directory".into()))?;
        write_private(
            &zdotdir.join(".zshenv"),
            zsh_environment_loader(&original, &zdotdir, source_user_config).as_bytes(),
        )?;
        let mut zshrc = zsh_startup_loader(".zshrc", &zdotdir, source_user_config, false);
        zshrc.push_str(&source_if_readable(&self.init_path, "source"));
        zshrc.push_str(
            "if [[ ! -o login ]]; then\n  export ZDOTDIR=\"$__hokan_user_zdotdir\"\nfi\n",
        );
        write_private(&zdotdir.join(".zshrc"), zshrc.as_bytes())?;
        if login {
            write_private(
                &zdotdir.join(".zprofile"),
                zsh_startup_loader(".zprofile", &zdotdir, source_user_config, false).as_bytes(),
            )?;
            write_private(
                &zdotdir.join(".zlogin"),
                zsh_startup_loader(".zlogin", &zdotdir, source_user_config, true).as_bytes(),
            )?;
            write_private(
                &zdotdir.join(".zlogout"),
                zsh_startup_loader(".zlogout", &zdotdir, source_user_config, true).as_bytes(),
            )?;
            command.arg("-l");
        }
        command.env("ZDOTDIR", zdotdir);
        command.arg("-i");
        Ok(())
    }

    fn configure_bash(
        &self,
        command: &mut CommandBuilder,
        login: bool,
        source_user_config: bool,
    ) -> crate::Result<()> {
        let rc_path = self.directory.path().join("bashrc");
        let mut script = String::new();
        if source_user_config && let Some(home) = home_directory() {
            let original = home.join(".bashrc");
            script.push_str(&source_if_readable(&original, "source"));
        }
        script.push_str(&source_if_readable(&self.init_path, "source"));
        write_private(&rc_path, script.as_bytes())?;
        if login {
            command.arg("--login");
        }
        command.arg("--rcfile");
        command.arg(rc_path);
        command.arg("-i");
        Ok(())
    }

    fn configure_fish(&self, command: &mut CommandBuilder, login: bool, source_user_config: bool) {
        if login {
            command.arg("--login");
        }
        if !source_user_config {
            command.arg("--no-config");
        }
        command.arg("--interactive");
        command.arg("--init-command");
        command.arg(format!("source {}", quote_posix_path(&self.init_path)));
    }

    pub fn take_edit_from_environment(session: &str) -> crate::Result<String> {
        let expected = env::var("HOKAN_SESSION_TOKEN").map_err(|_| {
            crate::Error::Shell("IPC is only available inside a Hokan session".into())
        })?;
        if session != expected {
            return Err(crate::Error::Shell(
                "IPC session token does not match".into(),
            ));
        }
        let directory = env::var_os("HOKAN_SESSION_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| crate::Error::Shell("IPC session directory is missing".into()))?;
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.file_type().is_dir()
            || metadata.uid() != nix::unistd::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(crate::Error::Shell(
                "IPC session directory has unsafe ownership or permissions".into(),
            ));
        }
        let path = directory.join("edit.next");
        let bytes = read_edit_file(&path)?;
        fs::remove_file(path)?;
        String::from_utf8(bytes)
            .map_err(|_| crate::Error::Shell("IPC replacement is not valid UTF-8".into()))
    }
}

pub struct ControlReader {
    cancel: Option<UnixStream>,
    join: Option<JoinHandle<()>>,
}

impl ControlReader {
    fn start(
        fifo_path: PathBuf,
        shell: ShellKind,
        sender: Sender<ControlMessage>,
    ) -> crate::Result<Self> {
        let descriptor = open(
            &fifo_path,
            OFlag::O_RDWR | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(nix_io)?;
        let (cancel, cancel_reader) = UnixStream::pair()?;
        let cancel_descriptor = cancel_reader.as_raw_fd();
        let join = thread::Builder::new()
            .name("hokan-shell-control".into())
            .spawn(move || {
                let _cancel_reader = cancel_reader;
                let mut file = fs::File::from(descriptor);
                let control_descriptor = file.as_raw_fd();
                let mut decoder = ShellProtocolDecoder::new(shell);
                let mut bytes = [0_u8; 16 * 1024];
                loop {
                    let interests = PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR;
                    let mut descriptors = [
                        PollFd::new(control_descriptor, interests),
                        PollFd::new(cancel_descriptor, interests),
                    ];
                    match poll(&mut descriptors, -1) {
                        Ok(0) => continue,
                        Ok(_) => {}
                        Err(nix_compat::errno::Errno::EINTR) => continue,
                        Err(error) => {
                            let _ = sender.send(ControlMessage::Diagnostic(ProtocolDiagnostic {
                                code: "HK-SHL-012",
                                message: format!("control channel poll failed: {error}"),
                            }));
                            break;
                        }
                    }
                    if descriptors[1]
                        .revents()
                        .is_some_and(|events| !events.is_empty())
                    {
                        break;
                    }
                    let control_events = descriptors[0].revents().unwrap_or_else(PollFlags::empty);
                    if control_events.intersects(PollFlags::POLLERR | PollFlags::POLLNVAL) {
                        let _ = sender.send(ControlMessage::Diagnostic(ProtocolDiagnostic {
                            code: "HK-SHL-012",
                            message: "control channel reported a poll error".into(),
                        }));
                        break;
                    }
                    if !control_events.contains(PollFlags::POLLIN) {
                        continue;
                    }
                    match file.read(&mut bytes) {
                        Ok(0) => continue,
                        Ok(count) => {
                            let decoded = decoder.feed(&bytes[..count]);
                            if decoded.buffer_uncertain {
                                let _ = sender.send(ControlMessage::BufferUncertain);
                            }
                            for diagnostic in decoded.diagnostics {
                                let _ = sender.send(ControlMessage::Diagnostic(diagnostic));
                            }
                            for event in decoded.events {
                                let _ = sender.send(ControlMessage::Event(event));
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) => {}
                        Err(error) => {
                            let _ = sender.send(ControlMessage::Diagnostic(ProtocolDiagnostic {
                                code: "HK-SHL-012",
                                message: format!("control channel failed: {error}"),
                            }));
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            cancel: Some(cancel),
            join: Some(join),
        })
    }
}

impl Drop for ControlReader {
    fn drop(&mut self) {
        self.cancel.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn configured_executable(shell: ShellKind) -> PathBuf {
    env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.trim_start_matches('-') == shell.name())
        })
        .unwrap_or_else(|| PathBuf::from(shell.name()))
}

fn home_directory() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn zsh_environment_loader(original: &Path, wrapper: &Path, source_user_config: bool) -> String {
    let mut script = format!(
        "typeset -gr __hokan_wrapper_zdotdir={}\n\
         typeset -g __hokan_user_zdotdir={}\n",
        quote_posix_path(wrapper),
        quote_posix_path(original)
    );
    script.push_str(&zsh_startup_loader(
        ".zshenv",
        wrapper,
        source_user_config,
        false,
    ));
    script
}

fn zsh_startup_loader(
    name: &str,
    wrapper: &Path,
    source_user_config: bool,
    restore_user_zdotdir: bool,
) -> String {
    let mut script = String::new();
    if source_user_config {
        script.push_str(&format!(
            "typeset -g __hokan_user_startup_path=\"${{__hokan_user_zdotdir}}/{name}\"\n\
             export ZDOTDIR=\"$__hokan_user_zdotdir\"\n\
             if [[ -r \"$__hokan_user_startup_path\" ]]; then\n\
               builtin source -- \"$__hokan_user_startup_path\"\n\
             fi\n\
             if [[ -n ${{ZDOTDIR:-}} ]]; then\n\
               typeset -g __hokan_user_zdotdir=\"$ZDOTDIR\"\n\
             else\n\
               typeset -g __hokan_user_zdotdir=\"$HOME\"\n\
             fi\n\
             unset __hokan_user_startup_path\n"
        ));
    }
    if restore_user_zdotdir {
        script.push_str("export ZDOTDIR=\"$__hokan_user_zdotdir\"\n");
    } else {
        script.push_str(&format!("export ZDOTDIR={}\n", quote_posix_path(wrapper)));
    }
    script
}

fn source_if_readable(path: &Path, command: &str) -> String {
    let path = quote_posix_path(path);
    format!("if [ -r {path} ]; then {command} {path}; fi\n")
}

fn quote_posix_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true).mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn read_edit_file(path: &Path) -> crate::Result<Vec<u8>> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(crate::Error::Shell(
            "IPC replacement must be a private regular file owned by the current user".into(),
        ));
    }
    if metadata.len() > EDIT_PAYLOAD_MAX_BYTES as u64 {
        return Err(crate::Error::Shell("IPC replacement is too large".into()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(EDIT_PAYLOAD_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > EDIT_PAYLOAD_MAX_BYTES {
        return Err(crate::Error::Shell("IPC replacement is too large".into()));
    }
    Ok(bytes)
}

fn nix_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn session_is_private_and_builds_each_shell_command() {
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            let session = ShellSession::new(shell).expect("session should initialize");
            assert_eq!(
                fs::metadata(session.directory.path())
                    .expect("session metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            let command = session
                .command_builder(false)
                .expect("command should build");
            assert_eq!(
                command.get_env("HOKAN_SESSION_TOKEN"),
                Some(session.token.as_str().as_ref())
            );
            assert!(command.get_argv().len() >= 2);
        }
    }

    #[test]
    fn replacement_rejects_multiline_and_is_atomic() {
        let session = ShellSession::new(ShellKind::Zsh).expect("session should initialize");
        assert!(session.write_edit("one\ntwo", 3).is_err());
        session
            .write_edit("echo '中'", "echo '中'".len())
            .expect("edit should write");
        assert_eq!(
            fs::read_to_string(&session.edit_path).expect("edit should exist"),
            "8\techo '中'"
        );
    }

    #[test]
    fn maximum_size_replacement_includes_protocol_overhead_safely() {
        let session = ShellSession::new(ShellKind::Zsh).expect("session should initialize");
        let text = "x".repeat(EDIT_MAX_BYTES);
        session
            .write_edit(&text, text.len())
            .expect("maximum replacement should write");

        let payload = read_edit_file(&session.edit_path).expect("maximum payload should read");
        assert!(payload.len() > EDIT_MAX_BYTES);
        assert!(payload.len() <= EDIT_PAYLOAD_MAX_BYTES);
        assert!(payload.ends_with(text.as_bytes()));
    }

    #[test]
    fn edit_files_reject_symlinks_and_fifos_without_blocking() {
        let session = ShellSession::new(ShellKind::Zsh).expect("session should initialize");
        let protected = session.directory.path().join("protected");
        fs::write(&protected, b"unchanged").expect("protected fixture");
        let pending = session.directory.path().join("edit.pending");
        std::os::unix::fs::symlink(&protected, &pending).expect("pending symlink");
        assert!(session.write_edit("echo safe", 9).is_err());
        assert_eq!(
            fs::read(&protected).expect("protected contents"),
            b"unchanged"
        );

        let fifo = session.directory.path().join("edit.fifo");
        mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).expect("edit FIFO");
        let started = Instant::now();
        assert!(read_edit_file(&fifo).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn control_reader_cancellation_wakes_an_idle_poll() {
        let session = ShellSession::new(ShellKind::Zsh).expect("session should initialize");
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let reader = session
            .start_control_reader(sender)
            .expect("control reader");
        let started = Instant::now();
        drop(reader);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn zsh_loader_tracks_user_zdotdir_and_restores_it_after_startup() {
        let wrapper = Path::new("/tmp/hokan wrapper");
        let script = zsh_environment_loader(Path::new("/tmp/user config"), wrapper, true);
        assert!(script.contains("${__hokan_user_zdotdir}/.zshenv"));
        assert!(script.contains("typeset -g __hokan_user_zdotdir=\"$ZDOTDIR\""));
        assert!(script.contains("export ZDOTDIR='/tmp/hokan wrapper'"));

        let login = zsh_startup_loader(".zlogin", wrapper, true, true);
        assert!(login.ends_with("export ZDOTDIR=\"$__hokan_user_zdotdir\"\n"));
    }
}
