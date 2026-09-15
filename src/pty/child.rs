use std::{
    io::{Read, Write},
    thread,
    time::{Duration, Instant},
};

use nix::sys::signal::Signal;
use nix_compat::{
    fcntl::{FcntlArg, OFlag, fcntl},
    poll::{PollFd, PollFlags, poll},
};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use crate::terminal::TerminalSize;

/// A foreground child that stops draining the PTY input queue entirely (for
/// example a SIGTSTP'd TUI) must not freeze the session on a large write: if
/// no byte is accepted for this long the write fails instead of blocking the
/// runtime — including its signal handling — forever.
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(10);
/// A child already past exit should reap instantly; one stuck in the kernel's
/// exit path can take arbitrarily long, so teardown bounds the wait.
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

pub struct PtyChild {
    master: Box<dyn MasterPty + Send>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl PtyChild {
    pub fn spawn(command: CommandBuilder, terminal_size: TerminalSize) -> crate::Result<Self> {
        let system = native_pty_system();
        let pair = system
            .openpty(to_pty_size(terminal_size))
            .map_err(|error| crate::Error::Pty(error.to_string()))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| crate::Error::Pty(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| crate::Error::Pty(error.to_string()))?;
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| crate::Error::Pty(error.to_string()))?;
        drop(pair.slave);
        Ok(Self {
            master: pair.master,
            reader: Some(reader),
            writer: Some(writer),
            child,
        })
    }

    pub fn take_reader(&mut self) -> crate::Result<Box<dyn Read + Send>> {
        self.reader
            .take()
            .ok_or_else(|| crate::Error::Pty("PTY reader was already taken".into()))
    }

    pub fn enable_nonblocking_reads(&self) -> crate::Result<i32> {
        let descriptor = self
            .master
            .as_raw_fd()
            .ok_or_else(|| crate::Error::Pty("PTY master descriptor is unavailable".into()))?;
        let flags = fcntl(descriptor, FcntlArg::F_GETFL)
            .map(OFlag::from_bits_truncate)
            .map_err(nix_compat_io)?;
        fcntl(descriptor, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(nix_compat_io)?;
        Ok(descriptor)
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> crate::Result<()> {
        let descriptor = self
            .master
            .as_raw_fd()
            .ok_or_else(|| crate::Error::Pty("PTY master descriptor is unavailable".into()))?;
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| crate::Error::Pty("PTY writer is closed".into()))?;
        // `take_writer` duplicates the master descriptor, so the `O_NONBLOCK`
        // set by `enable_nonblocking_reads` applies here as well. A paste
        // larger than the PTY input queue must wait for the child to drain
        // it — returning `WouldBlock` would tear down the whole session.
        let mut remaining = bytes;
        let mut stall_deadline = Instant::now() + WRITE_STALL_TIMEOUT;
        while !remaining.is_empty() {
            match writer.write(remaining) {
                Ok(0) => {
                    return Err(crate::Error::Pty("PTY writer accepted no bytes".into()));
                }
                Ok(written) => {
                    remaining = &remaining[written..];
                    stall_deadline = Instant::now() + WRITE_STALL_TIMEOUT;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    wait_until_writable(descriptor, stall_deadline)?;
                }
                Err(error) => return Err(crate::Error::Io(error)),
            }
        }
        writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, size: TerminalSize) -> crate::Result<()> {
        self.master
            .resize(to_pty_size(size))
            .map_err(|error| crate::Error::Pty(error.to_string()))
    }

    pub fn try_wait(&mut self) -> crate::Result<Option<ExitStatus>> {
        self.child.try_wait().map_err(crate::Error::Io)
    }

    pub fn kill(&mut self) -> crate::Result<()> {
        self.child.kill().map_err(crate::Error::Io)
    }

    pub fn signal_foreground(&self, signal: Signal) -> crate::Result<()> {
        let process_group = self
            .foreground_process_group()
            .or_else(|| self.shell_process_group())
            .ok_or_else(|| crate::Error::Pty("child process group is unavailable".into()))?;
        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(process_group), signal).map_err(nix_io)
    }

    pub fn close_writer(&mut self) {
        self.writer.take();
    }

    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    #[must_use]
    pub fn foreground_process_group(&self) -> Option<i32> {
        self.master.process_group_leader()
    }

    #[must_use]
    pub fn shell_process_group(&self) -> Option<i32> {
        self.process_id().and_then(|id| i32::try_from(id).ok())
    }

    #[must_use]
    pub fn shell_is_foreground(&self) -> Option<bool> {
        Some(self.foreground_process_group()? == self.shell_process_group()?)
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        self.writer.take();
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.child.kill();
        // `wait` blocks until the child becomes reapable; a child wedged in
        // the kernel's exit path never does, which would hang teardown. Poll
        // briefly and leave the orphan to the init process instead.
        let deadline = Instant::now() + REAP_TIMEOUT;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn wait_until_writable(descriptor: i32, deadline: Instant) -> crate::Result<()> {
    let interests = PollFlags::POLLOUT | PollFlags::POLLHUP | PollFlags::POLLERR;
    let mut descriptors = [PollFd::new(descriptor, interests)];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(crate::Error::Pty(
                "PTY writer stalled: child stopped consuming input".into(),
            ));
        }
        let timeout = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        match poll(&mut descriptors, timeout) {
            // Any readiness result — writable, hang-up, or error — returns so
            // the next `write` reports the real outcome.
            Ok(0) => continue,
            Ok(_) => return Ok(()),
            Err(nix_compat::errno::Errno::EINTR) => continue,
            Err(error) => return Err(nix_compat_io(error)),
        }
    }
}

fn to_pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn nix_io(error: nix::errno::Errno) -> crate::Error {
    crate::Error::Io(std::io::Error::from_raw_os_error(error as i32))
}

fn nix_compat_io(error: nix_compat::errno::Errno) -> crate::Error {
    crate::Error::Io(std::io::Error::from_raw_os_error(error as i32))
}
