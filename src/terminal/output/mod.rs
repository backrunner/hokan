mod actor;
mod frame;
mod gate;
mod lifecycle;
#[cfg(test)]
mod tests;

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

use thiserror::Error;

use super::{
    AnchorConfidence, BoundaryId, BufferRevision, ChildOutputBatch, CompositorError, FrameTicket,
    OverlayView, RenderReadiness, ScreenEpoch, ScreenRevision, SessionToken, SurfaceGeometry,
    SurfaceKey, SyncOutputCapability, TerminalGuard, TerminalSize,
};
use crate::diagnostics::DebugLog;

pub use actor::OutputActor;

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

    pub fn set_bracketed_paste(&self, enabled: bool) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::SetBracketedPaste(enabled))
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
        nerd_fonts: bool,
    ) -> Result<(), OutputError> {
        self.mailbox.push_control(ControlCommand::ConfigureOverlay {
            max_height,
            max_width,
            color,
            nerd_fonts,
        })
    }

    pub fn set_foreground(&self, foreground: bool) -> Result<(), OutputError> {
        self.mailbox
            .push_control(ControlCommand::SetForeground(foreground))
    }

    pub fn set_debug_log(&self, log: Option<DebugLog>) -> Result<(), OutputError> {
        self.mailbox.push_control(ControlCommand::SetDebugLog(log))
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
    SetBracketedPaste(bool),
    Resize(TerminalSize),
    ConfigureOverlay {
        max_height: u16,
        max_width: u16,
        color: bool,
        nerd_fonts: bool,
    },
    SetForeground(bool),
    SetDebugLog(Option<DebugLog>),
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
            // A render-gate deadline is a hard boundary. Once elapsed it must
            // beat queued child/control/shutdown work, otherwise a busy or
            // briefly descheduled actor can indefinitely postpone expiry.
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(ActorCommand::Tick);
            }
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
        .name("hokan-output".into())
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
