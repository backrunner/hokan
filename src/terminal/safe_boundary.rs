const DEFAULT_MAX_SEQUENCE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StringKind {
    Osc,
    Dcs,
    Sos,
    Pm,
    Apc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanState {
    Ground,
    Utf8 {
        remaining: u8,
        next_min: u8,
        next_max: u8,
    },
    Escape,
    EscapeIntermediate,
    Csi,
    ControlString {
        kind: StringKind,
        escape_pending: bool,
    },
    Desynchronized,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BoundaryScan {
    pub safe_to_inject: bool,
    pub became_desynchronized: bool,
    pub opaque_control_seen: bool,
}

#[derive(Debug)]
pub struct SafeBoundaryScanner {
    state: ScanState,
    sequence_bytes: usize,
    max_sequence_bytes: usize,
}

impl Default for SafeBoundaryScanner {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SEQUENCE_BYTES)
    }
}

impl SafeBoundaryScanner {
    #[must_use]
    pub fn new(max_sequence_bytes: usize) -> Self {
        Self {
            state: ScanState::Ground,
            sequence_bytes: 0,
            max_sequence_bytes: max_sequence_bytes.max(8),
        }
    }

    #[must_use]
    pub const fn is_safe(&self) -> bool {
        matches!(self.state, ScanState::Ground)
    }

    #[must_use]
    pub const fn is_desynchronized(&self) -> bool {
        matches!(self.state, ScanState::Desynchronized)
    }

    pub fn reset_at_trusted_boundary(&mut self) {
        self.state = ScanState::Ground;
        self.sequence_bytes = 0;
    }

    pub fn feed(&mut self, bytes: &[u8]) -> BoundaryScan {
        let was_desynchronized = self.is_desynchronized();
        let mut opaque_control_seen = false;

        for &byte in bytes {
            opaque_control_seen |= self.advance(byte);
            self.update_sequence_budget();
        }

        BoundaryScan {
            safe_to_inject: self.is_safe(),
            became_desynchronized: !was_desynchronized && self.is_desynchronized(),
            opaque_control_seen,
        }
    }

    fn advance(&mut self, byte: u8) -> bool {
        match self.state {
            ScanState::Ground => self.advance_ground(byte),
            ScanState::Utf8 {
                remaining,
                next_min,
                next_max,
            } => {
                if !(next_min..=next_max).contains(&byte) {
                    self.state = ScanState::Desynchronized;
                } else if remaining == 1 {
                    self.state = ScanState::Ground;
                } else {
                    self.state = ScanState::Utf8 {
                        remaining: remaining - 1,
                        next_min: 0x80,
                        next_max: 0xbf,
                    };
                }
                false
            }
            ScanState::Escape => {
                match byte {
                    b'[' => self.state = ScanState::Csi,
                    b']' => self.start_string(StringKind::Osc),
                    b'P' => self.start_string(StringKind::Dcs),
                    b'X' => self.start_string(StringKind::Sos),
                    b'^' => self.start_string(StringKind::Pm),
                    b'_' => self.start_string(StringKind::Apc),
                    0x1b => {}
                    0x18 | 0x1a => self.state = ScanState::Ground,
                    0x20..=0x2f => self.state = ScanState::EscapeIntermediate,
                    0x30..=0x7e => self.state = ScanState::Ground,
                    0x00..=0x1f => {}
                    _ => self.state = ScanState::Desynchronized,
                }
                matches!(byte, b'P' | b'X' | b'^' | b'_')
            }
            ScanState::EscapeIntermediate => {
                match byte {
                    0x1b => self.state = ScanState::Escape,
                    0x18 | 0x1a => self.state = ScanState::Ground,
                    0x20..=0x2f => {}
                    0x30..=0x7e => self.state = ScanState::Ground,
                    0x00..=0x1f => {}
                    _ => self.state = ScanState::Desynchronized,
                }
                false
            }
            ScanState::Csi => {
                match byte {
                    0x1b => self.state = ScanState::Escape,
                    0x18 | 0x1a => self.state = ScanState::Ground,
                    0x40..=0x7e => self.state = ScanState::Ground,
                    0x00..=0x3f => {}
                    _ => self.state = ScanState::Desynchronized,
                }
                false
            }
            ScanState::ControlString {
                kind,
                escape_pending,
            } => {
                if escape_pending {
                    match byte {
                        b'\\' | 0x18 | 0x1a => self.state = ScanState::Ground,
                        0x1b => {}
                        _ => {
                            self.state = ScanState::ControlString {
                                kind,
                                escape_pending: false,
                            };
                        }
                    }
                } else {
                    match byte {
                        0x1b => {
                            self.state = ScanState::ControlString {
                                kind,
                                escape_pending: true,
                            };
                        }
                        0x9c | 0x18 | 0x1a => self.state = ScanState::Ground,
                        0x07 if kind == StringKind::Osc => self.state = ScanState::Ground,
                        _ => {}
                    }
                }
                false
            }
            ScanState::Desynchronized => false,
        }
    }

