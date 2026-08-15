pub(crate) const PASTE_START: &[u8] = b"\x1b[200~";
pub(crate) const PASTE_END: &[u8] = b"\x1b[201~";
pub(crate) const MAX_PASTE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputKind {
    Text(String),
    Paste(Vec<u8>),
    /// Raw bytes from an oversized paste; the flags identify protocol
    /// delimiters at this fragment's edges without inspecting its payload.
    PasteFragment {
        strip_start: bool,
        strip_end: bool,
    },
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Tab,
    BackTab,
    Enter,
    Escape,
    CtrlC,
    CtrlD,
    CtrlA,
    CtrlE,
    CtrlK,
    CtrlL,
    CtrlR,
    CtrlU,
    CtrlW,
    Raw,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputEvent {
    pub kind: InputKind,
    pub raw: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct InputDecoder {
    pending: Vec<u8>,
    paste_mode: bool,
    paste_overflow: bool,
    paste_start_pending: bool,
    paste_raw: Vec<u8>,
}

impl InputDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            if self.paste_mode {
                self.paste_raw.append(&mut self.pending);
                if let Some(end) = find_subslice(&self.paste_raw, PASTE_END) {
                    let consumed = end + PASTE_END.len();
                    let raw: Vec<_> = self.paste_raw.drain(..consumed).collect();
                    self.pending = std::mem::take(&mut self.paste_raw);
                    self.paste_mode = false;
                    if self.paste_overflow
                        || raw.len() > MAX_PASTE_BYTES + PASTE_START.len() + PASTE_END.len()
                    {
                        let strip_start = self.paste_start_pending;
                        self.paste_start_pending = false;
                        events.push(InputEvent {
                            kind: InputKind::PasteFragment {
                                strip_start,
                                strip_end: true,
                            },
                            raw,
                        });
                    } else {
                        let payload_end = raw.len() - PASTE_END.len();
                        let payload = raw[PASTE_START.len()..payload_end].to_vec();
                        events.push(InputEvent {
                            kind: InputKind::Paste(payload),
                            raw,
                        });
                    }
                    self.paste_overflow = false;
                    self.paste_start_pending = false;
                    continue;
                }

                if self.paste_raw.len() > MAX_PASTE_BYTES + PASTE_START.len() {
                    self.paste_overflow = true;
                }
                if self.paste_overflow && self.paste_raw.len() >= PASTE_END.len() {
                    let emit = self.paste_raw.len() - (PASTE_END.len() - 1);
                    let raw: Vec<_> = self.paste_raw.drain(..emit).collect();
                    let strip_start = self.paste_start_pending;
                    self.paste_start_pending = false;
                    events.push(InputEvent {
                        kind: InputKind::PasteFragment {
                            strip_start,
                            strip_end: false,
                        },
                        raw,
                    });
                }
                break;
            }

            match parse_one(&self.pending) {
                ParseResult::Event { consumed, kind } => {
                    let raw: Vec<_> = self.pending.drain(..consumed).collect();
                    if raw == PASTE_START {
                        self.paste_mode = true;
                        self.paste_overflow = false;
                        self.paste_start_pending = true;
                        self.paste_raw = raw;
                    } else {
                        events.push(InputEvent { kind, raw });
                    }
                }
                ParseResult::NeedMore => break,
            }
        }
        events
    }

    pub fn flush_ambiguous(&mut self) -> Option<InputEvent> {
        if self.paste_mode || self.pending.is_empty() {
            return None;
        }
        let raw = std::mem::take(&mut self.pending);
        let kind = if raw == b"\x1b" {
            InputKind::Escape
        } else {
            InputKind::Raw
        };
        Some(InputEvent { kind, raw })
    }

    /// Return bytes held while waiting for an ambiguous key sequence or a
    /// bracketed-paste terminator. Foreground applications own terminal input
    /// framing, so a hand-off must preserve these bytes without interpreting
    /// or delaying them further.
    pub fn take_buffered_raw(&mut self) -> Vec<u8> {
        let mut raw = std::mem::take(&mut self.paste_raw);
        raw.append(&mut self.pending);
        self.paste_mode = false;
        self.paste_overflow = false;
        self.paste_start_pending = false;
        raw
    }

    #[must_use]
    pub fn has_pending_ambiguity(&self) -> bool {
        !self.pending.is_empty() && !self.paste_mode
    }
}

enum ParseResult {
    Event { consumed: usize, kind: InputKind },
    NeedMore,
}

