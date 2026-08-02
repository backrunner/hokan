use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use ratatui::buffer::Buffer;
use thiserror::Error;

use super::{
    AnchorConfidence, BoundaryId, BufferRevision, ChildOutputBatch, CompositorError, DrainState,
    FrameRevision, FrameTicket, OverlayCompositor, OverlaySurfaceRenderer, OverlayView,
    RenderBoundaryDecoder, RenderBoundaryEvent, RenderReadiness, SafeBoundaryScanner, ScreenEpoch,
    ScreenRevision, SessionToken, SurfaceGeometry, SurfaceKey, SurfaceTheme, SyncOutputCapability,
    SyncOwnership, TerminalGuard, TerminalModel, TerminalSize,
};

const RECENT_BOUNDARY_LIMIT: usize = 8;
const MAX_PENDING_CHILD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct FrameRequest {
    pub ticket: FrameTicket,
    pub key: SurfaceKey,
    pub geometry: SurfaceGeometry,
    pub view: OverlayView,
}

#[derive(Clone, Copy, Debug)]
pub struct RenderGateRequest {
    pub boundary_id: BoundaryId,
    pub buffer_revision: BufferRevision,
    pub deadline: Instant,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OutputReport {
    pub child_batches: u64,
    pub child_bytes: u64,
    pub committed_frames: u64,
    pub rejected_frames: u64,
    pub consumed_boundaries: u64,
}

#[derive(Debug)]
pub struct OutputActorExit<W> {
    pub writer: W,
    pub report: OutputReport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputState {
    pub cursor: super::CellPos,
    pub confidence: AnchorConfidence,
    pub screen_revision: ScreenRevision,
    pub screen_epoch: ScreenEpoch,
    pub readiness: RenderReadiness,
    pub alternate_screen: bool,
    pub foreground: bool,
    pub cursor_probe_ready: bool,
}

pub type OutputJoin<W> = JoinHandle<Result<OutputActorExit<W>, OutputError>>;
pub type SpawnedOutput<W> = (OutputHandle, OutputJoin<W>);

#[derive(Debug, Error)]
pub enum OutputError {
    #[error("output actor is closed")]
    Closed,

    #[error("output actor state was poisoned")]
    Poisoned,

    #[error("output I/O failed: {0}")]
    Io(#[from] io::Error),

    #[error("frame composition failed: {0}")]
    Compose(#[from] CompositorError),

    #[error("terminal state failed: {0}")]
    Terminal(#[from] crate::Error),
}

#[derive(Clone)]
pub struct OutputHandle {
    mailbox: Arc<OutputMailbox>,
}

impl OutputHandle {
    pub fn child_output(&self, batch: ChildOutputBatch) -> Result<(), OutputError> {
        self.mailbox.push_child(batch)
    }

    pub fn arm_render_gate(&self, request: RenderGateRequest) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::ArmRenderGate(request))
    }

    pub fn arm_prompt_gate(&self, boundary_id: BoundaryId) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::ArmPromptGate(boundary_id))
    }

    pub fn commit_latest(&self, request: FrameRequest) -> Result<bool, OutputError> {
        self.mailbox.push_frame(request)
    }

    pub fn confirm_cursor(&self, position: super::CellPos) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::ConfirmCursor(position))
    }

    pub fn set_sync_capability(&self, capability: SyncOutputCapability) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::SetSyncCapability(capability))
    }

    pub fn probe(&self, bytes: &'static [u8]) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::Probe(bytes.to_vec()))
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), OutputError> {
        self.mailbox.push_control(ControlCommand::Resize(size))
    }

    pub fn configure_overlay(
        &self,
        max_height: u16,
        max_width: u16,
        color: bool,
    ) -> Result<(), OutputError> {
        self.mailbox.push_control(ControlCommand::ConfigureOverlay {
            max_height,
            max_width,
            color,
        })
    }

    pub fn set_foreground(&self, foreground: bool) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::SetForeground(foreground))
    }

    pub fn set_foreground_and_wait(&self, foreground: bool) -> Result<(), OutputError> {
        self.set_foreground(foreground)?;
        self.barrier()
    }

    pub fn unlock_mirrored(&self, revision: BufferRevision) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::UnlockMirrored(revision))
    }

    pub fn state(&self) -> Result<OutputState, OutputError> {
        let (sender, receiver) = mpsc::sync_channel(0);
        self.mailbox
            .push_control(ControlCommand::Snapshot(sender))?;
        receiver.recv().map_err(|_| OutputError::Closed)
    }

    pub fn prepare_surface(&self) -> Result<Option<SurfaceGeometry>, OutputError> {
        let (sender, receiver) = mpsc::sync_channel(0);
        self.mailbox
            .push_control(ControlCommand::PrepareSurface(sender))?;
        receiver.recv().map_err(|_| OutputError::Closed)
    }

    pub fn invalidate_anchor(&self) -> Result<(), OutputError> {
        self.mailbox.push_control(ControlCommand::InvalidateAnchor)
    }

    pub fn allow_cursor_probe(&self, revision: BufferRevision) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::AllowCursorProbe(revision))
    }

    pub fn hide_overlay(&self) -> Result<(), OutputError> {
        self.mailbox.push_hide()
    }

    pub fn barrier(&self) -> Result<(), OutputError> {
        let (sender, receiver) = mpsc::sync_channel(0);
        self.mailbox.push_barrier(sender)?;
        receiver.recv().map_err(|_| OutputError::Closed)
    }

    pub fn restore_and_exit(&self) -> Result<(), OutputError> {
        self.mailbox.shutdown()
    }

    pub fn restore_for_suspend(&self) -> Result<(), OutputError> {
        let (sender, receiver) = mpsc::sync_channel(0);
        self.mailbox.push_suspend(sender)?;
        receiver
            .recv()
            .map_err(|_| OutputError::Closed)?
            .map_err(OutputError::Io)
    }

    pub fn resume_after_continue(&self, size: TerminalSize) -> Result<(), OutputError> {
        let (sender, receiver) = mpsc::sync_channel(0);
        self.mailbox.push_resume(size, sender)?;
        receiver
            .recv()
            .map_err(|_| OutputError::Closed)?
            .map_err(OutputError::Io)
    }
}

