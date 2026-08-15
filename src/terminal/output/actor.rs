use std::{collections::VecDeque, io::Write, sync::Arc, time::Instant};

use super::super::{
    AnchorConfidence, BoundaryId, BufferRevision, ChildOutputBatch, DrainState, FrameRevision,
    FrameTicket, OverlayCompositor, OverlaySurfaceRenderer, RenderBoundaryDecoder,
    RenderBoundaryEvent, RenderReadiness, SafeBoundaryScanner, ScreenEpoch, ScreenRevision,
    SessionToken, SurfaceTheme, SyncOutputCapability, TerminalGuard, TerminalModel, TerminalSize,
};
use super::{
    ActorCommand, ControlCommand, FrameRequest, MailboxCloseGuard, OutputActorExit, OutputError,
    OutputHandle, OutputMailbox, OutputReport, OutputState,
    gate::{ObservedBoundary, RECENT_BOUNDARY_LIMIT, RedisplayConvergence},
};
use crate::diagnostics::DebugLog;

pub struct OutputActor<W: Write> {
    pub(super) mailbox: Arc<OutputMailbox>,
    pub(super) guard: TerminalGuard<W>,
    pub(super) decoder: RenderBoundaryDecoder,
    pub(super) scanner: SafeBoundaryScanner,
    pub(super) model: TerminalModel,
    pub(super) compositor: OverlayCompositor,
    pub(super) renderer: OverlaySurfaceRenderer,
    pub(super) surface_theme: SurfaceTheme,
    pub(super) nerd_fonts: bool,
    pub(super) max_overlay_height: u16,
    pub(super) max_overlay_width: u16,
    pub(super) capability: SyncOutputCapability,
    pub(super) readiness: RenderReadiness,
    pub(super) buffer_revision: BufferRevision,
    pub(super) newest_frame_revision: FrameRevision,
    pub(super) latest_frame: Option<FrameRequest>,
    pub(super) last_committed_ticket: Option<FrameTicket>,
    pub(super) recent_boundaries: VecDeque<ObservedBoundary>,
    pub(super) pending_redisplays: VecDeque<(BoundaryId, u64, ScreenEpoch, ScreenRevision)>,
    pub(super) recent_convergences: VecDeque<RedisplayConvergence>,
    pub(super) expected_prompt: Option<BoundaryId>,
    /// Prompt control messages travel over a separate FIFO from PTY output.
    /// Defer mode/alternate-screen recovery until the authenticated marker
    /// itself has crossed the output actor when the two streams race.
    pub(super) pending_prompt_recovery: Option<BoundaryId>,
    pub(super) cursor_probe_ready: bool,
    pub(super) cursor_probe_revision: Option<BufferRevision>,
    pub(super) foreground: bool,
    pub(super) size: TerminalSize,
    pub(super) report: OutputReport,
    pub(super) debug_log: Option<DebugLog>,
    /// Most recent pump read cycle seen from the child; lets hokan's own
    /// commits re-attempt deferred redisplay drains.
    pub(super) last_read_cycle: u64,
}

impl<W: Write> OutputActor<W> {
    #[must_use]
    pub fn new(writer: W, token: SessionToken, size: TerminalSize, overlay_height: u16) -> Self {
        Self::with_guard(TerminalGuard::new(writer), token, size, overlay_height)
    }

    pub(super) fn with_guard(
        guard: TerminalGuard<W>,
        token: SessionToken,
        size: TerminalSize,
        overlay_height: u16,
    ) -> Self {
        let surface_theme = SurfaceTheme::default();
        let current_height = overlay_height.min(size.rows.saturating_sub(1)).max(1);
        Self {
            mailbox: Arc::new(OutputMailbox::default()),
            guard,
            decoder: RenderBoundaryDecoder::new(token),
            scanner: SafeBoundaryScanner::default(),
            model: TerminalModel::new(size),
            compositor: OverlayCompositor::default(),
            renderer: OverlaySurfaceRenderer::new(current_height, surface_theme, true),
            surface_theme,
            nerd_fonts: true,
            max_overlay_height: overlay_height.max(1),
            max_overlay_width: u16::MAX,
            capability: SyncOutputCapability::UnsupportedFallback,
            readiness: RenderReadiness::Unknown,
            buffer_revision: BufferRevision::ZERO,
            newest_frame_revision: FrameRevision::ZERO,
            latest_frame: None,
            last_committed_ticket: None,
            recent_boundaries: VecDeque::with_capacity(RECENT_BOUNDARY_LIMIT),
            pending_redisplays: VecDeque::with_capacity(RECENT_BOUNDARY_LIMIT),
            recent_convergences: VecDeque::with_capacity(RECENT_BOUNDARY_LIMIT),
            expected_prompt: None,
            pending_prompt_recovery: None,
            cursor_probe_ready: false,
            cursor_probe_revision: None,
            foreground: false,
            size,
            report: OutputReport::default(),
            debug_log: None,
            last_read_cycle: 0,
        }
    }

