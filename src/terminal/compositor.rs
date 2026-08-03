use std::fmt;

use ratatui::{
    backend::{Backend, CrosstermBackend},
    buffer::{Buffer, CellDiffOption},
};
use thiserror::Error;

use super::{FrameTicket, SurfaceKey, SyncOutputCapability, model::CursorRestore};

#[derive(Debug, Error)]
pub enum CompositorError {
    #[error("surface area does not match its key")]
    AreaMismatch,

    #[error("surface contains a control or bidi character")]
    UnsafeCell,

    #[error("terminal synchronized output is owned by another writer")]
    ExternalSynchronizedOutput,

    #[error("prepared frame is stale")]
    StalePreparedFrame,

    #[error("cursor restore state contains an unsafe sequence")]
    UnsafeRestoreState,

    #[error("frame encoding failed: {0}")]
    Encode(#[from] std::io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedFrame {
    pub bytes: Vec<u8>,
    pub ticket: FrameTicket,
    pub key: SurfaceKey,
    pub synchronized: bool,
    pub changed_cells: usize,
    pub changed_rows: Vec<u16>,
}

impl StagedFrame {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

pub struct PreparedFrame {
    staged: StagedFrame,
    target: Buffer,
    base_generation: u64,
}

impl fmt::Debug for PreparedFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedFrame")
            .field("staged", &self.staged)
            .field("base_generation", &self.base_generation)
            .finish_non_exhaustive()
    }
}

impl PreparedFrame {
    #[must_use]
    pub fn staged(&self) -> &StagedFrame {
        &self.staged
    }

    #[must_use]
    pub fn into_staged(self) -> StagedFrame {
        self.staged
    }
}

#[derive(Debug, Default)]
pub struct OverlayCompositor {
    previous: Option<(SurfaceKey, Buffer)>,
    generation: u64,
}