#[derive(Debug)]
enum ControlCommand {
    ArmPromptGate(BoundaryId),
    ArmRenderGate(RenderGateRequest),
    ConfirmCursor(super::CellPos),
    SetSyncCapability(SyncOutputCapability),
    Resize(TerminalSize),
    ConfigureOverlay {
        max_height: u16,
        max_width: u16,
        color: bool,
    },
    SetForeground(bool),
    Probe(Vec<u8>),
    UnlockMirrored(BufferRevision),
    Snapshot(SyncSender<OutputState>),
    PrepareSurface(SyncSender<Option<SurfaceGeometry>>),
    InvalidateAnchor,
    AllowCursorProbe(BufferRevision),
}

#[derive(Debug)]
enum ActorCommand {
    Tick,
    Suspend(SyncSender<io::Result<()>>),
    Resume(TerminalSize, SyncSender<io::Result<()>>),
    Child(ChildOutputBatch),
    Hide,
    Control(ControlCommand),
    Frame(FrameRequest),
    Barrier(SyncSender<()>),
    RestoreAndExit,
}

#[derive(Debug, Default)]
struct MailboxQueues {
    restore_and_exit: bool,
    closed: bool,
    suspend: VecDeque<SyncSender<io::Result<()>>>,
    resume: VecDeque<(TerminalSize, SyncSender<io::Result<()>>)>,
    child: VecDeque<ChildOutputBatch>,
    child_bytes: usize,
    hide: bool,
    control: VecDeque<ControlCommand>,
    frame: Option<FrameRequest>,
    barriers: VecDeque<SyncSender<()>>,
}

#[derive(Debug, Default)]
struct OutputMailbox {
    queues: Mutex<MailboxQueues>,
    wake: Condvar,
}

impl OutputMailbox {
    fn close(&self) {
        let mut queues = match self.queues.lock() {
            Ok(queues) => queues,
            Err(poisoned) => poisoned.into_inner(),
        };
        *queues = MailboxQueues {
            closed: true,
            ..MailboxQueues::default()
        };
        self.wake.notify_all();
    }

    fn push_child(&self, batch: ChildOutputBatch) -> Result<(), OutputError> {
        let mut queues = self.lock()?;
        while !queues.closed
            && queues.child_bytes >= MAX_PENDING_CHILD_BYTES
            && !queues.child.is_empty()
        {
            queues = self.wake.wait(queues).map_err(|_| OutputError::Poisoned)?;
        }
        ensure_open(&queues)?;
        queues.child_bytes = queues.child_bytes.saturating_add(batch.bytes.len());
        queues.child.push_back(batch);
        self.wake.notify_one();
        Ok(())
    }

    fn push_suspend(&self, sender: SyncSender<io::Result<()>>) -> Result<(), OutputError> {
        let mut queues = self.lock()?;
        ensure_open(&queues)?;
        queues.suspend.push_back(sender);
        queues.frame = None;
        self.wake.notify_one();
        Ok(())
    }

    fn push_resume(
        &self,
        size: TerminalSize,
        sender: SyncSender<io::Result<()>>,
    ) -> Result<(), OutputError> {
        let mut queues = self.lock()?;
        ensure_open(&queues)?;
        queues.resume.push_back((size, sender));
        self.wake.notify_one();
        Ok(())
    }

    fn push_hide(&self) -> Result<(), OutputError> {
        let mut queues = self.lock()?;
        ensure_open(&queues)?;
        queues.hide = true;
        queues.frame = None;
        self.wake.notify_one();
        Ok(())
    }

