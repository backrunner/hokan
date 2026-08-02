use std::io::{Read, Write};

use nix::sys::signal::Signal;
use nix_compat::fcntl::{FcntlArg, OFlag, fcntl};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use crate::terminal::TerminalSize;

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
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| crate::Error::Pty("PTY writer is closed".into()))?;
        writer.write_all(bytes)?;
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
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
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
