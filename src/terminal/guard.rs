use std::io::{self, Write};

use super::{StagedFrame, SyncOwnership};

const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";
const RESTORE_PRESENTATION: &[u8] = b"\x18\x1b[0m\x1b[?25h";

pub struct TerminalGuard<W: Write> {
    writer: Option<W>,
    sync_ownership: SyncOwnership,
    raw_mode: Option<RawModeLease>,
    restored: bool,
}

struct RawModeLease {
    restore_needed: bool,
    restored: bool,
}

impl RawModeLease {
    fn acquire() -> io::Result<Self> {
        let already_enabled = crossterm::terminal::is_raw_mode_enabled()?;
        if !already_enabled {
            crossterm::terminal::enable_raw_mode()?;
        }
        Ok(Self {
            restore_needed: !already_enabled,
            restored: false,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.restored && self.restore_needed {
            crossterm::terminal::disable_raw_mode()?;
        }
        self.restored = true;
        Ok(())
    }
}

impl Drop for RawModeLease {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

impl<W: Write> TerminalGuard<W> {
    #[must_use]
    pub fn new(writer: W) -> Self {
        Self {
            writer: Some(writer),
            sync_ownership: SyncOwnership::None,
            raw_mode: None,
            restored: false,
        }
    }

    pub fn acquire_raw_mode(writer: W) -> io::Result<Self> {
        let raw_mode = RawModeLease::acquire()?;
        Ok(Self {
            writer: Some(writer),
            sync_ownership: SyncOwnership::None,
            raw_mode: Some(raw_mode),
            restored: false,
        })
    }

    pub fn observe_external_ownership(&mut self, ownership: SyncOwnership) {
        if self.sync_ownership != SyncOwnership::MayBeOpenByHokann {
            self.sync_ownership = ownership;
        }
    }

    pub fn write_child(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let writer = self.writer_mut()?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    pub fn write_control(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_child(bytes)
    }

    pub fn write_staged(&mut self, frame: &StagedFrame) -> io::Result<()> {
        if frame.is_empty() {
            return Ok(());
        }
        if self.sync_ownership == SyncOwnership::External {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "external synchronized-output transaction is active",
            ));
        }
        if frame.synchronized {
            self.sync_ownership = SyncOwnership::MayBeOpenByHokann;
        }
        let result = (|| {
            let writer = self.writer_mut()?;
            writer.write_all(&frame.bytes)?;
            writer.flush()
        })();
        if result.is_ok() && frame.synchronized {
            self.sync_ownership = SyncOwnership::None;
        }
        result
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        let presentation_result = self.restore_presentation();
        let raw_mode_result = self.raw_mode.as_mut().map_or(Ok(()), RawModeLease::restore);
        match (presentation_result, raw_mode_result) {
            (Ok(()), Ok(())) => {
                self.restored = true;
                Ok(())
            }
            (Err(error), _) | (Ok(()), Err(error)) => Err(error),
        }
    }

    pub fn suspend(&mut self) -> io::Result<()> {
        self.restore()
    }

    pub fn resume(&mut self) -> io::Result<()> {
        if !self.restored {
            return Ok(());
        }
        if self.raw_mode.is_some() {
            self.raw_mode = Some(RawModeLease::acquire()?);
        }
        self.restored = false;
        Ok(())
    }

    fn restore_presentation(&mut self) -> io::Result<()> {
        let end_own_transaction = self.sync_ownership == SyncOwnership::MayBeOpenByHokann;
        let writer = self.writer_mut()?;
        if end_own_transaction {
            writer.write_all(END_SYNCHRONIZED_UPDATE)?;
        }
        writer.write_all(RESTORE_PRESENTATION)?;
        writer.flush()?;
        self.sync_ownership = SyncOwnership::None;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.restore()?;
        self.writer
            .take()
            .ok_or_else(|| io::Error::other("terminal writer already taken"))
    }

    #[must_use]
    pub const fn sync_ownership(&self) -> SyncOwnership {
        self.sync_ownership
    }

    fn writer_mut(&mut self) -> io::Result<&mut W> {
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal writer is unavailable"))
    }
}

impl<W: Write> Drop for TerminalGuard<W> {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{
        BufferRevision, FrameRevision, FrameTicket, ScreenEpoch, ScreenRevision, SurfaceKey,
        WidthPolicy,
    };
    use ratatui::layout::Rect;

    fn frame(synchronized: bool) -> StagedFrame {
        StagedFrame {
            bytes: if synchronized {
                b"\x1b[?2026hframe\x1b[?2026l".to_vec()
            } else {
                b"frame".to_vec()
            },
            ticket: FrameTicket {
                buffer_revision: BufferRevision::new(1),
                frame_revision: FrameRevision::new(1),
                screen_revision: ScreenRevision::new(1),
                screen_epoch: ScreenEpoch::new(1),
            },
            key: SurfaceKey {
                screen_epoch: ScreenEpoch::new(1),
                rect: Rect::new(0, 1, 79, 3),
                theme_revision: 1,
                width_policy: WidthPolicy::Auto,
            },
            synchronized,
            changed_cells: 1,
            changed_rows: vec![1],
        }
    }

    #[test]
    fn successful_frame_clears_hokann_ownership() {
        let mut guard = TerminalGuard::new(Vec::new());
        guard
            .write_staged(&frame(true))
            .expect("frame should write");
        assert_eq!(guard.sync_ownership(), SyncOwnership::None);
        let output = guard.finish().expect("guard should restore");
        assert_eq!(
            output
                .windows(END_SYNCHRONIZED_UPDATE.len())
                .filter(|window| *window == END_SYNCHRONIZED_UPDATE)
                .count(),
            1
        );
    }

    #[test]
    fn restore_never_ends_external_transaction() {
        let mut guard = TerminalGuard::new(Vec::new());
        guard.observe_external_ownership(SyncOwnership::External);
        let error = guard
            .write_staged(&frame(false))
            .expect_err("overlay writes must wait for external transactions");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let output = guard.finish().expect("guard should restore");
        assert!(
            !output
                .windows(END_SYNCHRONIZED_UPDATE.len())
                .any(|window| window == END_SYNCHRONIZED_UPDATE)
        );
        assert!(output.ends_with(RESTORE_PRESENTATION));
    }
}