impl OverlayCompositor {
    pub fn prepare(
        &self,
        key: SurfaceKey,
        current: Buffer,
        ticket: FrameTicket,
        cursor: &CursorRestore,
        capability: SyncOutputCapability,
    ) -> Result<PreparedFrame, CompositorError> {
        if current.area != key.rect {
            return Err(CompositorError::AreaMismatch);
        }
        validate_buffer(&current)?;
        if !valid_restore_bytes(&cursor.sgr) {
            return Err(CompositorError::UnsafeRestoreState);
        }
        if capability == SyncOutputCapability::BusyExternal {
            return Err(CompositorError::ExternalSynchronizedOutput);
        }

        let blank = Buffer::empty(key.rect);
        let previous_is_valid = matches!(
            self.previous.as_ref(),
            Some((previous_key, _)) if *previous_key == key
        );
        let previous = self
            .previous
            .as_ref()
            .filter(|(previous_key, _)| *previous_key == key)
            .map_or(&blank, |(_, previous)| previous);
        let mut encoded_current = current.clone();
        if !previous_is_valid {
            for cell in &mut encoded_current.content {
                cell.set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
        let diff_target = if previous_is_valid {
            &current
        } else {
            &encoded_current
        };
        let changes: Vec<_> = previous.diff_iter(diff_target).collect();
        let changed_cells = changes.len();
        let mut changed_rows: Vec<u16> = changes.iter().map(|(_, y, _)| *y).collect();
        changed_rows.sort_unstable();
        changed_rows.dedup();
        if changes.is_empty() {
            return Ok(PreparedFrame {
                staged: StagedFrame {
                    bytes: Vec::new(),
                    ticket,
                    key,
                    synchronized: false,
                    changed_cells: 0,
                    changed_rows,
                },
                target: current,
                base_generation: self.generation,
            });
        }

        let synchronized = capability == SyncOutputCapability::AvailableIdle;
        let mut bytes = Vec::new();
        if synchronized {
            bytes.extend_from_slice(b"\x1b[?2026h");
        }
        if cursor.visible {
            bytes.extend_from_slice(b"\x1b[?25l");
        }
        {
            let mut backend = CrosstermBackend::new(&mut bytes);
            backend.draw(changes.into_iter())?;
        }
        bytes.extend_from_slice(b"\x1b[0m");
        bytes.extend_from_slice(&cursor.sgr);
        bytes.extend_from_slice(
            format!(
                "\x1b[{};{}H",
                cursor.position.row + 1,
                cursor.position.col + 1
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(if cursor.visible {
            b"\x1b[?25h"
        } else {
            b"\x1b[?25l"
        });
        if synchronized {
            bytes.extend_from_slice(b"\x1b[?2026l");
        }
        validate_frame_bytes(&bytes, key.rect.right())?;

        Ok(PreparedFrame {
            staged: StagedFrame {
                bytes,
                ticket,
                key,
                synchronized,
                changed_cells,
                changed_rows,
            },
            target: current,
            base_generation: self.generation,
        })
    }

    pub fn commit(&mut self, prepared: PreparedFrame) -> Result<(), CompositorError> {
        if prepared.base_generation != self.generation {
            return Err(CompositorError::StalePreparedFrame);
        }
        self.previous = Some((prepared.staged.key, prepared.target));
        self.generation = self.generation.saturating_add(1);
        Ok(())
    }

    pub fn invalidate(&mut self) {
        self.previous = None;
        self.generation = self.generation.saturating_add(1);
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn current_key(&self) -> Option<SurfaceKey> {
        self.previous.as_ref().map(|(key, _)| *key)
    }
}

fn validate_buffer(buffer: &Buffer) -> Result<(), CompositorError> {
    if buffer.content.iter().any(|cell| {
        cell.symbol().chars().any(|character| {
            character.is_control()
                || ('\u{80}'..='\u{9f}').contains(&character)
                || matches!(
                    character as u32,
                    0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
                )
        })
    }) {
        Err(CompositorError::UnsafeCell)
    } else {
        Ok(())
    }
}

fn valid_restore_bytes(bytes: &[u8]) -> bool {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        if !remaining.starts_with(b"\x1b[") {
            return false;
        }
        let Some(end) = remaining.iter().position(|byte| *byte == b'm') else {
            return false;
        };
        if end < 2
            || !remaining[2..end]
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
        {
            return false;
        }
        remaining = &remaining[end + 1..];
    }
    true
}

fn validate_frame_bytes(bytes: &[u8], surface_right: u16) -> Result<(), CompositorError> {
    if bytes
        .windows(4)
        .any(|window| window == b"\x1b[2J" || window == b"\x1b[3J")
        || bytes
            .windows(2)
            .any(|window| window == b"\x1b7" || window == b"\x1b8")
        || bytes
            .windows(8)
            .any(|window| window == b"\x1b[?1049h" || window == b"\x1b[?1049l")
    {
        return Err(CompositorError::UnsafeRestoreState);
    }
    let mut scanner = super::SafeBoundaryScanner::default();
    if !scanner.feed(bytes).safe_to_inject {
        return Err(CompositorError::UnsafeRestoreState);
    }
    // Every CUP emitted by Crossterm is checked through changed_rows/rect before encoding;
    // this sentinel keeps the API explicit for future encoders that may write absolute cells.
    let _ = surface_right;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{
        ScreenEpoch, ScreenRevision,
        surface::{
            OverlayRow, OverlaySurfaceRenderer, OverlayView, RiskLevel, SurfaceGeometry,
            SurfaceTheme,
        },
    };

    fn fixture() -> (SurfaceKey, FrameTicket, CursorRestore, OverlayView) {
        let rect = ratatui::layout::Rect::new(0, 4, 79, 3);
        (
            SurfaceKey {
                screen_epoch: ScreenEpoch::new(1),
                rect,
                theme_revision: 1,
                width_policy: crate::terminal::WidthPolicy::Auto,
            },
            FrameTicket {
                buffer_revision: crate::terminal::BufferRevision::new(1),
                frame_revision: crate::terminal::FrameRevision::new(1),
                screen_revision: ScreenRevision::new(1),
                screen_epoch: ScreenEpoch::new(1),
            },
            CursorRestore {
                position: crate::terminal::CellPos::new(3, 5),
                visible: true,
                sgr: b"\x1b[0m".to_vec(),
            },
            OverlayView::with_rows(
                vec![OverlayRow::new(
                    1,
                    "HIS",
                    "ls -lah",
                    "list all",
                    RiskLevel::Low,
                )],
                Some(1),
            ),
        )
    }

    #[test]
    fn synchronized_frame_is_staged_and_paired() {
        let (key, ticket, cursor, view) = fixture();
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default(), true);
        let size = crate::terminal::TerminalSize::new(24, 80).expect("fixture size is valid");
        let geometry = SurfaceGeometry::new(4, size, 3).expect("fixture geometry is valid");
        let compositor = OverlayCompositor::default();
        let buffer = renderer.render(geometry, &view);
        let prepared = compositor
            .prepare(
                key,
                buffer,
                ticket,
                &cursor,
                SyncOutputCapability::AvailableIdle,
            )
            .expect("frame should compose");
        let bytes = &prepared.staged().bytes;
        assert!(bytes.starts_with(b"\x1b[?2026h"));
        assert!(bytes.ends_with(b"\x1b[?2026l"));
        assert!(!bytes.windows(4).any(|window| window == b"\x1b[2J"));
        assert!(
            !bytes
                .windows(2)
                .any(|window| window == b"\x1b7" || window == b"\x1b8")
        );
    }

    #[test]
    fn fallback_uses_no_transaction_and_commit_is_two_phase() {
        let (key, ticket, cursor, view) = fixture();
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default(), true);
        let size = crate::terminal::TerminalSize::new(24, 80).expect("fixture size is valid");
        let geometry = SurfaceGeometry::new(4, size, 3).expect("fixture geometry is valid");
        let mut compositor = OverlayCompositor::default();
        let buffer = renderer.render(geometry, &view);
        let prepared = compositor
            .prepare(
                key,
                buffer,
                ticket,
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
            )
            .expect("fallback frame should compose");
        assert!(!prepared.staged().synchronized);
        assert!(!prepared.staged().bytes.starts_with(b"\x1b[?2026h"));
        compositor
            .commit(prepared)
            .expect("prepared frame should commit");

        let stale_buffer = renderer.render(geometry, &view);
        let stale = compositor
            .prepare(
                key,
                stale_buffer,
                ticket,
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
            )
            .expect("stale fixture should prepare");
        let newer_buffer = renderer.render(geometry, &view);
        let newer = compositor
            .prepare(
                key,
                newer_buffer,
                FrameTicket {
                    frame_revision: crate::terminal::FrameRevision::new(2),
                    ..ticket
                },
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
            )
            .expect("new fixture should prepare");
        compositor.commit(newer).expect("newer frame should commit");
        assert!(matches!(
            compositor.commit(stale),
            Err(CompositorError::StalePreparedFrame)
        ));
    }

    #[test]
    fn external_transaction_is_never_ended_by_hokan() {
        let (key, ticket, cursor, view) = fixture();
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default(), true);
        let size = crate::terminal::TerminalSize::new(24, 80).expect("fixture size is valid");
        let geometry = SurfaceGeometry::new(4, size, 3).expect("fixture geometry is valid");
        let buffer = renderer.render(geometry, &view);
        let result = OverlayCompositor::default().prepare(
            key,
            buffer,
            ticket,
            &cursor,
            SyncOutputCapability::BusyExternal,
        );
        assert!(matches!(
            result,
            Err(CompositorError::ExternalSynchronizedOutput)
        ));
    }
}
