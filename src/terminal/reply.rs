use std::time::{Duration, Instant};

use super::{CellPos, QueryId, SyncOutputCapability};

const MAX_REPLY_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalQueryKind {
    CursorPosition,
    SynchronizedOutput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredQuery {
    pub id: QueryId,
    pub bytes: &'static [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalReply {
    CursorPosition {
        query_id: QueryId,
        position: CellPos,
    },
    SynchronizedOutput {
        query_id: QueryId,
        raw_status: u8,
        capability: SyncOutputCapability,
    },
    Timeout {
        query_id: QueryId,
        kind: TerminalQueryKind,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RoutedInput {
    pub input: Vec<u8>,
    pub replies: Vec<TerminalReply>,
}

#[derive(Clone, Copy, Debug)]
struct OutstandingQuery {
    id: QueryId,
    kind: TerminalQueryKind,
    deadline: Instant,
}

#[derive(Debug, Default)]
pub struct TerminalReplyRouter {
    next_query_id: QueryId,
    outstanding: Option<OutstandingQuery>,
    candidate: Vec<u8>,
}

impl TerminalReplyRouter {
    #[must_use]
    pub const fn has_outstanding(&self) -> bool {
        self.outstanding.is_some()
    }

    pub fn register(
        &mut self,
        kind: TerminalQueryKind,
        now: Instant,
        timeout: Duration,
    ) -> crate::Result<RegisteredQuery> {
        if self.outstanding.is_some() {
            return Err(crate::Error::TerminalProtocol(
                "a terminal query is already outstanding".into(),
            ));
        }
        let id = self
            .next_query_id
            .checked_next()
            .ok_or_else(|| crate::Error::TerminalProtocol("query id exhausted".into()))?;
        self.next_query_id = id;
        self.outstanding = Some(OutstandingQuery {
            id,
            kind,
            deadline: now + timeout,
        });

        let bytes = match kind {
            TerminalQueryKind::CursorPosition => b"\x1b[6n" as &'static [u8],
            TerminalQueryKind::SynchronizedOutput => b"\x1b[?2026$p",
        };
        Ok(RegisteredQuery { id, bytes })
    }

    pub fn route(&mut self, bytes: &[u8], now: Instant) -> RoutedInput {
        let mut routed = self.expire(now);
        for &byte in bytes {
            self.push_byte(byte, &mut routed);
        }
        routed
    }

    pub fn expire(&mut self, now: Instant) -> RoutedInput {
        let mut routed = RoutedInput::default();
        let Some(outstanding) = self.outstanding else {
            if !self.candidate.is_empty() {
                routed.input.append(&mut self.candidate);
            }
            return routed;
        };
        if now < outstanding.deadline {
            return routed;
        }

        routed.input.append(&mut self.candidate);
        routed.replies.push(TerminalReply::Timeout {
            query_id: outstanding.id,
            kind: outstanding.kind,
        });
        self.outstanding = None;
        routed
    }

    pub fn cancel(&mut self) -> Vec<u8> {
        self.outstanding = None;
        std::mem::take(&mut self.candidate)
    }

    fn push_byte(&mut self, byte: u8, routed: &mut RoutedInput) {
        let Some(outstanding) = self.outstanding else {
            routed.input.push(byte);
            return;
        };

        if self.candidate.is_empty() && byte != 0x1b {
            routed.input.push(byte);
            return;
        }
        self.candidate.push(byte);
        if self.candidate.len() > MAX_REPLY_BYTES {
            routed.input.append(&mut self.candidate);
            return;
        }

        match classify(outstanding.kind, &self.candidate) {
            CandidateState::Potential => {}
            CandidateState::Invalid => routed.input.append(&mut self.candidate),
            CandidateState::Complete(parsed) => {
                let reply = match parsed {
                    ParsedReply::Cursor(position) => TerminalReply::CursorPosition {
                        query_id: outstanding.id,
                        position,
                    },
                    ParsedReply::SyncStatus(raw_status) => TerminalReply::SynchronizedOutput {
                        query_id: outstanding.id,
                        raw_status,
                        capability: capability_for_status(raw_status),
                    },
                };
                self.candidate.clear();
                self.outstanding = None;
                routed.replies.push(reply);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParsedReply {
    Cursor(CellPos),
    SyncStatus(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateState {
    Potential,
    Complete(ParsedReply),
    Invalid,
}

fn classify(kind: TerminalQueryKind, bytes: &[u8]) -> CandidateState {
    match kind {
        TerminalQueryKind::CursorPosition => classify_cpr(bytes),
        TerminalQueryKind::SynchronizedOutput => classify_sync_status(bytes),
    }
}

fn classify_cpr(bytes: &[u8]) -> CandidateState {
    const PREFIX: &[u8] = b"\x1b[";
    if bytes.len() <= PREFIX.len() {
        return if PREFIX.starts_with(bytes) {
            CandidateState::Potential
        } else {
            CandidateState::Invalid
        };
    }
    if !bytes.starts_with(PREFIX) {
        return CandidateState::Invalid;
    }

    let body = &bytes[PREFIX.len()..];
    if body.last() == Some(&b'R') {
        let numbers = &body[..body.len() - 1];
        let Some(separator) = numbers.iter().position(|byte| *byte == b';') else {
            return CandidateState::Invalid;
        };
        if numbers[separator + 1..].contains(&b';') {
            return CandidateState::Invalid;
        }
        let row = parse_positive_u16(&numbers[..separator]);
        let col = parse_positive_u16(&numbers[separator + 1..]);
        return match (row, col) {
            (Some(row), Some(col)) => CandidateState::Complete(ParsedReply::Cursor(CellPos {
                row: row - 1,
                col: col - 1,
            })),
            _ => CandidateState::Invalid,
        };
    }

    let semicolons = body.iter().filter(|byte| **byte == b';').count();
    if semicolons <= 1
        && body
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
    {
        CandidateState::Potential
    } else {
        CandidateState::Invalid
    }
}

fn classify_sync_status(bytes: &[u8]) -> CandidateState {
    const PREFIX: &[u8] = b"\x1b[?2026;";
    if bytes.len() <= PREFIX.len() {
        return if PREFIX.starts_with(bytes) {
            CandidateState::Potential
        } else {
            CandidateState::Invalid
        };
    }
    if !bytes.starts_with(PREFIX) {
        return CandidateState::Invalid;
    }

    let suffix = &bytes[PREFIX.len()..];
    match suffix {
        [status] if (b'0'..=b'4').contains(status) => CandidateState::Potential,
        [status, b'$'] if (b'0'..=b'4').contains(status) => CandidateState::Potential,
        [status, b'$', b'y'] if (b'0'..=b'4').contains(status) => {
            CandidateState::Complete(ParsedReply::SyncStatus(status - b'0'))
        }
        _ => CandidateState::Invalid,
    }
}

fn parse_positive_u16(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value = std::str::from_utf8(bytes).ok()?.parse::<u16>().ok()?;
    (value > 0).then_some(value)
}

const fn capability_for_status(status: u8) -> SyncOutputCapability {
    match status {
        2 => SyncOutputCapability::AvailableIdle,
        1 | 3 => SyncOutputCapability::BusyExternal,
        _ => SyncOutputCapability::UnsupportedFallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpr_is_consumed_for_every_chunk_split() {
        let reply = b"\x1b[12;34R";
        for split in 0..=reply.len() {
            let now = Instant::now();
            let mut router = TerminalReplyRouter::default();
            let registration = router
                .register(
                    TerminalQueryKind::CursorPosition,
                    now,
                    Duration::from_secs(1),
                )
                .expect("query should register");
            let first = router.route(&reply[..split], now);
            let second = router.route(&reply[split..], now);
            assert!(first.input.is_empty());
            assert!(second.input.is_empty());
            let replies: Vec<_> = first.replies.into_iter().chain(second.replies).collect();
            assert_eq!(
                replies,
                vec![TerminalReply::CursorPosition {
                    query_id: registration.id,
                    position: CellPos::new(11, 33),
                }]
            );
        }
    }

    #[test]
    fn sync_status_maps_all_protocol_values() {
        let expected = [
            SyncOutputCapability::UnsupportedFallback,
            SyncOutputCapability::BusyExternal,
            SyncOutputCapability::AvailableIdle,
            SyncOutputCapability::BusyExternal,
            SyncOutputCapability::UnsupportedFallback,
        ];
        for (status, capability) in expected.into_iter().enumerate() {
            let now = Instant::now();
            let mut router = TerminalReplyRouter::default();
            router
                .register(
                    TerminalQueryKind::SynchronizedOutput,
                    now,
                    Duration::from_secs(1),
                )
                .expect("query should register");
            let bytes = format!("\x1b[?2026;{status}$y");
            let routed = router.route(bytes.as_bytes(), now);
            assert!(matches!(
                routed.replies.as_slice(),
                [TerminalReply::SynchronizedOutput {
                    capability: actual,
                    ..
                }] if *actual == capability
            ));
        }
    }

    #[test]
    fn malformed_reply_and_timeout_restore_input_bytes() {
        let now = Instant::now();
        let mut malformed = TerminalReplyRouter::default();
        malformed
            .register(
                TerminalQueryKind::CursorPosition,
                now,
                Duration::from_secs(1),
            )
            .expect("query should register");
        let routed = malformed.route(b"\x1b[Ahello", now);
        assert_eq!(routed.input, b"\x1b[Ahello");
        assert!(routed.replies.is_empty());

        let mut timed_out = TerminalReplyRouter::default();
        let registration = timed_out
            .register(
                TerminalQueryKind::CursorPosition,
                now,
                Duration::from_millis(1),
            )
            .expect("query should register");
        assert!(timed_out.route(b"\x1b[12", now).input.is_empty());
        let routed = timed_out.expire(now + Duration::from_millis(2));
        assert_eq!(routed.input, b"\x1b[12");
        assert_eq!(
            routed.replies,
            vec![TerminalReply::Timeout {
                query_id: registration.id,
                kind: TerminalQueryKind::CursorPosition,
            }]
        );
    }
}