fn parse_one(bytes: &[u8]) -> ParseResult {
    let Some(&first) = bytes.first() else {
        return ParseResult::NeedMore;
    };
    if first == 0x1b {
        return parse_escape(bytes);
    }
    let control = match first {
        b'\r' | b'\n' => Some(InputKind::Enter),
        b'\t' => Some(InputKind::Tab),
        0x7f | 0x08 => Some(InputKind::Backspace),
        0x03 => Some(InputKind::CtrlC),
        0x04 => Some(InputKind::CtrlD),
        0x01 => Some(InputKind::CtrlA),
        0x05 => Some(InputKind::CtrlE),
        0x0b => Some(InputKind::CtrlK),
        0x0c => Some(InputKind::CtrlL),
        0x12 => Some(InputKind::CtrlR),
        0x15 => Some(InputKind::CtrlU),
        0x17 => Some(InputKind::CtrlW),
        0x00..=0x1f => Some(InputKind::Raw),
        _ => None,
    };
    if let Some(kind) = control {
        return ParseResult::Event { consumed: 1, kind };
    }

    let width = utf8_sequence_len(first);
    if width == 0 {
        return ParseResult::Event {
            consumed: 1,
            kind: InputKind::Raw,
        };
    }
    if bytes.len() < width {
        return ParseResult::NeedMore;
    }
    match std::str::from_utf8(&bytes[..width]) {
        Ok(text) => ParseResult::Event {
            consumed: width,
            kind: InputKind::Text(text.to_owned()),
        },
        Err(_) => ParseResult::Event {
            consumed: 1,
            kind: InputKind::Raw,
        },
    }
}

fn parse_escape(bytes: &[u8]) -> ParseResult {
    const SEQUENCES: &[(&[u8], InputKind)] = &[
        (b"\x1b[A", InputKind::Up),
        (b"\x1b[B", InputKind::Down),
        (b"\x1b[C", InputKind::Right),
        (b"\x1b[D", InputKind::Left),
        (b"\x1bOA", InputKind::Up),
        (b"\x1bOB", InputKind::Down),
        (b"\x1bOC", InputKind::Right),
        (b"\x1bOD", InputKind::Left),
        (b"\x1b[H", InputKind::Home),
        (b"\x1b[F", InputKind::End),
        (b"\x1b[1~", InputKind::Home),
        (b"\x1b[4~", InputKind::End),
        (b"\x1b[7~", InputKind::Home),
        (b"\x1b[8~", InputKind::End),
        (b"\x1bOH", InputKind::Home),
        (b"\x1bOF", InputKind::End),
        (b"\x1b[5~", InputKind::PageUp),
        (b"\x1b[6~", InputKind::PageDown),
        (b"\x1b[3~", InputKind::Delete),
        (b"\x1b[Z", InputKind::BackTab),
        (PASTE_START, InputKind::Raw),
    ];
    for (sequence, kind) in SEQUENCES {
        if bytes.starts_with(sequence) {
            return ParseResult::Event {
                consumed: sequence.len(),
                kind: kind.clone(),
            };
        }
    }
    if bytes.len() == 1
        || SEQUENCES
            .iter()
            .any(|(sequence, _)| sequence.starts_with(bytes))
    {
        ParseResult::NeedMore
    } else {
        ParseResult::Event {
            consumed: 2,
            kind: InputKind::Raw,
        }
    }
}

