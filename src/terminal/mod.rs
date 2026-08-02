pub mod compositor;
pub mod guard;
pub mod input;
pub mod model;
pub mod output;
pub mod render_boundary;
pub mod reply;
pub mod safe_boundary;
pub mod scheduler;
pub mod surface;
pub mod types;

pub use compositor::{CompositorError, OverlayCompositor, PreparedFrame, StagedFrame};
pub use guard::TerminalGuard;
pub use input::{InputDecoder, InputEvent, InputKind};
pub use model::{CursorRestore, ModelUpdate, ScreenRegionSnapshot, TerminalModel};
pub use output::{
    FrameRequest, OutputActor, OutputActorExit, OutputError, OutputHandle, OutputJoin,
    OutputReport, OutputState, RenderGateRequest, SpawnedOutput, process_stdout_is_terminal,
    spawn_stdout, spawn_with_writer, write_process_output,
};
pub use render_boundary::{
    DecodedChildOutput, RenderBoundaryDecoder, RenderBoundaryEvent, SessionToken,
};
pub use reply::{
    RegisteredQuery, RoutedInput, TerminalQueryKind, TerminalReply, TerminalReplyRouter,
};
pub use safe_boundary::{BoundaryScan, SafeBoundaryScanner};
pub use scheduler::LatestFrameScheduler;
pub use surface::{
    OverlayRow, OverlaySurfaceRenderer, OverlayView, RiskLevel, SanitizedText, SurfaceGeometry,
    SurfaceTheme,
};
pub use types::{
    Anchor, AnchorConfidence, BoundaryId, BufferRevision, CellPos, ChildOutputBatch, DrainState,
    FrameRevision, FrameTicket, QueryId, RenderReadiness, ScreenEpoch, ScreenRevision, SurfaceKey,
    SyncOutputCapability, SyncOwnership, TerminalSize, WidthPolicy,
};
