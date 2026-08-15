use super::{
    AnchorConfidence, CellPos, ScreenEpoch, ScreenRevision, SyncOwnership, TerminalSize,
    modes::TerminalInputModes,
};
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
    mode_changes: Vec<InputModeChange>,
}

#[derive(Clone, Copy, Debug)]
enum InputModeChange {
    Dec { mode: u16, enabled: bool },
    KittyPush(u32),
    KittyPop(u16),
    KittySet(u32),
    ModifyOtherKeys(u16),
    Keypad(bool),
    Reset,
}

// OSC sequences are intentionally tolerated: without an osc_dispatch override
// they fall through to the vte::Perform default no-op. Hokan's own OSC 6973
// markers never reach this observer — RenderBoundaryDecoder strips them from
// the child stream before TerminalModel::process sees it.
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
                | 'S' | 'T' | 'X' | 'd' | 'f' | 'm' | 'r',
            ) => {}
            // Device/status queries have no screen side effect.  TUI clients
            // (Kimi in particular) use DA, CPR-like private queries, and
            // keyboard-protocol queries during startup.
            ([], 'c' | 'n' | 'u')
            | ([b'?'], 'n' | 'u' | 'p')
            | ([b'?', b'$'], 'p')
            | ([b'>'], 'c') => {}
            ([], 't') => self.unknown_screen_effect = true,
            ([], 'h' | 'l') => {
                for param in params {
                    match param {
                        [4] => {}
                        _ => self.unknown_screen_effect = true,
                    }
                }
            }
            ([b' '], 'q') => {}
            ([b'?'], 'J' | 'K') => {}
            ([b'?'], 'h' | 'l') => {
                for param in params {
                    match param {
                        [2026] => self.sync_mode_change = Some(action == 'h'),
                        [2004] => self.mode_changes.push(InputModeChange::Dec {
                            mode: 2004,
                            enabled: action == 'h',
                        }),
                        [1 | 6 | 7 | 9 | 25 | 47 | 1047 | 1049] => {
                            if let [mode @ (1 | 9)] = *param {
                                self.mode_changes.push(InputModeChange::Dec {
                                    mode,
                                    enabled: action == 'h',
                                });
                            }
                        }
                        [
                            1000 | 1001 | 1002 | 1003 | 1004 | 1005 | 1006 | 1007 | 1015 | 1016
                            | 1034 | 1036 | 1039 | 2027 | 2028 | 2031 | 8452,
                        ] => {
                            if let [mode] = *param {
                                self.mode_changes.push(InputModeChange::Dec {
                                    mode,
                                    enabled: action == 'h',
                                });
                            }
                        }
                        _ => self.unknown_screen_effect = true,
                    }
                }
            }
            ([b'>'], 'm') => {
                // xterm modifyOtherKeys: CSI > 4;{level} m.  Accept the
                // complete family so a reset/query from a TUI does not make
                // the prompt epoch permanently unknown.
                let mut values = params.into_iter().flat_map(|param| match param {
                    [value] => Some(*value),
                    _ => None,
                });
                if values.next() == Some(4) {
                    self.mode_changes
                        .push(InputModeChange::ModifyOtherKeys(values.next().unwrap_or(0)));
                } else {
                    self.unknown_screen_effect = true;
                }
            }
            ([b'>'], 'u') => {
                let flags = params
                    .into_iter()
                    .next()
                    .and_then(|param| match param {
                        [value] => Some(*value as u32),
                        _ => None,
                    })
                    .unwrap_or(0);
                self.mode_changes.push(InputModeChange::KittyPush(flags));
            }
            ([b'<'], 'u') => {
                let count = params
                    .into_iter()
                    .next()
                    .and_then(|param| match param {
                        [value] => Some(*value),
                        _ => None,
                    })
                    .unwrap_or(1);
                self.mode_changes.push(InputModeChange::KittyPop(count));
            }
            ([b'='], 'u') => {
                let flags = params
                    .into_iter()
                    .next()
                    .and_then(|param| match param {
                        [value] => Some(*value as u32),
                        _ => None,
                    })
                    .unwrap_or(0);
                self.mode_changes.push(InputModeChange::KittySet(flags));
            }
            _ => self.unknown_screen_effect = true,
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore {
            self.unknown_screen_effect = true;
            return;
        }
        match intermediates {
            [] if byte == b'c' => self.mode_changes.push(InputModeChange::Reset),
            [] if byte == b'=' => self.mode_changes.push(InputModeChange::Keypad(true)),
            [] if byte == b'>' => self.mode_changes.push(InputModeChange::Keypad(false)),
            [] if matches!(byte, b'7' | b'8' | b'M' | b'g') => {}
            // G0–G3 charset designation (e.g. ESC ( B): vt100 models charsets,
            // and the final byte is constrained to the valid range by the parser.
            [b'(' | b')' | b'*' | b'+'] => {}
            _ => self.unknown_screen_effect = true,
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
    input_modes: TerminalInputModes,
    foreground: bool,
    foreground_baseline: Option<TerminalInputModes>,
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
            input_modes: TerminalInputModes::default(),
            foreground: false,
            foreground_baseline: None,
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
        for change in self.observer.mode_changes.drain(..) {
            match change {
                InputModeChange::Dec { mode, enabled } => {
                    self.input_modes.apply_dec_mode(mode, enabled);
                }
                InputModeChange::KittyPush(flags) => self.input_modes.push_kitty(flags),
                InputModeChange::KittyPop(count) => self.input_modes.pop_kitty(count),
                InputModeChange::KittySet(flags) => self.input_modes.set_kitty(flags),
                InputModeChange::ModifyOtherKeys(level) => {
                    self.input_modes.apply_modify_other_keys(level);
                }
                InputModeChange::Keypad(enabled) => {
                    self.input_modes.application_keypad = enabled;
                }
                InputModeChange::Reset => {
                    self.input_modes = TerminalInputModes::default();
                    self.sync_ownership = SyncOwnership::None;
                }
            }
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

    pub fn apply_hokan_frame(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Record the shell's terminal input state immediately before a command
    /// or foreground application receives the PTY.
    pub fn begin_foreground(&mut self) {
        if !self.foreground {
            self.foreground_baseline = Some(self.input_modes);
        }
        self.foreground = true;
    }

    pub fn end_foreground(&mut self) {
        self.foreground = false;
    }

    /// Restore input modes changed by the foreground child and return the
    /// exact control bytes that must be sent to the outer terminal.
    pub fn recover_foreground_modes(&mut self) -> Vec<u8> {
        let target = self
            .foreground_baseline
            .take()
            .unwrap_or_else(|| self.input_modes.fallback_prompt_baseline());
        let bytes = self.input_modes.restore_bytes(target);
        self.input_modes = target;
        self.foreground = false;
        bytes
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
    pub fn cell_contents(&self, row: u16, col: u16) -> Option<String> {
        self.parser
            .screen()
            .cell(row, col)
            .map(|cell| cell.contents().to_string())
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

    /// Sync ownership can only be cleared by child bytes (`?2026l` via the
    /// observer); a TUI that leaked a sync transaction never sends it, so the
    /// trusted prompt boundary force-resets it here after writing `?2026l`
    /// to the terminal itself.
    pub fn reset_sync_ownership(&mut self) {
        self.sync_ownership = SyncOwnership::None;
    }

    #[must_use]
    pub const fn sync_ownership(&self) -> SyncOwnership {
        self.sync_ownership
    }

    #[must_use]
    pub const fn bracketed_paste(&self) -> bool {
        self.input_modes.bracketed_paste
    }

    #[must_use]
    pub(crate) const fn input_modes(&self) -> TerminalInputModes {
        self.input_modes
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

    #[test]
    fn tracks_bracketed_paste_and_terminal_resets() {
        let mut model = model();
        model
            .process(b"\x1b[?2004h")
            .expect("bracketed paste enable should parse");
        assert!(model.bracketed_paste());
        model
            .process(b"\x1b[?2004l")
            .expect("bracketed paste disable should parse");
        assert!(!model.bracketed_paste());

        model
            .process(b"\x1b[?2004h\x1bc")
            .expect("full reset should parse");
        assert!(!model.bracketed_paste());
    }

    #[test]
    fn recognizes_kimi_input_modes_and_queries_without_invalidating_screen() {
        let mut model = model();
        model
            .confirm_cursor(CellPos::new(0, 0))
            .expect("CPR should apply");
        let epoch = model.screen_epoch();
        let update = model
            .process(b"\x1b[?1003h\x1b[?1004h\x1b[?1006h\x1b[?2031h\x1b[>7u\x1b[?u\x1b[?996n\x1b[c")
            .expect("TUI mode bytes should parse");
        assert!(!update.epoch_changed);
        assert_eq!(model.screen_epoch(), epoch);

        let mut child = model;
        child.begin_foreground();
        child
            .process(b"\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?2031l\x1b[<u")
            .expect("TUI cleanup bytes should parse");
        child.end_foreground();
        let recovery = child.recover_foreground_modes();
        assert!(
            recovery
                .windows(b"\x1b[?1003h".len())
                .any(|w| w == b"\x1b[?1003h")
        );
        assert!(
            recovery
                .windows(b"\x1b[?1004h".len())
                .any(|w| w == b"\x1b[?1004h")
        );
    }

    #[test]
    fn foreground_recovery_restores_shell_paste_and_clears_tui_modes() {
        let mut model = model();
        model
            .process(b"\x1b[?2004h")
            .expect("shell bracketed paste should parse");
        model.begin_foreground();
        model
            .process(b"\x1b[?2004l\x1b[?1004h\x1b[?2031h\x1b[>7u")
            .expect("TUI modes should parse");
        model.end_foreground();
        let recovery = model.recover_foreground_modes();
        assert!(
            recovery
                .windows(b"\x1b[?2004h".len())
                .any(|w| w == b"\x1b[?2004h")
        );
        assert!(
            recovery
                .windows(b"\x1b[?1004l".len())
                .any(|w| w == b"\x1b[?1004l")
        );
        assert!(
            recovery
                .windows(b"\x1b[?2031l".len())
                .any(|w| w == b"\x1b[?2031l")
        );
        assert!(recovery.windows(b"\x1b[<u".len()).any(|w| w == b"\x1b[<u"));
        assert!(model.bracketed_paste());
    }

    #[test]
    fn powerline_theme_sequences_keep_the_epoch() {
        let mut model = model();
        model
            .confirm_cursor(CellPos::new(0, 0))
            .expect("CPR should apply");
        let epoch = model.screen_epoch();
        for bytes in [
            b"\x1b[2 q".as_slice(),  // DECSCUSR: block cursor shape
            b"\x1b[ q".as_slice(),   // DECSCUSR: default shape
            b"\x1b[5;3f".as_slice(), // HVP: equivalent to CUP
            b"\x1b[?7h".as_slice(),  // DECAWM: autowrap set
            b"\x1b[?7l".as_slice(),  // DECAWM: autowrap reset
            b"\x1b[4h".as_slice(),   // IRM: insert mode set
            b"\x1b[4l".as_slice(),   // IRM: insert mode reset
            b"\x1b(B".as_slice(),    // G0 charset: ASCII
            b"\x1b)0".as_slice(),    // G1 charset: DEC graphics
            b"\x1b*B".as_slice(),    // G2 charset: ASCII
            b"\x1b+0".as_slice(),    // G3 charset: DEC graphics
        ] {
            let update = model.process(bytes).expect("theme sequence should parse");
            assert!(
                !update.epoch_changed,
                "{bytes:?} must not invalidate the epoch"
            );
        }
        assert_eq!(model.screen_epoch(), epoch);
        assert_eq!(model.confidence(), AnchorConfidence::Derived);
    }

    #[test]
    fn modes_outside_the_whitelist_still_invalidate_the_epoch() {
        let mut model = model();
        for bytes in [
            b"\x1b[20h".as_slice(),   // LNM is not a no-op for line endings
            b"\x1b[4;20h".as_slice(), // mixed mode batch keeps the unknown param visible
            b"\x1b[?5h".as_slice(),   // DECSCNM repaints every cell
            b"\x1b[!p".as_slice(),    // DECSTR resets terminal modes
            b"\x1b[!q".as_slice(),    // only DECSCUSR (SP q) is whitelisted
        ] {
            let epoch = model.screen_epoch();
            model.process(bytes).expect("sequence should parse");
            assert!(
                model.screen_epoch() > epoch,
                "{bytes:?} must invalidate the epoch"
            );
        }
    }
}