const fn utf8_sequence_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_utf8_and_paste_work_at_every_split() {
        for fixture in [
            b"\x1b[A".as_slice(),
            b"\x1bOA".as_slice(),
            "中".as_bytes(),
            "粘贴中文🙂e\u{301}👩‍💻".as_bytes(),
            b"\x1b[200~hello\x1b[201~".as_slice(),
            "\x1b[200~粘贴中文🙂e\u{301}👩‍💻\x1b[201~".as_bytes(),
        ] {
            let expected = InputDecoder::default().feed(fixture);
            for split in 0..=fixture.len() {
                let mut decoder = InputDecoder::default();
                let mut actual = decoder.feed(&fixture[..split]);
                actual.extend(decoder.feed(&fixture[split..]));
                assert_eq!(actual, expected, "split {split} for {fixture:?}");
            }
        }
    }

    #[test]
    fn decodes_csi_and_application_cursor_arrows() {
        for (raw, expected) in [
            (b"\x1b[A".as_slice(), InputKind::Up),
            (b"\x1b[B".as_slice(), InputKind::Down),
            (b"\x1bOA".as_slice(), InputKind::Up),
            (b"\x1bOB".as_slice(), InputKind::Down),
        ] {
            let events = InputDecoder::default().feed(raw);
            assert_eq!(events.len(), 1, "sequence {raw:?}");
            assert_eq!(events[0].kind, expected, "sequence {raw:?}");
            assert_eq!(events[0].raw, raw, "sequence {raw:?}");
        }
    }

    #[test]
    fn decodes_common_navigation_and_editing_keys() {
        for (raw, expected) in [
            (b"\x1b[C".as_slice(), InputKind::Right),
            (b"\x1b[D".as_slice(), InputKind::Left),
            (b"\x1bOC".as_slice(), InputKind::Right),
            (b"\x1bOD".as_slice(), InputKind::Left),
            (b"\x1b[H".as_slice(), InputKind::Home),
            (b"\x1b[1~".as_slice(), InputKind::Home),
            (b"\x1b[7~".as_slice(), InputKind::Home),
            (b"\x1bOH".as_slice(), InputKind::Home),
            (b"\x1b[F".as_slice(), InputKind::End),
            (b"\x1b[4~".as_slice(), InputKind::End),
            (b"\x1b[8~".as_slice(), InputKind::End),
            (b"\x1bOF".as_slice(), InputKind::End),
            (b"\x1b[3~".as_slice(), InputKind::Delete),
            (b"\x7f".as_slice(), InputKind::Backspace),
            (b"\x1b[Z".as_slice(), InputKind::BackTab),
        ] {
            let events = InputDecoder::default().feed(raw);
            assert_eq!(events.len(), 1, "sequence {raw:?}");
            assert_eq!(events[0].kind, expected, "sequence {raw:?}");
            assert_eq!(events[0].raw, raw, "sequence {raw:?}");
        }
    }

    #[test]
    fn lone_escape_is_resolved_by_timeout_flush() {
        let mut decoder = InputDecoder::default();
        assert!(decoder.feed(b"\x1b").is_empty());
        assert_eq!(
            decoder.flush_ambiguous(),
            Some(InputEvent {
                kind: InputKind::Escape,
                raw: b"\x1b".to_vec(),
            })
        );
    }

    #[test]
    fn buffered_input_can_be_reclaimed_byte_exact_on_foreground_handoff() {
        let partial_paste = "\x1b[200~first\n第二行🙂".as_bytes();
        let mut decoder = InputDecoder::default();
        assert!(decoder.feed(partial_paste).is_empty());
        assert_eq!(decoder.take_buffered_raw(), partial_paste);

        let events = decoder.feed(b"x");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].raw, b"x");

        assert!(decoder.feed(b"\x1b").is_empty());
        assert_eq!(decoder.take_buffered_raw(), b"\x1b");
        assert!(!decoder.has_pending_ambiguity());
    }

    #[test]
    fn paste_is_one_event_and_preserves_raw_bytes() {
        let raw = b"\x1b[200~a\nb\x1b[201~";
        let events = InputDecoder::default().feed(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, InputKind::Paste(b"a\nb".to_vec()));
        assert_eq!(events[0].raw, raw);
    }

    #[test]
    fn one_mib_paste_is_one_event_across_input_batches() {
        let payload = vec![b'x'; MAX_PASTE_BYTES];
        let mut decoder = InputDecoder::default();
        let mut events = decoder.feed(PASTE_START);
        for chunk in payload.chunks(16 * 1024) {
            events.extend(decoder.feed(chunk));
        }
        events.extend(decoder.feed(PASTE_END));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, InputKind::Paste(payload));
        assert_eq!(events[0].raw.len(), MAX_PASTE_BYTES + 12);
    }

    #[test]
    fn oversized_paste_streams_without_interpreting_payload() {
        let mut raw = Vec::with_capacity(MAX_PASTE_BYTES + 32);
        raw.extend_from_slice(PASTE_START);
        raw.extend(std::iter::repeat_n(b'x', MAX_PASTE_BYTES + 1));
        raw.extend_from_slice(b"\r\x03\x1b[A");
        raw.extend_from_slice(PASTE_END);
        raw.extend_from_slice(b"after");

        let mut decoder = InputDecoder::default();
        let mut events = Vec::new();
        for chunk in raw.chunks(16 * 1024) {
            events.extend(decoder.feed(chunk));
        }

        let first_text = events
            .iter()
            .position(|event| matches!(event.kind, InputKind::Text(_)))
            .expect("bytes after the paste should be decoded normally");
        assert!(
            events[..first_text]
                .iter()
                .all(|event| matches!(event.kind, InputKind::PasteFragment { .. }))
        );
        assert!(matches!(
            events.first().map(|event| &event.kind),
            Some(InputKind::PasteFragment {
                strip_start: true,
                ..
            })
        ));
        assert!(matches!(
            events.get(first_text - 1).map(|event| &event.kind),
            Some(InputKind::PasteFragment {
                strip_end: true,
                ..
            })
        ));
        let passthrough: Vec<_> = events[..first_text]
            .iter()
            .flat_map(|event| event.raw.iter().copied())
            .collect();
        let paste_end = raw.len() - b"after".len();
        assert_eq!(passthrough, raw[..paste_end]);
        assert_eq!(
            events[first_text..]
                .iter()
                .flat_map(|event| event.raw.iter().copied())
                .collect::<Vec<_>>(),
            b"after"
        );
    }

    #[test]
    fn oversized_complete_paste_cannot_bypass_the_limit_in_one_feed() {
        let mut raw = Vec::with_capacity(MAX_PASTE_BYTES + 13);
        raw.extend_from_slice(PASTE_START);
        raw.extend(std::iter::repeat_n(b'x', MAX_PASTE_BYTES + 1));
        raw.extend_from_slice(PASTE_END);
        let events = InputDecoder::default().feed(&raw);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].kind,
            InputKind::PasteFragment {
                strip_start: true,
                strip_end: true,
            }
        );
        assert_eq!(events[0].raw, raw);
    }

    #[test]
    fn decodes_standard_emacs_editing_controls() {
        let events = InputDecoder::default().feed(&[0x01, 0x05, 0x0b, 0x0c, 0x15, 0x17]);
        assert_eq!(
            events
                .into_iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                InputKind::CtrlA,
                InputKind::CtrlE,
                InputKind::CtrlK,
                InputKind::CtrlL,
                InputKind::CtrlU,
                InputKind::CtrlW,
            ]
        );
    }
}
