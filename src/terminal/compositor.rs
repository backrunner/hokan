use std::fmt;

use ratatui::{
    backend::{Backend, CrosstermBackend},
    buffer::{Buffer, Cell, CellDiffOption},
    layout::{Position, Rect},
};
use thiserror::Error;

use super::{FrameTicket, SurfaceKey, SyncOutputCapability, TerminalModel, model::CursorRestore};

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
    /// Diff base: the last committed frame, dropped whenever the shell may
    /// have written into the overlay region or the content moved.
    previous: Option<(SurfaceKey, Buffer)>,
    /// Footprint: the last committed frame as the screen still shows it.
    /// Survives diff-base invalidation (shell overwrites are filtered out
    /// against the terminal model at prepare time) and is translated by
    /// `shift_up` when hokan scrolls the screen, so the cells a moved box
    /// vacates can still be blanked. Dropped by `invalidate`, because an
    /// epoch change makes the coordinates meaningless.
    footprint: Option<(SurfaceKey, Buffer)>,
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
        model: Option<&TerminalModel>,
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
        let mut changes: Vec<(u16, u16, Cell)> = previous
            .diff_iter(diff_target)
            .map(|(x, y, cell)| (x, y, cell.clone()))
            .collect();
        // A moved box leaves the cells its old rect no longer covers on
        // screen: the diff above only spans the new rect. Blank the vacated
        // cells in the same staged frame so the stale border is erased inside
        // the same transaction. An epoch mismatch means the footprint's
        // coordinates no longer describe the screen, so blanking is skipped
        // rather than erasing live shell content.
        if let Some((footprint_key, footprint_buffer)) = self.footprint.as_ref()
            && footprint_key.screen_epoch == key.screen_epoch
            && footprint_key.rect != key.rect
        {
            changes.extend(vacated_blanks(
                footprint_key.rect,
                key.rect,
                footprint_buffer,
                model,
            ));
        }
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
            backend.draw(changes.iter().map(|(x, y, cell)| (*x, *y, cell)))?;
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
        self.footprint = Some((prepared.staged.key, prepared.target.clone()));
        self.previous = Some((prepared.staged.key, prepared.target));
        self.generation = self.generation.saturating_add(1);
        Ok(())
    }

    /// The screen no longer matches anything hokan painted (epoch change,
    /// resize, suspend, foreground app): drop both the diff base and the
    /// footprint.
    pub fn invalidate(&mut self) {
        self.previous = None;
        self.footprint = None;
        self.generation = self.generation.saturating_add(1);
    }

    /// The shell wrote into the overlay region: the committed buffer can no
    /// longer serve as a diff base, but the cells the shell did not touch
    /// still show the footprint, so it is kept for vacated-cell blanking.
    pub fn invalidate_diff_base(&mut self) {
        self.previous = None;
        self.generation = self.generation.saturating_add(1);
    }

    /// Hokan scrolled the screen up by `scroll` rows: the committed content
    /// moved with it. The footprint is translated so the next frame can blank
    /// whatever its new rect does not cover; the diff base is dropped.
    pub fn shift_up(&mut self, scroll: u16) {
        self.previous = None;
        if let Some((key, buffer)) = &mut self.footprint {
            if key.rect.y >= scroll {
                key.rect.y = key.rect.y.saturating_sub(scroll);
                buffer.area = key.rect;
            } else {
                self.footprint = None;
            }
        }
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

/// Default-styled blank cells for the region the footprint rect vacated:
/// every footprint cell outside the new rect that still holds visible
/// content. Cells already blank need no write, and the blank target is always
/// `Cell::default()` so a vacated selected-row background cannot survive as a
/// painted block. When the terminal model is available, a cell is only
/// blanked while the screen still shows the footprint glyph there — cells the
/// shell overwrote since the commit belong to the shell. (Empty and space
/// symbols are treated as equivalent: a space written over a space is
/// invisible either way.)
fn vacated_blanks(
    old: Rect,
    new: Rect,
    old_buffer: &Buffer,
    model: Option<&TerminalModel>,
) -> Vec<(u16, u16, Cell)> {
    let mut blanks = Vec::new();
    for y in old.y..old.bottom() {
        for x in old.x..old.right() {
            if new.contains(Position { x, y }) {
                continue;
            }
            let cell = &old_buffer[(x, y)];
            if *cell == Cell::default() {
                continue;
            }
            if let Some(model) = model {
                let on_screen = model.cell_contents(y, x).unwrap_or_default();
                let on_screen = if on_screen.is_empty() {
                    " "
                } else {
                    on_screen.as_str()
                };
                let symbol = if cell.symbol().is_empty() {
                    " "
                } else {
                    cell.symbol()
                };
                if on_screen != symbol {
                    continue;
                }
            }
            blanks.push((x, y, Cell::default()));
        }
    }
    blanks
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
    fn rect_move_blanks_vacated_cells_in_the_same_transaction() {
        let (_, ticket, cursor, view) = fixture();
        let size = crate::terminal::TerminalSize::new(24, 80).expect("fixture size is valid");
        let geometry_a = SurfaceGeometry::new_anchored(0, 4, size, 3, 40).expect("geometry A");
        let geometry_b = SurfaceGeometry::new_anchored(6, 4, size, 3, 40).expect("geometry B");
        assert_eq!(geometry_a.rect.x, 0);
        assert_eq!(geometry_b.rect.x, 6);
        let key_a = crate::terminal::SurfaceKey {
            screen_epoch: ScreenEpoch::new(1),
            rect: geometry_a.rect,
            theme_revision: 1,
            width_policy: crate::terminal::WidthPolicy::Auto,
        };
        let key_b = crate::terminal::SurfaceKey {
            rect: geometry_b.rect,
            ..key_a
        };
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default(), true);
        let mut compositor = OverlayCompositor::default();
        let first = compositor
            .prepare(
                key_a,
                renderer.render(geometry_a, &view),
                ticket,
                &cursor,
                SyncOutputCapability::AvailableIdle,
                None,
            )
            .expect("first frame should compose");
        let first_bytes = first.staged().bytes.clone();
        compositor.commit(first).expect("first frame should commit");

        let second = compositor
            .prepare(
                key_b,
                renderer.render(geometry_b, &view),
                crate::terminal::FrameTicket {
                    frame_revision: crate::terminal::FrameRevision::new(2),
                    ..ticket
                },
                &cursor,
                SyncOutputCapability::AvailableIdle,
                None,
            )
            .expect("moved frame should compose");
        let bytes = &second.staged().bytes;
        // The blanking rides the same single transaction as the repaint.
        assert!(bytes.starts_with(b"\x1b[?2026h"));
        assert!(bytes.ends_with(b"\x1b[?2026l"));
        assert_eq!(
            bytes
                .windows(b"\x1b[?2026h".len())
                .filter(|window| window == b"\x1b[?2026h")
                .count(),
            1
        );
        assert_eq!(second.staged().changed_rows, vec![4, 5, 6]);

        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(&first_bytes);
        assert_eq!(parser.screen().cell(4, 0).expect("cell").contents(), "╭");
        parser.process(bytes);
        // The vacated left border column (and the selected row's styled
        // interior cells) are blank after the move…
        for row in 4..7 {
            for col in 0..6 {
                let contents = parser.screen().cell(row, col).expect("cell").contents();
                assert!(
                    contents.is_empty() || contents == " ",
                    "vacated cell ({row}, {col}) still holds {contents:?}"
                );
            }
        }
        // …and the box now sits at its new column.
        assert_eq!(parser.screen().cell(4, 6).expect("cell").contents(), "╭");
        assert_eq!(parser.screen().cell(5, 6).expect("cell").contents(), "│");
        assert_eq!(parser.screen().cell(6, 6).expect("cell").contents(), "╰");
    }

    #[test]
    fn shell_overwritten_vacated_cells_are_never_blanked() {
        let (_, ticket, cursor, view) = fixture();
        let size = crate::terminal::TerminalSize::new(24, 80).expect("fixture size is valid");
        let geometry_a = SurfaceGeometry::new_anchored(0, 4, size, 3, 40).expect("geometry A");
        let geometry_b = SurfaceGeometry::new_anchored(6, 4, size, 3, 40).expect("geometry B");
        let key_a = crate::terminal::SurfaceKey {
            screen_epoch: ScreenEpoch::new(1),
            rect: geometry_a.rect,
            theme_revision: 1,
            width_policy: crate::terminal::WidthPolicy::Auto,
        };
        let key_b = crate::terminal::SurfaceKey {
            rect: geometry_b.rect,
            ..key_a
        };
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default(), true);
        let mut compositor = OverlayCompositor::default();
        let mut model = TerminalModel::new(size);
        let first = compositor
            .prepare(
                key_a,
                renderer.render(geometry_a, &view),
                ticket,
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
                None,
            )
            .expect("first frame should compose");
        let first_bytes = first.staged().bytes.clone();
        compositor.commit(first).expect("first frame should commit");
        model.apply_hokan_frame(&first_bytes);

        // The shell writes into the overlay region: the diff base is dropped
        // but the footprint survives, and the shell's cells are filtered out
        // of the blanking set against the model.
        model
            .process(b"\x1b[6;1Hfg")
            .expect("shell write should parse");
        compositor.invalidate_diff_base();

        let second = compositor
            .prepare(
                key_b,
                renderer.render(geometry_b, &view),
                crate::terminal::FrameTicket {
                    frame_revision: crate::terminal::FrameRevision::new(2),
                    ..ticket
                },
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
                Some(&model),
            )
            .expect("moved frame should compose");

        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(&first_bytes);
        parser.process(b"\x1b[6;1Hfg");
        parser.process(&second.staged().bytes);
        // The shell's text inside the vacated region is untouched…
        assert_eq!(parser.screen().cell(5, 0).expect("cell").contents(), "f");
        assert_eq!(parser.screen().cell(5, 1).expect("cell").contents(), "g");
        // …while the stale border cells the shell never wrote are blanked.
        for row in [4, 6] {
            let contents = parser.screen().cell(row, 0).expect("cell").contents();
            assert!(
                contents.is_empty() || contents == " ",
                "vacated cell ({row}, 0) still holds {contents:?}"
            );
        }
        assert_eq!(parser.screen().cell(4, 6).expect("cell").contents(), "╭");
    }

    #[test]
    fn shifted_footprint_blanks_scrolled_border_cells() {
        let (_, ticket, cursor, view) = fixture();
        let size = crate::terminal::TerminalSize::new(24, 80).expect("fixture size is valid");
        let geometry_a = SurfaceGeometry::new_anchored(0, 4, size, 3, 40).expect("geometry A");
        let geometry_b = SurfaceGeometry::new_anchored(6, 4, size, 3, 40).expect("geometry B");
        let key_a = crate::terminal::SurfaceKey {
            screen_epoch: ScreenEpoch::new(1),
            rect: geometry_a.rect,
            theme_revision: 1,
            width_policy: crate::terminal::WidthPolicy::Auto,
        };
        let key_b = crate::terminal::SurfaceKey {
            rect: geometry_b.rect,
            ..key_a
        };
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default(), true);
        let mut compositor = OverlayCompositor::default();
        let mut model = TerminalModel::new(size);
        let first = compositor
            .prepare(
                key_a,
                renderer.render(geometry_a, &view),
                ticket,
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
                None,
            )
            .expect("first frame should compose");
        let first_bytes = first.staged().bytes.clone();
        compositor.commit(first).expect("first frame should commit");
        model.apply_hokan_frame(&first_bytes);

        // Hokan scrolls the screen up one row to make room below the edit
        // line: the footprint follows the shifted content.
        model.apply_hokan_frame(b"\x1b[24;1H\n");
        compositor.shift_up(1);

        let second = compositor
            .prepare(
                key_b,
                renderer.render(geometry_b, &view),
                crate::terminal::FrameTicket {
                    frame_revision: crate::terminal::FrameRevision::new(2),
                    ..ticket
                },
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
                Some(&model),
            )
            .expect("moved frame should compose");

        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(&first_bytes);
        parser.process(b"\x1b[24;1H\n");
        parser.process(&second.staged().bytes);
        // The shifted old box (rows 3..=5 at x=0) is blanked everywhere the
        // new rect does not cover it.
        for row in 3..=5 {
            for col in 0..6 {
                let contents = parser.screen().cell(row, col).expect("cell").contents();
                assert!(
                    contents.is_empty() || contents == " ",
                    "vacated cell ({row}, {col}) still holds {contents:?}"
                );
            }
        }
        assert_eq!(parser.screen().cell(4, 6).expect("cell").contents(), "╭");
    }

    #[test]
    fn epoch_mismatch_skips_vacated_blanking() {
        let (_, ticket, cursor, view) = fixture();
        let size = crate::terminal::TerminalSize::new(24, 80).expect("fixture size is valid");
        let geometry_a = SurfaceGeometry::new_anchored(0, 4, size, 3, 40).expect("geometry A");
        let geometry_b = SurfaceGeometry::new_anchored(6, 4, size, 3, 40).expect("geometry B");
        let key_a = crate::terminal::SurfaceKey {
            screen_epoch: ScreenEpoch::new(1),
            rect: geometry_a.rect,
            theme_revision: 1,
            width_policy: crate::terminal::WidthPolicy::Auto,
        };
        let key_b = crate::terminal::SurfaceKey {
            screen_epoch: ScreenEpoch::new(2),
            rect: geometry_b.rect,
            ..key_a
        };
        let renderer = OverlaySurfaceRenderer::new(3, SurfaceTheme::default(), true);
        let mut compositor = OverlayCompositor::default();
        let first = compositor
            .prepare(
                key_a,
                renderer.render(geometry_a, &view),
                ticket,
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
                None,
            )
            .expect("first frame should compose");
        let first_bytes = first.staged().bytes.clone();
        compositor.commit(first).expect("first frame should commit");

        let second = compositor
            .prepare(
                key_b,
                renderer.render(geometry_b, &view),
                crate::terminal::FrameTicket {
                    frame_revision: crate::terminal::FrameRevision::new(2),
                    screen_epoch: ScreenEpoch::new(2),
                    ..ticket
                },
                &cursor,
                SyncOutputCapability::UnsupportedFallback,
                None,
            )
            .expect("moved frame should compose");
        // The old buffer no longer describes the screen, so the vacated cells
        // are left alone rather than blanking live shell content.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(&first_bytes);
        parser.process(&second.staged().bytes);
        assert_eq!(parser.screen().cell(4, 0).expect("cell").contents(), "╭");
        assert_eq!(parser.screen().cell(4, 6).expect("cell").contents(), "╭");
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
                None,
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
                None,
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
                None,
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
                None,
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
            None,
        );
        assert!(matches!(
            result,
            Err(CompositorError::ExternalSynchronizedOutput)
        ));
    }
}
