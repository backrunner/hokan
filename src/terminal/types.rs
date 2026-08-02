use std::time::Instant;

use ratatui::layout::Rect;

macro_rules! revision_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub const ZERO: Self = Self(0);

            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            #[must_use]
            pub const fn checked_next(self) -> Option<Self> {
                match self.0.checked_add(1) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }
        }
    };
}

revision_type!(BoundaryId);
revision_type!(BufferRevision);
revision_type!(FrameRevision);
revision_type!(QueryId);
revision_type!(ScreenEpoch);
revision_type!(ScreenRevision);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CellPos {
    pub row: u16,
    pub col: u16,
}

impl CellPos {
    #[must_use]
    pub const fn new(row: u16, col: u16) -> Self {
        Self { row, col }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl TerminalSize {
    pub fn new(rows: u16, cols: u16) -> crate::Result<Self> {
        if rows == 0 || cols == 0 {
            return Err(crate::Error::InvalidGeometry(format!(
                "terminal dimensions must be non-zero, got {rows}x{cols}"
            )));
        }
        Ok(Self { rows, cols })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnchorConfidence {
    Exact,
    Derived,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Anchor {
    pub shell_cursor: CellPos,
    pub overlay_origin: CellPos,
    pub terminal_size: TerminalSize,
    pub screen_revision: ScreenRevision,
    pub screen_epoch: ScreenEpoch,
    pub confidence: AnchorConfidence,
}

impl Anchor {
    #[must_use]
    pub const fn can_render(self) -> bool {
        matches!(
            self.confidence,
            AnchorConfidence::Exact | AnchorConfidence::Derived
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WidthPolicy {
    Auto,
    Narrow,
    Wide,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceKey {
    pub screen_epoch: ScreenEpoch,
    pub rect: Rect,
    pub theme_revision: u64,
    pub width_policy: WidthPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameTicket {
    pub buffer_revision: BufferRevision,
    pub frame_revision: FrameRevision,
    pub screen_revision: ScreenRevision,
    pub screen_epoch: ScreenEpoch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderReadiness {
    AwaitingPromptMarker {
        boundary_id: BoundaryId,
    },
    AwaitingRedisplay {
        buffer_revision: BufferRevision,
        boundary_id: BoundaryId,
        deadline: Instant,
    },
    Ready {
        buffer_revision: BufferRevision,
        screen_revision: ScreenRevision,
    },
    Unknown,
}

impl RenderReadiness {
    #[must_use]
    pub fn admits(self, ticket: FrameTicket, now: Instant) -> bool {
        match self {
            Self::Ready {
                buffer_revision,
                screen_revision,
            } => {
                buffer_revision == ticket.buffer_revision
                    && screen_revision == ticket.screen_revision
            }
            Self::AwaitingRedisplay { deadline, .. } if now >= deadline => false,
            Self::AwaitingPromptMarker { .. } | Self::AwaitingRedisplay { .. } | Self::Unknown => {
                false
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SyncOutputCapability {
    AvailableIdle,
    BusyExternal,
    #[default]
    UnsupportedFallback,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SyncOwnership {
    #[default]
    None,
    External,
    MayBeOpenByHokann,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainState {
    MoreInCurrentCycle,
    DrainedToEagain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildOutputBatch {
    pub read_cycle: u64,
    pub bytes: Vec<u8>,
    pub drain: DrainState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_never_wrap() {
        assert_eq!(
            ScreenRevision::new(41).checked_next(),
            Some(ScreenRevision::new(42))
        );
        assert_eq!(ScreenRevision::new(u64::MAX).checked_next(), None);
    }

    #[test]
    fn timeout_does_not_unlock_rendering() {
        let now = Instant::now();
        let ticket = FrameTicket {
            buffer_revision: BufferRevision::new(3),
            frame_revision: FrameRevision::new(9),
            screen_revision: ScreenRevision::new(5),
            screen_epoch: ScreenEpoch::new(1),
        };
        let readiness = RenderReadiness::AwaitingRedisplay {
            buffer_revision: ticket.buffer_revision,
            boundary_id: BoundaryId::new(7),
            deadline: now,
        };

        assert!(!readiness.admits(ticket, now));
    }
}
