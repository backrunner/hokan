use std::{io::Write, time::Instant};

use super::super::{
    AnchorConfidence, CellPos, CursorRestore, FrameTicket, RenderReadiness, SurfaceGeometry,
    SyncOutputCapability, SyncOwnership,
};
use super::{OutputError, actor::OutputActor};

impl<W: Write> OutputActor<W> {
    pub(super) fn try_commit_latest(&mut self, now: Instant) -> Result<(), OutputError> {
        if let RenderReadiness::AwaitingRedisplay { deadline, .. } = self.readiness
            && now >= deadline
        {
            self.readiness = RenderReadiness::Unknown;
            self.latest_frame = None;
            self.report.rejected_frames = self.report.rejected_frames.saturating_add(1);
            return Ok(());
        }

        let Some(frame) = self.latest_frame.as_ref() else {
            return Ok(());
        };
        let ticket = frame.ticket;
        let valid = !self.foreground
            && self.scanner.is_safe()
            && self.model.confidence() != AnchorConfidence::Unknown
            && self.model.sync_ownership() != SyncOwnership::External
            && self.capability != SyncOutputCapability::BusyExternal
            && self.readiness.admits(ticket, now)
            && ticket.buffer_revision == self.buffer_revision
            && ticket.screen_revision == self.model.screen_revision()
            && ticket.screen_epoch == self.model.screen_epoch()
            && frame.key.screen_epoch == ticket.screen_epoch
            && frame.geometry.rect == frame.key.rect;
        if !valid {
            return Ok(());
        }

        let frame = self.latest_frame.take().expect("frame was checked above");
        let buffer = self.renderer.render(frame.geometry, &frame.view);
        let prepared = self.compositor.prepare(
            frame.key,
            buffer,
            frame.ticket,
            &self.model.cursor_restore(),
            self.capability,
            Some(&self.model),
        )?;
        self.guard.write_staged(prepared.staged())?;
        self.model.apply_hokan_frame(&prepared.staged().bytes);
        self.compositor.commit(prepared)?;
        self.last_committed_ticket = Some(frame.ticket);
        self.report.committed_frames = self.report.committed_frames.saturating_add(1);
        // Our own frame may have closed the sync-output window a deferred
        // redisplay marker was waiting for.
        self.observe_drain(self.last_read_cycle);
        Ok(())
    }

    pub(super) fn hide_overlay(&mut self) -> Result<(), OutputError> {
        self.latest_frame = None;
        let Some(key) = self.compositor.footprint_key() else {
            return Ok(());
        };
        let Some(last_ticket) = self.last_committed_ticket else {
            self.compositor.invalidate();
            return Ok(());
        };
        let rejected_guard = if !self.scanner.is_safe() {
            Some("scanner-unsafe")
        } else if self.model.confidence() == AnchorConfidence::Unknown {
            Some("confidence-unknown")
        } else if key.screen_epoch != self.model.screen_epoch() {
            Some("epoch-mismatch")
        } else if self.model.sync_ownership() == SyncOwnership::External {
            Some("external-sync")
        } else {
            None
        };
        if let Some(guard) = rejected_guard {
            if let Some(log) = &self.debug_log {
                log.overlay_hide_rejected(guard);
            }
            // The committed overlay is still on screen: keep the footprint so
            // a later frame can still blank whatever its rect vacates.
            self.compositor.invalidate_diff_base();
            return Ok(());
        }
        let ticket = FrameTicket {
            buffer_revision: self.buffer_revision,
            frame_revision: last_ticket.frame_revision,
            screen_revision: self.model.screen_revision(),
            screen_epoch: self.model.screen_epoch(),
        };
        let Some(prepared) = self.compositor.prepare_hide(
            ticket,
            &self.model.cursor_restore(),
            self.capability,
            Some(&self.model),
        )?
        else {
            return Ok(());
        };
        self.guard.write_staged(prepared.staged())?;
        self.model.apply_hokan_frame(&prepared.staged().bytes);
        self.compositor.commit(prepared)?;
        self.last_committed_ticket = Some(ticket);
        // Same deferred-drain kick as after a frame commit: the erase closes
        // the sync-output window that held the redisplay marker.
        self.observe_drain(self.last_read_cycle);
        Ok(())
    }

    pub(super) fn prepare_surface_geometry(
        &mut self,
    ) -> Result<Option<SurfaceGeometry>, OutputError> {
        if self.foreground
            || self.model.alternate_screen()
            || self.model.confidence() == AnchorConfidence::Unknown
            || !self.scanner.is_safe()
            || self.size.cols < 2
            || self.size.rows <= self.renderer.height()
        {
            return Ok(None);
        }

        let cursor = self.model.cursor();
        let height = self.renderer.height();
        let required_bottom = cursor.row.saturating_add(1).saturating_add(height);
        let scroll = required_bottom.saturating_sub(self.size.rows);
        if scroll > 0 {
            let restore = self.model.cursor_restore();
            let synchronized = self.capability == SyncOutputCapability::AvailableIdle
                && self.model.sync_ownership() != SyncOwnership::External;
            let bytes = scroll_room_bytes(self.size.rows, cursor, scroll, &restore, synchronized);
            self.guard.write_control(&bytes)?;
            self.model.apply_hokan_frame(&bytes);
            self.compositor.shift_up(scroll);
        }

        let cursor = self.model.cursor();
        let origin = cursor.row.saturating_add(1);
        SurfaceGeometry::new_anchored(
            cursor.col,
            origin,
            self.size,
            height,
            self.max_overlay_width,
        )
        .map(Some)
        .map_err(OutputError::Terminal)
    }
}

/// Bytes that scroll the screen up by `scroll` lines so the overlay fits
/// below the edit line, then restore the cursor where the shell left it.
///
/// `\n` only scrolls at the bottom row of the scroll region; anywhere else it
/// merely moves the cursor down. Moving to the last row first turns every
/// newline into a guaranteed real scroll, so the shell's edit line — which
/// travels up with the rest of the screen — is exactly at
/// `cursor.row - scroll` when the restore CUP lands there. When synchronized
/// output is available the whole injection rides one mode-2026 transaction so
/// it cannot tear into the shell's in-flight redisplay; otherwise it stays a
/// single atomic write.
pub(super) fn scroll_room_bytes(
    rows: u16,
    cursor: CellPos,
    scroll: u16,
    restore: &CursorRestore,
    synchronized: bool,
) -> Vec<u8> {
    let restored_row = cursor.row.saturating_sub(scroll);
    let mut bytes = Vec::with_capacity(scroll as usize + 96 + restore.sgr.len());
    if synchronized {
        bytes.extend_from_slice(b"\x1b[?2026h");
    }
    bytes.extend_from_slice(b"\x1b[?25l\x1b[0m");
    bytes.extend_from_slice(format!("\x1b[{rows};1H").as_bytes());
    bytes.extend(std::iter::repeat_n(b'\n', scroll as usize));
    bytes.extend_from_slice(&restore.sgr);
    bytes.extend_from_slice(format!("\x1b[{};{}H", restored_row + 1, cursor.col + 1).as_bytes());
    bytes.extend_from_slice(if restore.visible {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    });
    if synchronized {
        bytes.extend_from_slice(b"\x1b[?2026l");
    }
    bytes
}