    fn push_control(&self, command: ControlCommand) -> Result<(), OutputError> {
        let mut queues = self.lock()?;
        ensure_open(&queues)?;
        queues.control.push_back(command);
        self.wake.notify_one();
        Ok(())
    }

    fn push_frame(&self, request: FrameRequest) -> Result<bool, OutputError> {
        let mut queues = self.lock()?;
        ensure_open(&queues)?;
        let accepted = queues
            .frame
            .as_ref()
            .is_none_or(|pending| request.ticket.frame_revision > pending.ticket.frame_revision);
        if accepted {
            queues.frame = Some(request);
            self.wake.notify_one();
        }
        Ok(accepted)
    }

    fn shutdown(&self) -> Result<(), OutputError> {
        let mut queues = self.lock()?;
        if queues.closed {
            return Ok(());
        }
        queues.restore_and_exit = true;
        queues.closed = true;
        queues.frame = None;
        self.wake.notify_one();
        Ok(())
    }

    fn push_barrier(&self, sender: SyncSender<()>) -> Result<(), OutputError> {
        let mut queues = self.lock()?;
        ensure_open(&queues)?;
        queues.barriers.push_back(sender);
        self.wake.notify_one();
        Ok(())
    }

    fn take(&self, deadline: Option<Instant>) -> Result<ActorCommand, OutputError> {
        let mut queues = self.lock()?;
        loop {
            if queues.restore_and_exit {
                queues.restore_and_exit = false;
                return Ok(ActorCommand::RestoreAndExit);
            }
            if let Some(sender) = queues.suspend.pop_front() {
                return Ok(ActorCommand::Suspend(sender));
            }
            if let Some((size, sender)) = queues.resume.pop_front() {
                return Ok(ActorCommand::Resume(size, sender));
            }
            if let Some(batch) = queues.child.pop_front() {
                queues.child_bytes = queues.child_bytes.saturating_sub(batch.bytes.len());
                self.wake.notify_all();
                return Ok(ActorCommand::Child(batch));
            }
            if queues.hide {
                queues.hide = false;
                return Ok(ActorCommand::Hide);
            }
            if let Some(control) = queues.control.pop_front() {
                return Ok(ActorCommand::Control(control));
            }
            if let Some(frame) = queues.frame.take() {
                return Ok(ActorCommand::Frame(frame));
            }
            if let Some(barrier) = queues.barriers.pop_front() {
                return Ok(ActorCommand::Barrier(barrier));
            }
            if queues.closed {
                return Ok(ActorCommand::RestoreAndExit);
            }
            if let Some(deadline) = deadline {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(ActorCommand::Tick);
                }
                let (next_queues, timeout) = self
                    .wake
                    .wait_timeout(queues, deadline.saturating_duration_since(now))
                    .map_err(|_| OutputError::Poisoned)?;
                queues = next_queues;
                if timeout.timed_out() && Instant::now() >= deadline {
                    return Ok(ActorCommand::Tick);
                }
            } else {
                queues = self.wake.wait(queues).map_err(|_| OutputError::Poisoned)?;
            }
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, MailboxQueues>, OutputError> {
        self.queues.lock().map_err(|_| OutputError::Poisoned)
    }
}

struct MailboxCloseGuard(Arc<OutputMailbox>);

impl Drop for MailboxCloseGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn ensure_open(queues: &MailboxQueues) -> Result<(), OutputError> {
    if queues.closed {
        Err(OutputError::Closed)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct ObservedBoundary {
    event: RenderBoundaryEvent,
    screen_revision: ScreenRevision,
    screen_epoch: ScreenEpoch,
}

#[derive(Clone, Copy, Debug)]
struct RedisplayConvergence {
    boundary_id: BoundaryId,
    screen_revision: ScreenRevision,
    screen_epoch: ScreenEpoch,
}

pub struct OutputActor<W: Write> {
    mailbox: Arc<OutputMailbox>,
    guard: TerminalGuard<W>,
    decoder: RenderBoundaryDecoder,
    scanner: SafeBoundaryScanner,
    model: TerminalModel,
    compositor: OverlayCompositor,
    renderer: OverlaySurfaceRenderer,
    surface_theme: SurfaceTheme,
    max_overlay_height: u16,
    max_overlay_width: u16,
    capability: SyncOutputCapability,
    readiness: RenderReadiness,
    buffer_revision: BufferRevision,
    newest_frame_revision: FrameRevision,
    latest_frame: Option<FrameRequest>,
    last_committed_ticket: Option<FrameTicket>,
    recent_boundaries: VecDeque<ObservedBoundary>,
    pending_redisplays: VecDeque<(BoundaryId, u64, ScreenEpoch, ScreenRevision)>,
    recent_convergences: VecDeque<RedisplayConvergence>,
    expected_prompt: Option<BoundaryId>,
    cursor_probe_ready: bool,
    cursor_probe_revision: Option<BufferRevision>,
    foreground: bool,
    size: TerminalSize,
    report: OutputReport,
}

impl<W: Write> OutputActor<W> {
    #[must_use]
    pub fn new(writer: W, token: SessionToken, size: TerminalSize, overlay_height: u16) -> Self {
        Self::with_guard(TerminalGuard::new(writer), token, size, overlay_height)
    }

    fn with_guard(
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
            renderer: OverlaySurfaceRenderer::new(current_height, surface_theme),
            surface_theme,
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
            cursor_probe_ready: false,
            cursor_probe_revision: None,
            foreground: false,
            size,
            report: OutputReport::default(),
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

    fn handle_control(&mut self, control: ControlCommand) -> Result<(), OutputError> {
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
            ControlCommand::Probe(bytes) => self.guard.write_control(&bytes)?,
            ControlCommand::Resize(size) => {
                self.model.resize(size)?;
                self.size = size;
                let height = self
                    .max_overlay_height
                    .min(size.rows.saturating_sub(1))
                    .max(1);
                self.renderer = OverlaySurfaceRenderer::new(height, self.surface_theme);
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
            } => {
                self.hide_overlay()?;
                self.max_overlay_height = max_height.max(1);
                self.max_overlay_width = max_width.max(1);
                self.surface_theme = if color {
                    SurfaceTheme::default()
                } else {
                    SurfaceTheme::plain()
                };
                let height = self
                    .max_overlay_height
                    .min(self.size.rows.saturating_sub(1))
                    .max(1);
                self.renderer = OverlaySurfaceRenderer::new(height, self.surface_theme);
                self.compositor.invalidate();
                self.latest_frame = None;
            }
            ControlCommand::SetForeground(foreground) => {
                self.foreground = foreground;
                self.decoder.set_foreground(foreground);
                if foreground {
                    self.compositor.invalidate();
                    self.latest_frame = None;
                    self.readiness = RenderReadiness::Unknown;
                    self.cursor_probe_ready = false;
                    self.cursor_probe_revision = None;
                }
            }
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

    fn arm_prompt_gate(&mut self, boundary_id: BoundaryId) -> Result<(), OutputError> {
        let observed_position = self.recent_boundaries.iter().rposition(|observed| {
            matches!(
                observed.event,
                RenderBoundaryEvent::PromptRendered { boundary_id: observed_id }
                    if observed_id == boundary_id
            )
        });
        self.model.invalidate()?;
        let new_epoch = self.model.screen_epoch();
        if let Some(position) = observed_position {
            for observed in self.recent_boundaries.iter_mut().skip(position) {
                observed.screen_epoch = new_epoch;
            }
            let prompt_revision = self.recent_boundaries[position].screen_revision;
            for convergence in &mut self.recent_convergences {
                if convergence.screen_revision >= prompt_revision {
                    convergence.screen_epoch = new_epoch;
                }
            }
            for (_, _, epoch, _) in &mut self.pending_redisplays {
                *epoch = new_epoch;
            }
        }
        self.compositor.invalidate();
        self.latest_frame = None;
        self.expected_prompt = Some(boundary_id);
        self.cursor_probe_ready = observed_position.is_some();
        self.cursor_probe_revision = None;
        self.readiness = RenderReadiness::AwaitingPromptMarker { boundary_id };
        if observed_position.is_some() {
            self.scanner.reset_at_trusted_boundary();
        }
        Ok(())
    }

    fn prepare_surface_geometry(&mut self) -> Result<Option<SurfaceGeometry>, OutputError> {
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
            let restored_row = cursor.row.saturating_sub(scroll);
            let restore = self.model.cursor_restore();
            let mut bytes = Vec::with_capacity(scroll as usize + 64 + restore.sgr.len());
            bytes.extend_from_slice(b"\x1b[?25l\x1b[0m");
            bytes.extend(std::iter::repeat_n(b'\n', scroll as usize));
            bytes.extend_from_slice(&restore.sgr);
            bytes.extend_from_slice(
                format!("\x1b[{};{}H", restored_row + 1, cursor.col + 1).as_bytes(),
            );
            bytes.extend_from_slice(if restore.visible {
                b"\x1b[?25h"
            } else {
                b"\x1b[?25l"
            });
            self.guard.write_control(&bytes)?;
            self.model.apply_hokann_frame(&bytes);
            self.compositor.invalidate();
        }

        let origin = self.model.cursor().row.saturating_add(1);
        SurfaceGeometry::new_with_width(origin, self.size, height, self.max_overlay_width)
            .map(Some)
            .map_err(OutputError::Terminal)
    }

    fn arm_gate(&mut self, request: RenderGateRequest) {
        self.buffer_revision = request.buffer_revision;
        self.readiness = RenderReadiness::AwaitingRedisplay {
            buffer_revision: request.buffer_revision,
            boundary_id: request.boundary_id,
            deadline: request.deadline,
        };
        if let Some(observed) = self.recent_convergences.iter().rev().find(|observed| {
            observed.boundary_id == request.boundary_id
                && observed.screen_revision == self.model.screen_revision()
                && observed.screen_epoch == self.model.screen_epoch()
        }) {
            self.readiness = RenderReadiness::Ready {
                buffer_revision: request.buffer_revision,
                screen_revision: observed.screen_revision,
            };
        }
    }

    fn handle_child(&mut self, batch: ChildOutputBatch) -> Result<(), OutputError> {
        let overlay_before = self
            .compositor
            .current_key()
            .map(|key| self.model.snapshot_region(key.rect));
        let decoded = self.decoder.feed(&batch.bytes);
        let mut start = 0;
        for boundary in decoded.boundaries {
            self.process_child_segment(&decoded.passthrough[start..boundary.passthrough_offset])?;
            start = boundary.passthrough_offset;
            self.observe_boundary(
                boundary.event,
                batch.read_cycle,
                batch.drain == DrainState::DrainedToEagain,
            );
        }
        self.process_child_segment(&decoded.passthrough[start..])?;
        if batch.drain == DrainState::DrainedToEagain {
            self.observe_drain(batch.read_cycle);
        }

        if overlay_before
            .as_ref()
            .is_some_and(|snapshot| self.model.region_changed(snapshot))
        {
            self.compositor.invalidate();
        }

        self.guard.write_child(&decoded.passthrough)?;
        self.report.child_batches = self.report.child_batches.saturating_add(1);
        self.report.child_bytes = self
            .report
            .child_bytes
            .saturating_add(decoded.passthrough.len() as u64);
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

    fn observe_boundary(&mut self, event: RenderBoundaryEvent, read_cycle: u64, _drained: bool) {
        if let RenderBoundaryEvent::PromptRendered { boundary_id } = event
            && self.expected_prompt == Some(boundary_id)
        {
            self.cursor_probe_ready = true;
            self.scanner.reset_at_trusted_boundary();
        }
        let observed = ObservedBoundary {
            event,
            screen_revision: self.model.screen_revision(),
            screen_epoch: self.model.screen_epoch(),
        };
        if self.recent_boundaries.len() == RECENT_BOUNDARY_LIMIT {
            self.recent_boundaries.pop_front();
        }
        self.recent_boundaries.push_back(observed);
        self.report.consumed_boundaries = self.report.consumed_boundaries.saturating_add(1);

        if let RenderBoundaryEvent::PostRedisplay { boundary_id } = event {
            if self.pending_redisplays.len() == RECENT_BOUNDARY_LIMIT {
                self.pending_redisplays.pop_front();
            }
            self.pending_redisplays.push_back((
                boundary_id,
                read_cycle,
                self.model.screen_epoch(),
                self.model.screen_revision(),
            ));
        }
    }

    fn observe_drain(&mut self, read_cycle: u64) {
        while let Some((boundary_id, marker_cycle, epoch, marker_revision)) =
            self.pending_redisplays.front().copied()
        {
            if marker_cycle > read_cycle {
                break;
            }
            if epoch != self.model.screen_epoch()
                || !self.scanner.is_safe()
                || self.model.alternate_screen()
            {
                self.pending_redisplays.pop_front();
                continue;
            }
            if self.model.screen_revision() <= marker_revision {
                break;
            }
            self.pending_redisplays.pop_front();
            let convergence = RedisplayConvergence {
                boundary_id,
                screen_revision: self.model.screen_revision(),
                screen_epoch: self.model.screen_epoch(),
            };
            if self.recent_convergences.len() == RECENT_BOUNDARY_LIMIT {
                self.recent_convergences.pop_front();
            }
            self.recent_convergences.push_back(convergence);
            if let RenderReadiness::AwaitingRedisplay {
                buffer_revision,
                boundary_id: expected,
                ..
            } = self.readiness
                && expected == boundary_id
            {
                self.readiness = RenderReadiness::Ready {
                    buffer_revision,
                    screen_revision: convergence.screen_revision,
                };
            }
        }
    }

    fn try_commit_latest(&mut self, now: Instant) -> Result<(), OutputError> {
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
        )?;
        self.guard.write_staged(prepared.staged())?;
        self.model.apply_hokann_frame(&prepared.staged().bytes);
        self.compositor.commit(prepared)?;
        self.last_committed_ticket = Some(frame.ticket);
        self.report.committed_frames = self.report.committed_frames.saturating_add(1);
        Ok(())
    }

    fn hide_overlay(&mut self) -> Result<(), OutputError> {
        self.latest_frame = None;
        let Some(key) = self.compositor.current_key() else {
            return Ok(());
        };
        let Some(last_ticket) = self.last_committed_ticket else {
            self.compositor.invalidate();
            return Ok(());
        };
        if !self.scanner.is_safe()
            || self.model.confidence() == AnchorConfidence::Unknown
            || key.screen_epoch != self.model.screen_epoch()
            || self.model.sync_ownership() == SyncOwnership::External
        {
            self.compositor.invalidate();
            return Ok(());
        }
        let ticket = FrameTicket {
            buffer_revision: self.buffer_revision,
            frame_revision: last_ticket.frame_revision,
            screen_revision: self.model.screen_revision(),
            screen_epoch: self.model.screen_epoch(),
        };
        let prepared = self.compositor.prepare(
            key,
            Buffer::empty(key.rect),
            ticket,
            &self.model.cursor_restore(),
            self.capability,
        )?;
        self.guard.write_staged(prepared.staged())?;
        self.model.apply_hokann_frame(&prepared.staged().bytes);
        self.compositor.commit(prepared)?;
        Ok(())
    }

    fn suspend_terminal(&mut self) -> io::Result<()> {
        let hide_result = self
            .hide_overlay()
            .map_err(|error| io::Error::other(error.to_string()));
        self.latest_frame = None;
        self.compositor.invalidate();
        self.readiness = RenderReadiness::Unknown;
        self.cursor_probe_ready = false;
        self.cursor_probe_revision = None;
        let restore_result = self.guard.suspend();
        hide_result.and(restore_result)
    }

    fn resume_terminal(&mut self, size: TerminalSize) -> io::Result<()> {
        self.guard.resume()?;
        self.size = size;
        let height = self
            .max_overlay_height
            .min(size.rows.saturating_sub(1))
            .max(1);
        self.renderer = OverlaySurfaceRenderer::new(height, self.surface_theme);
        self.model
            .resize(size)
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.compositor.invalidate();
        self.latest_frame = None;
        self.readiness = RenderReadiness::Unknown;
        self.cursor_probe_ready = false;
        self.cursor_probe_revision = None;
        Ok(())
    }
}

pub fn spawn_with_writer<W>(
    writer: W,
    token: SessionToken,
    size: TerminalSize,
    overlay_height: u16,
) -> Result<SpawnedOutput<W>, OutputError>
where
    W: Write + Send + 'static,
{
    let actor = OutputActor::new(writer, token, size, overlay_height);
    spawn_actor(actor)
}

fn spawn_actor<W>(actor: OutputActor<W>) -> Result<SpawnedOutput<W>, OutputError>
where
    W: Write + Send + 'static,
{
    let handle = actor.handle();
    let join = thread::Builder::new()
        .name("hokann-output".into())
        .spawn(move || actor.run())?;
    Ok((handle, join))
}

pub fn spawn_stdout(
    token: SessionToken,
    size: TerminalSize,
    overlay_height: u16,
) -> Result<SpawnedOutput<io::Stdout>, OutputError> {
    let guard = TerminalGuard::acquire_raw_mode(io::stdout())?;
    spawn_actor(OutputActor::with_guard(guard, token, size, overlay_height))
}

pub fn write_process_output(bytes: &[u8]) -> io::Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(bytes)?;
    output.flush()
}

#[must_use]
pub fn process_stdout_is_terminal() -> bool {
    io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::terminal::{OverlayRow, RiskLevel, WidthPolicy, render_boundary::encode_marker};

    fn token() -> SessionToken {
        SessionToken::parse("0123456789abcdef0123456789abcdef").expect("fixture token is valid")
    }

    fn frame_request() -> FrameRequest {
        let terminal = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
        let geometry = SurfaceGeometry::new(4, terminal, 3).expect("fixture geometry is valid");
        FrameRequest {
            ticket: FrameTicket {
                buffer_revision: BufferRevision::new(1),
                frame_revision: FrameRevision::new(1),
                screen_revision: ScreenRevision::new(2),
                screen_epoch: ScreenEpoch::new(1),
            },
            key: SurfaceKey {
                screen_epoch: ScreenEpoch::new(1),
                rect: geometry.rect,
                theme_revision: 1,
                width_policy: WidthPolicy::Auto,
            },
            geometry,
            view: OverlayView::with_rows(
                vec![OverlayRow::new(1, "HIS", "ls", "list", RiskLevel::Low)],
                Some(1),
            ),
        }
    }

    #[derive(Debug)]
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn actor_failure_closes_mailbox_and_releases_waiters() {
        let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
        let (handle, join) = spawn_with_writer(FailingWriter, token(), size, 3)
            .expect("output actor thread should spawn");
        handle.probe(b"probe").expect("probe should queue");
        let error = join
            .join()
            .expect("actor should not panic")
            .expect_err("writer should fail");
        assert!(matches!(error, OutputError::Io(_)));
        assert!(matches!(handle.state(), Err(OutputError::Closed)));
        assert!(matches!(handle.barrier(), Err(OutputError::Closed)));
        assert!(matches!(
            handle.child_output(ChildOutputBatch {
                read_cycle: 1,
                bytes: vec![b'x'],
                drain: DrainState::DrainedToEagain,
            }),
            Err(OutputError::Closed)
        ));
    }

    #[test]
    fn live_overlay_configuration_updates_fixed_geometry_and_theme() {
        let size = TerminalSize::new(24, 120).expect("terminal size");
        let mut actor = OutputActor::new(Vec::new(), token(), size, 12);
        actor
            .handle_control(ControlCommand::ConfigureOverlay {
                max_height: 5,
                max_width: 60,
                color: false,
            })
            .expect("configure overlay");
        assert_eq!(actor.max_overlay_height, 5);
        assert_eq!(actor.max_overlay_width, 60);
        assert_eq!(actor.renderer.height(), 5);
        assert_eq!(actor.surface_theme.normal, ratatui::style::Style::default());
    }

    #[test]
    fn end_to_end_gate_writes_child_before_overlay() {
        let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
        let (handle, join) = spawn_with_writer(Vec::new(), token(), size, 3)
            .expect("output actor thread should spawn");
        handle
            .arm_prompt_gate(BoundaryId::new(1))
            .expect("prompt gate should queue");
        let prompt_marker = encode_marker(
            &token(),
            RenderBoundaryEvent::PromptRendered {
                boundary_id: BoundaryId::new(1),
            },
        );
        let mut prompt = b"prompt".to_vec();
        prompt.extend_from_slice(&prompt_marker);
        handle
            .child_output(ChildOutputBatch {
                read_cycle: 1,
                bytes: prompt,
                drain: DrainState::DrainedToEagain,
            })
            .expect("prompt output should queue");
        handle
            .confirm_cursor(super::super::CellPos::new(0, 0))
            .expect("cursor confirmation should queue");
        handle
            .arm_render_gate(RenderGateRequest {
                boundary_id: BoundaryId::new(1),
                buffer_revision: BufferRevision::new(1),
                deadline: Instant::now() + Duration::from_secs(1),
            })
            .expect("render gate should queue");
        let marker = encode_marker(
            &token(),
            RenderBoundaryEvent::PostRedisplay {
                boundary_id: BoundaryId::new(1),
            },
        );
        let mut redraw = marker.clone();
        redraw.extend_from_slice(b"redraw");
        handle
            .child_output(ChildOutputBatch {
                read_cycle: 2,
                bytes: redraw,
                drain: DrainState::DrainedToEagain,
            })
            .expect("child output should queue");
        assert!(
            handle
                .commit_latest(frame_request())
                .expect("frame should queue")
        );
        handle.barrier().expect("actor should reach the barrier");
        handle
            .restore_and_exit()
            .expect("restore should be requested");
        let exit = join
            .join()
            .expect("actor should not panic")
            .expect("actor should exit cleanly");
        assert!(exit.writer.starts_with(b"prompt"));
        assert!(
            !exit
                .writer
                .windows(prompt_marker.len())
                .any(|window| window == prompt_marker)
        );
        assert!(
            !exit
                .writer
                .windows(marker.len())
                .any(|window| window == marker)
        );
        assert_eq!(exit.report.committed_frames, 1);
        assert_eq!(exit.report.consumed_boundaries, 2);
    }

    #[test]
    fn expired_gate_and_stale_ticket_never_render() {
        let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
        let (handle, join) = spawn_with_writer(Vec::new(), token(), size, 3)
            .expect("output actor thread should spawn");
        handle
            .confirm_cursor(super::super::CellPos::new(0, 0))
            .expect("cursor confirmation should queue");
        handle
            .arm_render_gate(RenderGateRequest {
                boundary_id: BoundaryId::new(1),
                buffer_revision: BufferRevision::new(1),
                deadline: Instant::now(),
            })
            .expect("render gate should queue");
        handle
            .commit_latest(frame_request())
            .expect("frame should queue");
        handle.barrier().expect("actor should reach the barrier");
        handle
            .restore_and_exit()
            .expect("restore should be requested");
        let exit = join
            .join()
            .expect("actor should not panic")
            .expect("actor should exit cleanly");
        assert_eq!(exit.report.committed_frames, 0);
        assert!(exit.report.rejected_frames >= 1);
    }

    #[test]
    fn marker_only_drain_does_not_unlock_redisplay_gate() {
        let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
        let (handle, join) = spawn_with_writer(Vec::new(), token(), size, 3)
            .expect("output actor thread should spawn");
        handle
            .arm_prompt_gate(BoundaryId::new(1))
            .expect("prompt gate should queue");
        let prompt_marker = encode_marker(
            &token(),
            RenderBoundaryEvent::PromptRendered {
                boundary_id: BoundaryId::new(1),
            },
        );
        let mut prompt = b"$ ".to_vec();
        prompt.extend_from_slice(&prompt_marker);
        handle
            .child_output(ChildOutputBatch {
                read_cycle: 1,
                bytes: prompt,
                drain: DrainState::DrainedToEagain,
            })
            .expect("prompt output should queue");
        handle
            .confirm_cursor(super::super::CellPos::new(0, 2))
            .expect("cursor confirmation should queue");
        handle
            .arm_render_gate(RenderGateRequest {
                boundary_id: BoundaryId::new(1),
                buffer_revision: BufferRevision::new(1),
                deadline: Instant::now() + Duration::from_secs(1),
            })
            .expect("render gate should queue");
        let marker = encode_marker(
            &token(),
            RenderBoundaryEvent::PostRedisplay {
                boundary_id: BoundaryId::new(1),
            },
        );
        handle
            .child_output(ChildOutputBatch {
                read_cycle: 2,
                bytes: marker,
                drain: DrainState::DrainedToEagain,
            })
            .expect("marker-only cycle should queue");
        handle.barrier().expect("actor should reach the barrier");
        assert!(matches!(
            handle.state().expect("state should be available").readiness,
            RenderReadiness::AwaitingRedisplay { .. }
        ));

        handle
            .child_output(ChildOutputBatch {
                read_cycle: 3,
                bytes: b"x".to_vec(),
                drain: DrainState::DrainedToEagain,
            })
            .expect("screen redraw should queue");
        handle.barrier().expect("actor should reach the barrier");
        assert!(matches!(
            handle.state().expect("state should be available").readiness,
            RenderReadiness::Ready {
                buffer_revision,
                ..
            } if buffer_revision == BufferRevision::new(1)
        ));
        handle
            .restore_and_exit()
            .expect("restore should be requested");
        join.join()
            .expect("actor should not panic")
            .expect("actor should exit cleanly");
    }

    #[test]
    fn render_gate_deadline_wakes_an_idle_actor() {
        let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
        let (handle, join) = spawn_with_writer(Vec::new(), token(), size, 3)
            .expect("output actor thread should spawn");
        handle
            .arm_render_gate(RenderGateRequest {
                boundary_id: BoundaryId::new(1),
                buffer_revision: BufferRevision::new(1),
                deadline: Instant::now() + Duration::from_millis(20),
            })
            .expect("render gate should queue");
        handle
            .commit_latest(frame_request())
            .expect("frame should queue");
        handle.barrier().expect("actor should reach the barrier");
        std::thread::sleep(Duration::from_millis(80));
        handle
            .restore_and_exit()
            .expect("restore should be requested");
        let exit = join
            .join()
            .expect("actor should not panic")
            .expect("actor should exit cleanly");
        assert_eq!(exit.report.committed_frames, 0);
        assert_eq!(exit.report.rejected_frames, 1);
    }

    #[test]
    fn shutdown_flushes_partial_marker_before_restoring_terminal() {
        let size = TerminalSize::new(24, 80).expect("fixture terminal size is valid");
        let (handle, join) = spawn_with_writer(Vec::new(), token(), size, 3)
            .expect("output actor thread should spawn");
        let partial = b"visible\x1b]6973;hokann;1;";
        handle
            .child_output(ChildOutputBatch {
                read_cycle: 1,
                bytes: partial.to_vec(),
                drain: DrainState::DrainedToEagain,
            })
            .expect("partial child output should queue");
        handle.barrier().expect("actor should reach the barrier");
        handle
            .restore_and_exit()
            .expect("restore should be requested");
        let exit = join
            .join()
            .expect("actor should not panic")
            .expect("actor should exit cleanly");

        assert!(exit.writer.starts_with(partial));
        assert_eq!(exit.writer.get(partial.len()), Some(&0x18));
        assert_eq!(exit.report.child_bytes, partial.len() as u64);
    }

    #[test]
    fn mailbox_priority_is_restore_child_hide_control_frame() {
        let mailbox = OutputMailbox::default();
        mailbox
            .push_control(ControlCommand::SetForeground(false))
            .expect("control should queue");
        mailbox
            .push_frame(frame_request())
            .expect("frame should queue");
        mailbox.push_hide().expect("hide should queue");
        mailbox
            .push_child(ChildOutputBatch {
                read_cycle: 1,
                bytes: b"child".to_vec(),
                drain: DrainState::DrainedToEagain,
            })
            .expect("child output should queue");
        assert!(matches!(
            mailbox
                .take(None)
                .expect("child command should be available"),
            ActorCommand::Child(_)
        ));
        assert!(matches!(
            mailbox
                .take(None)
                .expect("hide command should be available"),
            ActorCommand::Hide
        ));
        assert!(matches!(
            mailbox
                .take(None)
                .expect("control command should be available"),
            ActorCommand::Control(_)
        ));
    }
}