    fn advance_ground(&mut self, byte: u8) -> bool {
        match byte {
            0x1b => self.state = ScanState::Escape,
            0x9b => self.state = ScanState::Csi,
            0x9d => self.start_string(StringKind::Osc),
            0x90 => self.start_string(StringKind::Dcs),
            0x98 => self.start_string(StringKind::Sos),
            0x9e => self.start_string(StringKind::Pm),
            0x9f => self.start_string(StringKind::Apc),
            0xc2..=0xdf => self.start_utf8(1, 0x80, 0xbf),
            0xe0 => self.start_utf8(2, 0xa0, 0xbf),
            0xe1..=0xec | 0xee..=0xef => self.start_utf8(2, 0x80, 0xbf),
            0xed => self.start_utf8(2, 0x80, 0x9f),
            0xf0 => self.start_utf8(3, 0x90, 0xbf),
            0xf1..=0xf3 => self.start_utf8(3, 0x80, 0xbf),
            0xf4 => self.start_utf8(3, 0x80, 0x8f),
            0x80..=0x8f | 0x91..=0x97 | 0x99..=0x9a | 0x9c | 0xa0..=0xc1 | 0xf5..=0xff => {
                self.state = ScanState::Desynchronized;
            }
            _ => {}
        }

        matches!(byte, 0x90 | 0x98 | 0x9e | 0x9f)
    }

    fn start_utf8(&mut self, remaining: u8, next_min: u8, next_max: u8) {
        self.state = ScanState::Utf8 {
            remaining,
            next_min,
            next_max,
        };
    }

    fn start_string(&mut self, kind: StringKind) {
        self.state = ScanState::ControlString {
            kind,
            escape_pending: false,
        };
    }

    fn update_sequence_budget(&mut self) {
        if matches!(self.state, ScanState::Ground) {
            self.sequence_bytes = 0;
            return;
        }
        if matches!(self.state, ScanState::Desynchronized) {
            return;
        }

        self.sequence_bytes = self.sequence_bytes.saturating_add(1);
        if self.sequence_bytes > self.max_sequence_bytes {
            self.state = ScanState::Desynchronized;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn fragmented_sequences_only_unlock_at_ground_state() {
        let cases: &[&[u8]] = &[
            b"\x1b[31m",
            b"\x1b]2;title\x07",
            b"\x1b]2;title\x1b\\",
            b"\x1bPpayload\x1b\\",
            "cmd \u{4e2d}".as_bytes(),
        ];

        for bytes in cases {
            for split in 0..=bytes.len() {
                let mut scanner = SafeBoundaryScanner::default();
                scanner.feed(&bytes[..split]);
                scanner.feed(&bytes[split..]);
                assert!(scanner.is_safe(), "failed at split {split} for {bytes:?}");
            }
        }
    }

    #[test]
    fn partial_control_and_utf8_sequences_are_unsafe() {
        let mut scanner = SafeBoundaryScanner::default();
        assert!(!scanner.feed(b"\x1b[").safe_to_inject);
        assert!(scanner.feed(b"31m").safe_to_inject);
        assert!(!scanner.feed(&[0xe4, 0xb8]).safe_to_inject);
        assert!(scanner.feed(&[0xad]).safe_to_inject);
    }

    #[test]
    fn invalid_utf8_and_overlong_strings_require_trusted_reset() {
        let mut invalid = SafeBoundaryScanner::default();
        assert!(invalid.feed(&[0xf5]).became_desynchronized);
        assert!(!invalid.feed(b"plain").safe_to_inject);
        invalid.reset_at_trusted_boundary();
        assert!(invalid.is_safe());

        let mut overlong = SafeBoundaryScanner::new(8);
        assert!(overlong.feed(b"\x1bP123456789").became_desynchronized);
    }

    #[test]
    fn opaque_control_strings_are_reported_to_the_screen_model() {
        let mut scanner = SafeBoundaryScanner::default();
        let update = scanner.feed(b"\x1bPq data\x1b\\");
        assert!(update.opaque_control_seen);
        assert!(update.safe_to_inject);
    }

    proptest! {
        #[test]
        fn arbitrary_chunking_has_the_same_final_state(
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
            chunk_sizes in proptest::collection::vec(1usize..64, 0..64),
        ) {
            let mut whole = SafeBoundaryScanner::default();
            let whole_update = whole.feed(&bytes);

            let mut chunked = SafeBoundaryScanner::default();
            let mut offset = 0usize;
            let mut saw_opaque = false;
            for chunk_size in chunk_sizes {
                if offset >= bytes.len() {
                    break;
                }
                let end = offset.saturating_add(chunk_size).min(bytes.len());
                saw_opaque |= chunked.feed(&bytes[offset..end]).opaque_control_seen;
                offset = end;
            }
            if offset < bytes.len() {
                saw_opaque |= chunked.feed(&bytes[offset..]).opaque_control_seen;
            }

            prop_assert_eq!(chunked.is_safe(), whole.is_safe());
            prop_assert_eq!(chunked.is_desynchronized(), whole.is_desynchronized());
            prop_assert_eq!(saw_opaque, whole_update.opaque_control_seen);
        }
    }
}