    #[must_use]
    pub fn handle(&self) -> OutputHandle {
        OutputHandle {
            mailbox: Arc::clone(&self.mailbox),
        }
    }

    pub fn run(self) -> Result<OutputActorExit<W>, OutputError> {
        let _close_mailbox = MailboxCloseGuard(Arc::clone(&self.mailbox));
        self.run_inner()
    }

    fn run_inner(mut self) -> Result<OutputActorExit<W>, OutputError> {
        loop {
            let deadline = match self.readiness {
                RenderReadiness::AwaitingRedisplay { deadline, .. } => Some(deadline),
                _ => None,
            };
            let command = self.mailbox.take(deadline)?;
            let mut barrier = None;
            match command {
                ActorCommand::Tick => {}
                ActorCommand::RestoreAndExit => {
                    self.flush_decoder_tail()?;
                    let writer = self.guard.finish()?;
                    return Ok(OutputActorExit {
                        writer,
                        report: self.report,
                    });
                }
                ActorCommand::Suspend(sender) => {
                    let result = self.suspend_terminal();
                    let _ = sender.send(result);
                }
                ActorCommand::Resume(size, sender) => {
                    let result = self.resume_terminal(size);
                    let _ = sender.send(result);
                }
                ActorCommand::Child(batch) => self.handle_child(batch)?,
                ActorCommand::Hide => self.hide_overlay()?,
                ActorCommand::Control(control) => self.handle_control(control)?,
                ActorCommand::Frame(frame) => self.accept_frame(frame),
                ActorCommand::Barrier(sender) => barrier = Some(sender),
            }
            self.try_commit_latest(Instant::now())?;
            if let Some(sender) = barrier {
                let _ = sender.send(());
            }
        }
    }

    fn accept_frame(&mut self, frame: FrameRequest) {
        if frame.ticket.frame_revision <= self.newest_frame_revision {
            self.report.rejected_frames = self.report.rejected_frames.saturating_add(1);
            return;
        }
        self.newest_frame_revision = frame.ticket.frame_revision;
        self.latest_frame = Some(frame);
    }

    pub(super) fn handle_control(&mut self, control: ControlCommand) -> Result<(), OutputError> {
        match control {
            ControlCommand::ArmPromptGate(boundary_id) => self.arm_prompt_gate(boundary_id)?,
            ControlCommand::ArmRenderGate(request) => self.arm_gate(request),
            ControlCommand::ConfirmCursor(position) => {
                let confirmed = self.cursor_probe_ready && self.model.confirm_cursor(position)?;
                if !confirmed {
                    self.readiness = RenderReadiness::Unknown;
                } else if let Some(buffer_revision) = self.cursor_probe_revision.take() {
                    self.buffer_revision = buffer_revision;
                    self.readiness = RenderReadiness::Ready {
                        buffer_revision,
                        screen_revision: self.model.screen_revision(),
                    };
                }
                self.cursor_probe_ready = false;
            }
            ControlCommand::SetSyncCapability(capability) => self.capability = capability,
            ControlCommand::SetBracketedPaste(enabled) => {
                self.guard.set_bracketed_paste(enabled)?;
            }
            ControlCommand::Probe(bytes) => self.guard.write_control(&bytes)?,
            ControlCommand::Resize(size) => {
                self.model.resize(size)?;
                self.size = size;
                let height = self
                    .max_overlay_height
                    .min(size.rows.saturating_sub(1))
                    .max(1);
                self.renderer =
                    OverlaySurfaceRenderer::new(height, self.surface_theme, self.nerd_fonts);
                self.compositor.invalidate();
                self.latest_frame = None;
                self.readiness = RenderReadiness::Unknown;
                self.cursor_probe_ready = false;
                self.cursor_probe_revision = None;
            }
            ControlCommand::ConfigureOverlay {
                max_height,
                max_width,
                color,
                nerd_fonts,
            } => {
                self.hide_overlay()?;
                self.max_overlay_height = max_height.max(1);
                self.max_overlay_width = max_width.max(1);
                self.surface_theme = if color {
                    SurfaceTheme::default()
                } else {
                    SurfaceTheme::plain()
                };
                self.nerd_fonts = nerd_fonts;
                let height = self
                    .max_overlay_height
                    .min(self.size.rows.saturating_sub(1))
                    .max(1);
                self.renderer =
                    OverlaySurfaceRenderer::new(height, self.surface_theme, self.nerd_fonts);
                self.compositor.invalidate();
                self.latest_frame = None;
            }
            ControlCommand::SetForeground(foreground) => {
                self.foreground = foreground;
                if foreground {
                    self.model.begin_foreground();
                } else {
                    self.model.end_foreground();
                }
                self.decoder.set_foreground(foreground);
                if foreground {
                    self.compositor.invalidate();
                    self.latest_frame = None;
                    self.readiness = RenderReadiness::Unknown;
                    self.cursor_probe_ready = false;
                    self.cursor_probe_revision = None;
                }
            }
            ControlCommand::SetDebugLog(log) => self.debug_log = log,
            ControlCommand::UnlockMirrored(buffer_revision) => {
                self.buffer_revision = buffer_revision;
                self.readiness = if !self.foreground
                    && self.scanner.is_safe()
                    && self.model.confidence() != AnchorConfidence::Unknown
                    && !self.model.alternate_screen()
                {
                    RenderReadiness::Ready {
                        buffer_revision,
                        screen_revision: self.model.screen_revision(),
                    }
                } else {
                    RenderReadiness::Unknown
                };
            }
            ControlCommand::Snapshot(sender) => {
                let _ = sender.send(self.output_state());
            }
            ControlCommand::PrepareSurface(sender) => {
                let geometry = self.prepare_surface_geometry()?;
                let _ = sender.send(geometry);
            }
            ControlCommand::InvalidateAnchor => {
                self.model.invalidate()?;
                self.compositor.invalidate();
                self.latest_frame = None;
                self.readiness = RenderReadiness::Unknown;
                self.cursor_probe_ready = false;
                self.cursor_probe_revision = None;
            }
            ControlCommand::AllowCursorProbe(revision) => {
                self.cursor_probe_ready =
                    !self.foreground && !self.model.alternate_screen() && self.scanner.is_safe();
                self.cursor_probe_revision = self.cursor_probe_ready.then_some(revision);
            }
        }
        Ok(())
    }

