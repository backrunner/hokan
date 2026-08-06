use std::fmt;

use crc32fast::Hasher;

use super::{BoundaryId, SafeBoundaryScanner};

const MARKER_PREFIX: &[u8] = b"\x1b]6973;hokan;1;";
const MARKER_MAX_BYTES: usize = 256;

#[derive(Clone, Eq, PartialEq)]
pub struct SessionToken(String);

impl SessionToken {
    pub fn generate() -> crate::Result<Self> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|error| crate::Error::TerminalProtocol(error.to_string()))?;
        let mut value = String::with_capacity(32);
        for byte in bytes {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}")
                .map_err(|error| crate::Error::TerminalProtocol(error.to_string()))?;
        }
        Self::parse(value)
    }

    pub fn parse(value: impl Into<String>) -> crate::Result<Self> {
        let value = value.into();
        let valid = value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !valid {
            return Err(crate::Error::TerminalProtocol(
                "session token must be 32 lowercase hexadecimal characters".into(),
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionToken(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderBoundaryEvent {
    PromptRendered { boundary_id: BoundaryId },
    PostRedisplay { boundary_id: BoundaryId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundaryAt {
    pub passthrough_offset: usize,
    pub event: RenderBoundaryEvent,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DecodedChildOutput {
    pub passthrough: Vec<u8>,
    pub boundaries: Vec<BoundaryAt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellPhase {
    Prompt,
    Foreground,
}

#[derive(Debug)]
pub struct RenderBoundaryDecoder {
    token: SessionToken,
    scanner: SafeBoundaryScanner,
    candidate: Vec<u8>,
    next_prompt: BoundaryId,
    next_redisplay: BoundaryId,
    phase: ShellPhase,
}

enum MarkerOutcome {
    Event(RenderBoundaryEvent),
    Consume,
    Invalid,
}

impl RenderBoundaryDecoder {
    #[must_use]
    pub fn new(token: SessionToken) -> Self {
        Self {
            token,
            scanner: SafeBoundaryScanner::default(),
            candidate: Vec::new(),
            next_prompt: BoundaryId::new(1),
            next_redisplay: BoundaryId::new(1),
            phase: ShellPhase::Prompt,
        }
    }

    pub fn set_foreground(&mut self, foreground: bool) {
        self.phase = if foreground {
            ShellPhase::Foreground
        } else {
            ShellPhase::Prompt
        };
    }

    /// A fresh prompt announced over the trusted control channel is the one
    /// place a wedge may be cleared: drop any withheld partial marker and heal
    /// a desynchronized scanner. Without this a crashed TUI that emitted an
    /// overlong/invalid sequence leaves the scanner `Desynchronized` forever,
    /// every later marker passes through invisibly, and the overlay never
    /// recovers. The withheld fragment is DROPPED, not flushed: it is an
    /// unterminated control string the screen model never saw, so writing it
    /// through would put the real terminal into string-swallow mode while the
    /// model disagrees about what is on screen.
    pub fn reset_at_trusted_boundary(&mut self) {
        self.scanner.reset_at_trusted_boundary();
        self.candidate.clear();
    }

    pub fn feed(&mut self, bytes: &[u8]) -> DecodedChildOutput {
        let mut decoded = DecodedChildOutput::default();
        for &byte in bytes {
            self.push_byte(byte, &mut decoded);
        }
        decoded
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let pending = std::mem::take(&mut self.candidate);
        self.scanner.feed(&pending);
        pending
    }

    fn push_byte(&mut self, byte: u8, decoded: &mut DecodedChildOutput) {
        if self.candidate.is_empty() {
            if self.scanner.is_safe() && byte == 0x1b {
                self.candidate.push(byte);
            } else {
                decoded.passthrough.push(byte);
                self.scanner.feed(std::slice::from_ref(&byte));
            }
            return;
        }

        self.candidate.push(byte);
        let prefix_len = self.candidate.len().min(MARKER_PREFIX.len());
        if self.candidate[..prefix_len] != MARKER_PREFIX[..prefix_len] {
            self.flush_candidate(decoded);
            return;
        }

        if self.candidate.len() > MARKER_MAX_BYTES {
            self.flush_candidate(decoded);
            return;
        }

        if self.candidate.len() >= 2 && self.candidate.ends_with(b"\x1b\\") {
            match self.parse_complete_marker() {
                MarkerOutcome::Event(event) => {
                    decoded.boundaries.push(BoundaryAt {
                        passthrough_offset: decoded.passthrough.len(),
                        event,
                    });
                    self.candidate.clear();
                }
                MarkerOutcome::Consume => self.candidate.clear(),
                MarkerOutcome::Invalid => self.flush_candidate(decoded),
            }
        }
    }

    fn flush_candidate(&mut self, decoded: &mut DecodedChildOutput) {
        let candidate = std::mem::take(&mut self.candidate);
        self.scanner.feed(&candidate);
        decoded.passthrough.extend_from_slice(&candidate);
    }

    fn parse_complete_marker(&mut self) -> MarkerOutcome {
        if !self.candidate.starts_with(MARKER_PREFIX) || !self.candidate.ends_with(b"\x1b\\") {
            return MarkerOutcome::Invalid;
        }

        let body = &self.candidate[MARKER_PREFIX.len()..self.candidate.len() - 2];
        let Ok(body) = std::str::from_utf8(body) else {
            return MarkerOutcome::Invalid;
        };
        let mut fields = body.split(';');
        let Some(token) = fields.next() else {
            return MarkerOutcome::Invalid;
        };
        let Some(kind) = fields.next() else {
            return MarkerOutcome::Invalid;
        };
        let Some(id) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
            return MarkerOutcome::Invalid;
        };
        let Some(checksum) = fields.next() else {
            return MarkerOutcome::Invalid;
        };
        if fields.next().is_some() || token != self.token.as_str() || checksum.len() != 8 {
            return MarkerOutcome::Invalid;
        }

        let payload = format!("1;{token};{kind}");
        let expected_checksum = checksum_for(payload.as_bytes());
        let Ok(parsed_checksum) = u32::from_str_radix(checksum, 16) else {
            return MarkerOutcome::Invalid;
        };
        if parsed_checksum != expected_checksum {
            return MarkerOutcome::Invalid;
        }

        let boundary_id = BoundaryId::new(id);
        match kind {
            // An AUTHENTICATED marker id ahead of the counter means markers
            // were swallowed while the scanner was desynchronized (or raced
            // their control message): fast-forward instead of rejecting, or
            // the counter would never resynchronize. Ids behind the counter
            // are replays (the prompt reprints its marker on redisplay).
            "prompt" if boundary_id >= self.next_prompt => {
                let Some(next) = boundary_id.checked_next() else {
                    return MarkerOutcome::Invalid;
                };
                self.next_prompt = next;
                self.phase = ShellPhase::Prompt;
                MarkerOutcome::Event(RenderBoundaryEvent::PromptRendered { boundary_id })
            }
            "prompt" => MarkerOutcome::Consume,
            "redisplay" if boundary_id >= self.next_redisplay => {
                let Some(next) = boundary_id.checked_next() else {
                    return MarkerOutcome::Invalid;
                };
                self.next_redisplay = next;
                if self.phase == ShellPhase::Prompt {
                    MarkerOutcome::Event(RenderBoundaryEvent::PostRedisplay { boundary_id })
                } else {
                    MarkerOutcome::Consume
                }
            }
            "redisplay" => MarkerOutcome::Consume,
            _ => MarkerOutcome::Invalid,
        }
    }
}

#[must_use]
pub fn encode_marker(token: &SessionToken, event: RenderBoundaryEvent) -> Vec<u8> {
    let (kind, id) = match event {
        RenderBoundaryEvent::PromptRendered { boundary_id } => ("prompt", boundary_id.get()),
        RenderBoundaryEvent::PostRedisplay { boundary_id } => ("redisplay", boundary_id.get()),
    };
    let checksum = marker_checksum(token, kind);
    format!(
        "\x1b]6973;hokan;1;{};{kind};{id};{checksum:08x}\x1b\\",
        token.as_str()
    )
    .into_bytes()
}

#[must_use]
pub(crate) fn marker_checksum(token: &SessionToken, kind: &str) -> u32 {
    checksum_for(format!("1;{};{kind}", token.as_str()).as_bytes())
}

fn checksum_for(bytes: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> SessionToken {
        SessionToken::parse("0123456789abcdef0123456789abcdef")
            .expect("fixture token should be valid")
    }

    #[test]
    fn valid_marker_is_consumed_at_every_split() {
        let marker = encode_marker(
            &token(),
            RenderBoundaryEvent::PromptRendered {
                boundary_id: BoundaryId::new(1),
            },
        );

        for split in 0..=marker.len() {
            let mut decoder = RenderBoundaryDecoder::new(token());
            let first = decoder.feed(&marker[..split]);
            let second = decoder.feed(&marker[split..]);
            assert!(first.passthrough.is_empty());
            assert!(second.passthrough.is_empty());
            let events: Vec<_> = first
                .boundaries
                .into_iter()
                .chain(second.boundaries)
                .map(|boundary| boundary.event)
                .collect();
            assert_eq!(
                events,
                vec![RenderBoundaryEvent::PromptRendered {
                    boundary_id: BoundaryId::new(1)
                }]
            );
        }
    }

    #[test]
    fn wrong_token_checksum_and_phase_are_byte_exact_passthrough() {
        let valid = encode_marker(
            &token(),
            RenderBoundaryEvent::PostRedisplay {
                boundary_id: BoundaryId::new(1),
            },
        );
        let wrong_token = valid
            .windows(32)
            .position(|window| window == token().as_str().as_bytes())
            .expect("token should be present");
        let mut altered = valid.clone();
        altered[wrong_token] = b'f';

        for bytes in [&altered[..], b"\x1b]6973;hokan;1;broken\x1b\\"] {
            let mut decoder = RenderBoundaryDecoder::new(token());
            let output = decoder.feed(bytes);
            assert_eq!(output.passthrough, bytes);
            assert!(output.boundaries.is_empty());
        }

        let mut foreground = RenderBoundaryDecoder::new(token());
        foreground.set_foreground(true);
        let discarded = foreground.feed(&valid);
        assert!(discarded.passthrough.is_empty());
        assert!(discarded.boundaries.is_empty());

        let prompt = encode_marker(
            &token(),
            RenderBoundaryEvent::PromptRendered {
                boundary_id: BoundaryId::new(1),
            },
        );
        let mut duplicate = RenderBoundaryDecoder::new(token());
        assert_eq!(duplicate.feed(&prompt).boundaries.len(), 1);
        let repeated = duplicate.feed(&prompt);
        assert!(repeated.passthrough.is_empty());
        assert!(repeated.boundaries.is_empty());
    }

    #[test]
    fn authenticated_id_gaps_fast_forward_and_replays_are_consumed() {
        let mut decoder = RenderBoundaryDecoder::new(token());
        // Markers 1-4 were swallowed while the scanner was desynchronized:
        // the next authenticated marker fast-forwards the counter instead of
        // being rejected as out of order.
        let marker = encode_marker(
            &token(),
            RenderBoundaryEvent::PostRedisplay {
                boundary_id: BoundaryId::new(5),
            },
        );
        assert_eq!(decoder.feed(&marker).boundaries.len(), 1);
        // A replay behind the counter is consumed silently.
        let replayed = decoder.feed(&marker);
        assert!(replayed.passthrough.is_empty());
        assert!(replayed.boundaries.is_empty());
        // The counter keeps advancing from the fast-forwarded position.
        let next = encode_marker(
            &token(),
            RenderBoundaryEvent::PostRedisplay {
                boundary_id: BoundaryId::new(7),
            },
        );
        assert_eq!(decoder.feed(&next).boundaries.len(), 1);
    }

    #[test]
    fn marker_bytes_inside_another_control_string_are_not_consumed() {
        let marker = encode_marker(
            &token(),
            RenderBoundaryEvent::PromptRendered {
                boundary_id: BoundaryId::new(1),
            },
        );
        let mut wrapped = b"\x1bPouter:".to_vec();
        wrapped.extend_from_slice(&marker);
        wrapped.extend_from_slice(b"\x1b\\");

        let mut decoder = RenderBoundaryDecoder::new(token());
        let output = decoder.feed(&wrapped);
        assert_eq!(output.passthrough, wrapped);
        assert!(output.boundaries.is_empty());
    }

    #[test]
    fn unfinished_candidate_is_returned_on_stream_close() {
        let mut decoder = RenderBoundaryDecoder::new(token());
        assert!(decoder.feed(b"\x1b]6973;").passthrough.is_empty());
        assert_eq!(decoder.finish(), b"\x1b]6973;");
    }

    #[test]
    fn trusted_boundary_reset_heals_desynchronization_and_reanchors_ids() {
        let mut decoder = RenderBoundaryDecoder::new(token());
        // An invalid raw byte desynchronizes the scanner; while desynced every
        // marker — even a valid, authenticated one — passes through invisibly.
        assert!(decoder.feed(&[0xf5]).boundaries.is_empty());
        let lost = encode_marker(
            &token(),
            RenderBoundaryEvent::PromptRendered {
                boundary_id: BoundaryId::new(1),
            },
        );
        let swallowed = decoder.feed(&lost);
        assert_eq!(swallowed.passthrough, lost);
        assert!(swallowed.boundaries.is_empty());

        // The shell moved on: its next prompt carries id 2. The reset heals
        // the scanner, and the authenticated id gap fast-forwards the
        // counter, so the marker decodes.
        let marker = encode_marker(
            &token(),
            RenderBoundaryEvent::PromptRendered {
                boundary_id: BoundaryId::new(2),
            },
        );
        decoder.reset_at_trusted_boundary();
        let decoded = decoder.feed(&marker);
        assert!(decoded.passthrough.is_empty());
        let events: Vec<_> = decoded
            .boundaries
            .into_iter()
            .map(|boundary| boundary.event)
            .collect();
        assert_eq!(
            events,
            vec![RenderBoundaryEvent::PromptRendered {
                boundary_id: BoundaryId::new(2)
            }]
        );
    }

    #[test]
    fn trusted_boundary_reset_drops_withheld_partial_marker() {
        let marker = encode_marker(
            &token(),
            RenderBoundaryEvent::PromptRendered {
                boundary_id: BoundaryId::new(1),
            },
        );
        let mut decoder = RenderBoundaryDecoder::new(token());
        // Withhold the marker head, then drop it at the trusted boundary: the
        // remaining bytes must not reassemble into a marker afterwards.
        let split = MARKER_PREFIX.len() + 4;
        assert!(decoder.feed(&marker[..split]).passthrough.is_empty());
        decoder.reset_at_trusted_boundary();
        let rest = decoder.feed(&marker[split..]);
        assert_eq!(rest.passthrough, marker[split..]);
        assert!(rest.boundaries.is_empty());
    }
}
