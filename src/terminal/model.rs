use super::{AnchorConfidence, CellPos, ScreenEpoch, ScreenRevision, SyncOwnership, TerminalSize};
use ratatui::layout::Rect;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorRestore {
    pub position: CellPos,
    pub visible: bool,
    pub sgr: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelUpdate {
    pub cursor: CellPos,
    pub alternate_screen: bool,
    pub confidence: AnchorConfidence,
    pub screen_revision: ScreenRevision,
    pub screen_epoch: ScreenEpoch,
    pub epoch_changed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenRegionSnapshot {
    rect: Rect,
    cells: Vec<Option<vt100::Cell>>,
}

#[derive(Debug, Default)]
struct EffectObserver {
    unknown_screen_effect: bool,
    sync_mode_change: Option<bool>,
}

impl vte::Perform for EffectObserver {
    fn execute(&mut self, byte: u8) {
        if !matches!(byte, 0 | 7..=15 | 0x18 | 0x1a) {
            self.unknown_screen_effect = true;
        }
    }

    fn hook(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {
        self.unknown_screen_effect = true;
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            self.unknown_screen_effect = true;
            return;
        }

        match (intermediates, action) {
            (
                [],
                '@' | 'A' | 'B' | 'C' | 'D' | 'E' | 'F' | 'G' | 'H' | 'J' | 'K' | 'L' | 'M' | 'P'
                | 'S' | 'T' | 'X' | 'd' | 'm' | 'r',
            ) => {}
            ([], 't') => self.unknown_screen_effect = true,
            ([b'?'], 'J' | 'K') => {}
            ([b'?'], 'h' | 'l') => {
                for param in params {
                    match param {
                        [2026] => self.sync_mode_change = Some(action == 'h'),
                        [1 | 6 | 9 | 25 | 47 | 1000 | 1002 | 1003 | 1005 | 1006 | 1049 | 2004] => {}
                        _ => self.unknown_screen_effect = true,
                    }
                }
            }
            _ => self.unknown_screen_effect = true,
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore
            || !intermediates.is_empty()
            || !matches!(byte, b'7' | b'8' | b'=' | b'>' | b'M' | b'c' | b'g')
        {
            self.unknown_screen_effect = true;
        }
    }
}

pub struct TerminalModel {
    parser: vt100::Parser,
    observer_parser: vte::Parser,
    observer: EffectObserver,
    screen_revision: ScreenRevision,
    screen_epoch: ScreenEpoch,
    confidence: AnchorConfidence,
    sync_ownership: SyncOwnership,
}

impl TerminalModel {
    #[must_use]
    pub fn new(size: TerminalSize) -> Self {
        Self {
            parser: vt100::Parser::new(size.rows, size.cols, 0),
            observer_parser: vte::Parser::new(),
            observer: EffectObserver::default(),
            screen_revision: ScreenRevision::ZERO,
            screen_epoch: ScreenEpoch::ZERO,
            confidence: AnchorConfidence::Unknown,
            sync_ownership: SyncOwnership::None,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) -> crate::Result<ModelUpdate> {
        if bytes.is_empty() {
            return Ok(self.snapshot(false));
        }

        let was_alternate = self.parser.screen().alternate_screen();
        self.parser.process(bytes);
        self.observer_parser.advance(&mut self.observer, bytes);
        self.screen_revision = self
            .screen_revision
            .checked_next()
            .ok_or_else(|| crate::Error::TerminalProtocol("screen revision exhausted".into()))?;

        let is_alternate = self.parser.screen().alternate_screen();
        let unknown = std::mem::take(&mut self.observer.unknown_screen_effect);
        if let Some(is_set) = self.observer.sync_mode_change.take() {
            self.sync_ownership = if is_set {
                SyncOwnership::External
            } else {
                SyncOwnership::None
            };
        }

        let epoch_changed = unknown || was_alternate != is_alternate;
        if epoch_changed {
            self.invalidate()?;
        } else if self.confidence != AnchorConfidence::Unknown {
            self.confidence = AnchorConfidence::Derived;
        }
        Ok(self.snapshot(epoch_changed))
    }

    pub fn confirm_cursor(&mut self, position: CellPos) -> crate::Result<bool> {
        let cup = format!("\x1b[{};{}H", position.row + 1, position.col + 1);
        self.parser.process(cup.as_bytes());
        let confirmed = self.cursor() == position;
        self.confidence = if confirmed {
            AnchorConfidence::Exact
        } else {
            AnchorConfidence::Unknown
        };
        Ok(confirmed)
    }

    pub fn establish_anchor(&mut self) {
        self.confidence = AnchorConfidence::Exact;
    }

    pub fn resize(&mut self, size: TerminalSize) -> crate::Result<()> {
        self.parser.screen_mut().set_size(size.rows, size.cols);
        self.invalidate()
    }

    pub fn apply_hokann_frame(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    #[must_use]
    pub fn snapshot_region(&self, rect: Rect) -> ScreenRegionSnapshot {
        let mut cells = Vec::with_capacity(rect.width as usize * rect.height as usize);
        for row in rect.y..rect.bottom() {
            for col in rect.x..rect.right() {
                cells.push(self.parser.screen().cell(row, col).cloned());
            }
        }
        ScreenRegionSnapshot { rect, cells }
    }

    #[must_use]
    pub fn region_changed(&self, snapshot: &ScreenRegionSnapshot) -> bool {
        self.snapshot_region(snapshot.rect) != *snapshot
    }

    pub fn invalidate(&mut self) -> crate::Result<()> {
        self.confidence = AnchorConfidence::Unknown;
        self.screen_epoch = self
            .screen_epoch
            .checked_next()
            .ok_or_else(|| crate::Error::TerminalProtocol("screen epoch exhausted".into()))?;
        Ok(())
    }

    #[must_use]
    pub fn cursor(&self) -> CellPos {
        let (row, col) = self.parser.screen().cursor_position();
        CellPos { row, col }
    }

    #[must_use]
    pub fn cursor_restore(&self) -> CursorRestore {
        CursorRestore {
            position: self.cursor(),
            visible: !self.parser.screen().hide_cursor(),
            sgr: self.parser.screen().attributes_formatted(),
        }
    }

    #[must_use]
    pub const fn screen_revision(&self) -> ScreenRevision {
        self.screen_revision
    }

    #[must_use]
    pub const fn screen_epoch(&self) -> ScreenEpoch {
        self.screen_epoch
    }

    #[must_use]
    pub const fn confidence(&self) -> AnchorConfidence {
        self.confidence
    }

    #[must_use]
    pub const fn sync_ownership(&self) -> SyncOwnership {
        self.sync_ownership
    }

    #[must_use]
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    fn snapshot(&self, epoch_changed: bool) -> ModelUpdate {
        ModelUpdate {
            cursor: self.cursor(),
            alternate_screen: self.parser.screen().alternate_screen(),
            confidence: self.confidence,
            screen_revision: self.screen_revision,
            screen_epoch: self.screen_epoch,
            epoch_changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> TerminalModel {
        TerminalModel::new(TerminalSize::new(24, 80).expect("valid terminal size"))
    }

    #[test]
    fn tracks_cursor_sgr_and_visibility() {
        let mut model = model();
        assert!(
            model
                .confirm_cursor(CellPos::new(2, 4))
                .expect("CPR should apply")
        );
        model
            .process(b"\x1b[31mhi\x1b[?25l")
            .expect("terminal bytes should parse");
        let restore = model.cursor_restore();
        assert_eq!(restore.position, CellPos::new(2, 6));
        assert!(!restore.visible);
        assert!(restore.sgr.windows(3).any(|window| window == b"31m"));
        assert_eq!(model.confidence(), AnchorConfidence::Derived);
    }

    #[test]
    fn alternate_screen_and_unknown_sequences_invalidate_epoch() {
        let mut model = model();
        model
            .confirm_cursor(CellPos::new(0, 0))
            .expect("CPR should apply");
        let update = model
            .process(b"\x1b[?1049h")
            .expect("alternate screen should parse");
        assert!(update.alternate_screen);
        assert!(update.epoch_changed);
        assert_eq!(update.confidence, AnchorConfidence::Unknown);

        let epoch = model.screen_epoch();
        model
            .process(b"\x1b[999z")
            .expect("unknown CSI is tolerated");
        assert!(model.screen_epoch() > epoch);
    }

    #[test]
    fn child_synchronized_output_is_external_ownership() {
        let mut model = model();
        model
            .process(b"\x1b[?2026h")
            .expect("sync mode should parse");
        assert_eq!(model.sync_ownership(), SyncOwnership::External);
        model
            .process(b"\x1b[?2026l")
            .expect("sync reset should parse");
        assert_eq!(model.sync_ownership(), SyncOwnership::None);
    }
}