    fn output_state(&self) -> OutputState {
        OutputState {
            cursor: self.model.cursor(),
            confidence: self.model.confidence(),
            screen_revision: self.model.screen_revision(),
            screen_epoch: self.model.screen_epoch(),
            readiness: self.readiness,
            alternate_screen: self.model.alternate_screen(),
            foreground: self.foreground,
            cursor_probe_ready: self.cursor_probe_ready,
        }
    }

    pub(super) fn handle_child(&mut self, batch: ChildOutputBatch) -> Result<(), OutputError> {
        self.last_read_cycle = batch.read_cycle;
        let overlay_before = self
            .compositor
            .current_key()
            .map(|key| self.model.snapshot_region(key.rect));
        let decoded = self.decoder.feed(&batch.bytes);
        let mut start = 0;
        let mut written_bytes = 0usize;
        for boundary in &decoded.boundaries {
            let segment = &decoded.passthrough[start..boundary.passthrough_offset];
            self.process_child_segment(segment)?;
            self.guard.write_child(segment)?;
            written_bytes = written_bytes.saturating_add(segment.len());
            start = boundary.passthrough_offset;
            self.observe_boundary(
                boundary.event,
                batch.read_cycle,
                batch.drain == DrainState::DrainedToEagain,
            );
            if matches!(
                boundary.event,
                RenderBoundaryEvent::PromptRendered { boundary_id }
                    if self
                        .pending_prompt_recovery
                        .is_some_and(|pending| boundary_id >= pending)
            ) {
                self.recover_prompt_terminal_state()?;
            }
        }
        let tail = &decoded.passthrough[start..];
        self.process_child_segment(tail)?;
        self.guard.write_child(tail)?;
        written_bytes = written_bytes.saturating_add(tail.len());
        if batch.drain == DrainState::DrainedToEagain {
            self.observe_drain(batch.read_cycle);
        }

        if overlay_before
            .as_ref()
            .is_some_and(|snapshot| self.model.region_changed(snapshot))
        {
            self.compositor.invalidate_diff_base();
        }

        self.report.child_batches = self.report.child_batches.saturating_add(1);
        self.report.child_bytes = self.report.child_bytes.saturating_add(written_bytes as u64);
        self.guard
            .observe_external_ownership(self.model.sync_ownership());
        Ok(())
    }

    fn flush_decoder_tail(&mut self) -> Result<(), OutputError> {
        let tail = self.decoder.finish();
        if tail.is_empty() {
            return Ok(());
        }
        self.guard.write_child(&tail)?;
        self.report.child_bytes = self.report.child_bytes.saturating_add(tail.len() as u64);
        Ok(())
    }

    fn process_child_segment(&mut self, bytes: &[u8]) -> Result<(), OutputError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let scan = self.scanner.feed(bytes);
        let update = self.model.process(bytes)?;
        if (scan.became_desynchronized || scan.opaque_control_seen) && !update.epoch_changed {
            self.model.invalidate()?;
        }
        if update.epoch_changed
            || update.alternate_screen
            || scan.became_desynchronized
            || scan.opaque_control_seen
        {
            self.readiness = RenderReadiness::Unknown;
            self.latest_frame = None;
            self.compositor.invalidate();
            self.cursor_probe_ready = false;
            self.cursor_probe_revision = None;
        }
        Ok(())
    }
}
