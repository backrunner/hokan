use std::time::{Duration, Instant};

use super::{CellPos, QueryId, SyncOutputCapability};

const MAX_REPLY_BYTES: usize = 64;
const LATE_REPLY_GRACE: Duration = Duration::from_secs(2);
const LATE_REPLY_PREFIX_TIMEOUT: Duration = Duration::from_millis(32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalQueryKind {
    CursorPosition,
    CursorPositionStandardGuarded,
    SynchronizedOutput,
}

impl TerminalQueryKind {
    /// Explicit name for the private DECXCPR behavior used by Hokan. The enum
    /// variant keeps its original public name for source compatibility.
    #[allow(non_upper_case_globals)]
    pub const CursorPositionPrivate: Self = Self::CursorPosition;
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

#[derive(Clone, Copy, Debug)]
struct LateQuery {
    kind: TerminalQueryKind,
    deadline: Instant,
}

#[derive(Debug, Default)]
pub struct TerminalReplyRouter {
    next_query_id: QueryId,
    outstanding: Option<OutstandingQuery>,
    late: Vec<LateQuery>,
    candidate: Vec<u8>,
    candidate_started: Option<Instant>,
    /// Whether the foreground child currently owns the PTY. Late Hokan
    /// replies are quarantined only while the shell owns input; a foreground
    /// TUI can issue the same private terminal queries itself.
    foreground: bool,
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
        self.late.retain(|query| now < query.deadline);
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
            // Standard CPR replies (`CSI row;col R`) collide byte-for-byte
            // with xterm's modified F3 keys (`CSI 1;modifier R`). DECXCPR
            // adds the private `?` marker, so user shortcuts can never be
            // consumed as Hokan's cursor response.
            TerminalQueryKind::CursorPositionPrivate => b"\x1b[?6n" as &'static [u8],
            // Some terminals implement standard CPR but not DECXCPR. Prefix
            // the fallback with a status DSR whose `CSI 0 n` response cannot
            // be produced by a modified function key, then require both
            // replies as one guarded cursor report.
            TerminalQueryKind::CursorPositionStandardGuarded => b"\x1b[5n\x1b[6n",
            TerminalQueryKind::SynchronizedOutput => b"\x1b[?2026$p",
        };
        Ok(RegisteredQuery { id, bytes })
    }

    /// Switch reply ownership along with the shell foreground state.
    ///
    /// Hokan probes are emitted just before a command is handed to the child,
    /// so an outstanding probe may legitimately finish after that hand-off.
    /// Keeping that one outstanding registration preserves ordering. Expired
    /// probes are released at the hand-off because a foreground TUI can issue
    /// the same terminal queries and their replies are indistinguishable.
    pub fn set_foreground(&mut self, foreground: bool) -> Vec<u8> {
        self.foreground = foreground;
        if !foreground {
            return Vec::new();
        }
        // A late candidate belongs to the foreground program once the hand-off
        // has happened. Only a partial reply for the one query that is still
        // outstanding remains ordered across the hand-off.
        self.late.clear();
        let belongs_to_outstanding = self.outstanding.is_some_and(|query| {
            matches!(
                classify(query.kind, &self.candidate),
                CandidateState::Potential
            )
        });
        if belongs_to_outstanding {
            return Vec::new();
        }
        self.candidate_started = None;
        std::mem::take(&mut self.candidate)
    }

    pub fn route(&mut self, bytes: &[u8], now: Instant) -> RoutedInput {
        let mut routed = self.expire(now);
        for &byte in bytes {
            self.push_byte(byte, now, &mut routed);
        }
        routed
    }

    pub fn expire(&mut self, now: Instant) -> RoutedInput {
        let mut routed = RoutedInput::default();
        self.late.retain(|query| now < query.deadline);
        if let Some(outstanding) = self.outstanding
            && now >= outstanding.deadline
        {
            routed.replies.push(TerminalReply::Timeout {
                query_id: outstanding.id,
                kind: outstanding.kind,
            });
            self.outstanding = None;
            if !self.foreground {
                self.remember_late(outstanding.kind, now);
            }
        }
        self.flush_stale_candidate(now, &mut routed);
        routed
    }

    pub fn cancel(&mut self) -> Vec<u8> {
        self.outstanding = None;
        self.late.clear();
        self.candidate_started = None;
        std::mem::take(&mut self.candidate)
    }

    fn push_byte(&mut self, byte: u8, now: Instant, routed: &mut RoutedInput) {
        self.late.retain(|query| now < query.deadline);
        if self.outstanding.is_none() && self.late.is_empty() {
            routed.input.push(byte);
            return;
        }

        if self.candidate.is_empty() && byte != 0x1b {
            routed.input.push(byte);
            return;
        }
        if self.candidate.is_empty() {
            self.candidate_started = Some(now);
        }
        self.candidate.push(byte);
        if self.candidate.len() > MAX_REPLY_BYTES {
            self.release_candidate_preserving_fresh_escape(now, routed);
            return;
        }

        let mut potential = false;
        if let Some(outstanding) = self.outstanding {
            match classify(outstanding.kind, &self.candidate) {
                CandidateState::Potential => potential = true,
                CandidateState::Invalid => {}
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
                    self.candidate_started = None;
                    self.outstanding = None;
                    routed.replies.push(reply);
                    return;
                }
            }
        }

        let mut late_complete = None;
        for (index, query) in self.late.iter().enumerate() {
            match classify(query.kind, &self.candidate) {
                CandidateState::Potential => potential = true,
                CandidateState::Invalid => {}
                CandidateState::Complete(_) => {
                    late_complete = Some(index);
                    break;
                }
            }
        }
        if let Some(index) = late_complete {
            self.late.swap_remove(index);
            self.candidate.clear();
            self.candidate_started = None;
            return;
        }

        if !potential {
            self.release_candidate_preserving_fresh_escape(now, routed);
        }
    }

    fn release_candidate_preserving_fresh_escape(
        &mut self,
        now: Instant,
        routed: &mut RoutedInput,
    ) {
        // The byte which invalidated one candidate may itself start the next
        // terminal reply. This matters for `ESC ESC [ ? ... R`: the first Esc
        // belongs to the user while the second begins a valid DECXCPR reply.
        // Release the invalid prefix, then keep the trailing Esc for the next
        // bytes instead of leaking the whole reply into the child.
        let fresh_escape = self.candidate.last() == Some(&0x1b);
        if fresh_escape {
            self.candidate.pop();
        }
        routed.input.append(&mut self.candidate);
        if fresh_escape {
            self.candidate.push(0x1b);
            self.candidate_started = Some(now);
        } else {
            self.candidate_started = None;
        }
    }

    fn remember_late(&mut self, kind: TerminalQueryKind, now: Instant) {
        self.late.retain(|query| query.kind != kind);
        self.late.push(LateQuery {
            kind,
            deadline: now + LATE_REPLY_GRACE,
        });
    }

    fn flush_stale_candidate(&mut self, now: Instant, routed: &mut RoutedInput) {
        if self.candidate.is_empty() {
            self.candidate_started = None;
            return;
        }
        let outstanding_potential = self.outstanding.is_some_and(|query| {
            matches!(
                classify(query.kind, &self.candidate),
                CandidateState::Potential
            )
        });
        let late_potential = self.late.iter().any(|query| {
            matches!(
                classify(query.kind, &self.candidate),
                CandidateState::Potential
            )
        });
        let prefix_expired = self.candidate_started.is_some_and(|started| {
            now.saturating_duration_since(started) >= LATE_REPLY_PREFIX_TIMEOUT
        });
        // A bare Escape/Ctrl-[ belongs to the user even if it is also a prefix
        // of Hokan's outstanding terminal reply. Bound that ambiguity to the
        // same short window used for late replies instead of delaying the key
        // until the full query timeout. The outstanding query remains
        // registered, so a subsequent complete reply can still be consumed.
        let escape_expired = self.candidate == b"\x1b" && prefix_expired;
        let outstanding_blocks = outstanding_potential && !escape_expired;
        let late_blocks = late_potential && !prefix_expired;
        if !outstanding_blocks && !late_blocks {
            routed.input.append(&mut self.candidate);
            self.candidate_started = None;
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
        TerminalQueryKind::CursorPositionPrivate => classify_cpr(bytes),
        TerminalQueryKind::CursorPositionStandardGuarded => classify_guarded_standard_cpr(bytes),
        TerminalQueryKind::SynchronizedOutput => classify_sync_status(bytes),
    }
}

fn classify_cpr(bytes: &[u8]) -> CandidateState {
    classify_cpr_with_prefix(bytes, b"\x1b[?")
}

fn classify_guarded_standard_cpr(bytes: &[u8]) -> CandidateState {
    classify_cpr_with_prefix(bytes, b"\x1b[0n\x1b[")
}

fn classify_cpr_with_prefix(bytes: &[u8], prefix: &[u8]) -> CandidateState {
    if bytes.len() <= prefix.len() {
        return if prefix.starts_with(bytes) {
            CandidateState::Potential
        } else {
            CandidateState::Invalid
        };
    }
    if !bytes.starts_with(prefix) {
        return CandidateState::Invalid;
    }

    let body = &bytes[prefix.len()..];
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
        let reply = b"\x1b[?12;34R";
        for split in 0..=reply.len() {
            let now = Instant::now();
            let mut router = TerminalReplyRouter::default();
            let registration = router
                .register(
                    TerminalQueryKind::CursorPositionPrivate,
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
    fn cursor_position_compatibility_name_uses_private_query_bytes() {
        let mut router = TerminalReplyRouter::default();
        let query = router
            .register(
                TerminalQueryKind::CursorPosition,
                Instant::now(),
                Duration::from_secs(1),
            )
            .expect("query should register");
        assert_eq!(query.bytes, b"\x1b[?6n");
    }

    #[test]
    fn guarded_standard_cpr_is_consumed_for_every_chunk_split() {
        let reply = b"\x1b[0n\x1b[12;34R";
        for split in 0..=reply.len() {
            let now = Instant::now();
            let mut router = TerminalReplyRouter::default();
            let registration = router
                .register(
                    TerminalQueryKind::CursorPositionStandardGuarded,
                    now,
                    Duration::from_secs(1),
                )
                .expect("query should register");
            assert_eq!(registration.bytes, b"\x1b[5n\x1b[6n");
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
    fn guarded_standard_cpr_never_consumes_modified_f3() {
        let started = Instant::now();
        let mut router = TerminalReplyRouter::default();
        let query = router
            .register(
                TerminalQueryKind::CursorPositionStandardGuarded,
                started,
                Duration::from_secs(1),
            )
            .expect("cursor query");

        for key in [
            b"\x1b[1;1R".as_slice(),
            b"\x1b[1;2R".as_slice(),
            b"\x1b[1;3R".as_slice(),
            b"\x1b[1;4R".as_slice(),
            b"\x1b[1;5R".as_slice(),
            b"\x1b[1;6R".as_slice(),
            b"\x1b[1;7R".as_slice(),
            b"\x1b[1;8R".as_slice(),
        ] {
            let routed = router.route(key, started);
            assert_eq!(routed.input, key);
            assert!(routed.replies.is_empty());
        }

        let routed = router.route(b"\x1b[0n\x1b[12;34R", started);
        assert!(routed.input.is_empty());
        assert_eq!(
            routed.replies,
            vec![TerminalReply::CursorPosition {
                query_id: query.id,
                position: CellPos::new(11, 33),
            }]
        );
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
                TerminalQueryKind::CursorPositionPrivate,
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
                TerminalQueryKind::CursorPositionPrivate,
                now,
                Duration::from_millis(1),
            )
            .expect("query should register");
        assert!(timed_out.route(b"\x1b[?12", now).input.is_empty());
        let routed = timed_out.expire(now + Duration::from_millis(2));
        assert!(routed.input.is_empty());
        assert_eq!(
            routed.replies,
            vec![TerminalReply::Timeout {
                query_id: registration.id,
                kind: TerminalQueryKind::CursorPositionPrivate,
            }]
        );
        let released = timed_out.expire(now + Duration::from_millis(2) + LATE_REPLY_PREFIX_TIMEOUT);
        assert_eq!(released.input, b"\x1b[?12");
    }

    #[test]
    fn late_sync_reply_is_discarded_while_a_cursor_query_is_outstanding() {
        let started = Instant::now();
        let mut router = TerminalReplyRouter::default();
        let sync = router
            .register(
                TerminalQueryKind::SynchronizedOutput,
                started,
                Duration::from_millis(1),
            )
            .expect("sync query");
        assert_eq!(
            router.expire(started + Duration::from_millis(2)).replies,
            vec![TerminalReply::Timeout {
                query_id: sync.id,
                kind: TerminalQueryKind::SynchronizedOutput,
            }]
        );

        let now = started + Duration::from_millis(2);
        let cursor = router
            .register(
                TerminalQueryKind::CursorPositionPrivate,
                now,
                Duration::from_secs(1),
            )
            .expect("cursor query");
        let routed = router.route(b"\x1b[?2026;2$y\x1b[?12;34Rhello", now);
        assert_eq!(routed.input, b"hello");
        assert_eq!(
            routed.replies,
            vec![TerminalReply::CursorPosition {
                query_id: cursor.id,
                position: CellPos::new(11, 33),
            }]
        );
    }

    #[test]
    fn late_reply_filter_releases_an_ambiguous_escape_key_quickly() {
        let started = Instant::now();
        let mut router = TerminalReplyRouter::default();
        router
            .register(
                TerminalQueryKind::SynchronizedOutput,
                started,
                Duration::from_millis(1),
            )
            .expect("sync query");
        let timed_out = started + Duration::from_millis(2);
        router.expire(timed_out);

        assert!(router.route(b"\x1b", timed_out).input.is_empty());
        let routed = router.expire(timed_out + LATE_REPLY_PREFIX_TIMEOUT);
        assert_eq!(routed.input, b"\x1b");
    }

    #[test]
    fn foreground_handoff_drops_late_reply_quarantine() {
        let started = Instant::now();
        let mut router = TerminalReplyRouter::default();
        router
            .register(
                TerminalQueryKind::CursorPositionPrivate,
                started,
                Duration::from_millis(1),
            )
            .expect("cursor query");
        assert!(router.set_foreground(true).is_empty());
        router.expire(started + Duration::from_millis(2));

        let child_reply = b"\x1b[?12;34Rpayload";
        let routed = router.route(child_reply, started + Duration::from_millis(2));
        assert_eq!(routed.input, child_reply);
        assert!(routed.replies.is_empty());

        let child_reply = b"\x1b[12;34Rpayload";
        let routed = router.route(child_reply, started + Duration::from_millis(2));
        assert_eq!(routed.input, child_reply);
        assert!(routed.replies.is_empty());
    }

    #[test]
    fn late_replies_are_released_after_a_foreground_handoff() {
        for (kind, hokan_reply, child_reply) in [
            (
                TerminalQueryKind::CursorPositionPrivate,
                b"\x1b[?12;34R".as_slice(),
                b"\x1b[?7;9R".as_slice(),
            ),
            (
                TerminalQueryKind::SynchronizedOutput,
                b"\x1b[?2026;2$y".as_slice(),
                b"\x1b[?2026;1$y".as_slice(),
            ),
        ] {
            let started = Instant::now();
            let mut router = TerminalReplyRouter::default();
            router
                .register(kind, started, Duration::from_millis(1))
                .expect("terminal query");
            router.expire(started + Duration::from_millis(2));
            assert!(router.set_foreground(true).is_empty());

            let mut replies = hokan_reply.to_vec();
            replies.extend_from_slice(child_reply);
            replies.extend_from_slice(b"payload");
            let routed = router.route(&replies, started + Duration::from_millis(2));
            let mut expected = hokan_reply.to_vec();
            expected.extend_from_slice(child_reply);
            expected.extend_from_slice(b"payload");
            assert_eq!(routed.input, expected);
            assert!(routed.replies.is_empty());
        }
    }

    #[test]
    fn foreground_handoff_keeps_a_partial_private_reply_ordered() {
        let started = Instant::now();
        let mut router = TerminalReplyRouter::default();
        router
            .register(
                TerminalQueryKind::CursorPositionPrivate,
                started,
                Duration::from_millis(1),
            )
            .expect("cursor query");
        assert!(router.route(b"\x1b[?12", started).input.is_empty());
        router.expire(started + Duration::from_millis(2));

        assert_eq!(router.set_foreground(true), b"\x1b[?12");
        let routed = router.route(b";34Rpayload", started + Duration::from_millis(2));
        assert_eq!(routed.input, b";34Rpayload");
        assert!(routed.replies.is_empty());
    }

    #[test]
    fn outstanding_probe_remains_ordered_across_foreground_handoff() {
        let started = Instant::now();
        let mut router = TerminalReplyRouter::default();
        let query = router
            .register(
                TerminalQueryKind::CursorPositionPrivate,
                started,
                Duration::from_secs(1),
            )
            .expect("cursor query");
        assert!(router.route(b"\x1b[?12", started).input.is_empty());
        assert!(router.set_foreground(true).is_empty());
        let delayed = started + LATE_REPLY_PREFIX_TIMEOUT;
        assert!(router.expire(delayed).input.is_empty());
        let routed = router.route(b";34R", delayed);
        assert_eq!(routed.input, b"");
        assert_eq!(
            routed.replies,
            vec![TerminalReply::CursorPosition {
                query_id: query.id,
                position: CellPos::new(11, 33),
            }]
        );
    }

    #[test]
    fn outstanding_probe_releases_a_bare_escape_after_the_prefix_window() {
        let started = Instant::now();
        let mut router = TerminalReplyRouter::default();
        let query = router
            .register(
                TerminalQueryKind::CursorPositionPrivate,
                started,
                Duration::from_secs(1),
            )
            .expect("cursor query");
        assert!(router.route(b"\x1b", started).input.is_empty());

        let released = router.expire(started + LATE_REPLY_PREFIX_TIMEOUT);
        assert_eq!(released.input, b"\x1b");
        assert!(released.replies.is_empty());

        let routed = router.route(b"\x1b[?12;34R", started + LATE_REPLY_PREFIX_TIMEOUT);
        assert!(routed.input.is_empty());
        assert_eq!(
            routed.replies,
            vec![TerminalReply::CursorPosition {
                query_id: query.id,
                position: CellPos::new(11, 33),
            }]
        );
    }

    #[test]
    fn query_timeout_does_not_restart_the_escape_prefix_window() {
        let started = Instant::now();
        let mut router = TerminalReplyRouter::default();
        router
            .register(
                TerminalQueryKind::CursorPositionPrivate,
                started,
                Duration::from_millis(16),
            )
            .expect("cursor query");
        assert!(router.route(b"\x1b", started).input.is_empty());

        let timed_out = router.expire(started + Duration::from_millis(16));
        assert!(timed_out.input.is_empty());
        assert!(matches!(
            timed_out.replies.as_slice(),
            [TerminalReply::Timeout { .. }]
        ));

        let released = router.expire(started + LATE_REPLY_PREFIX_TIMEOUT);
        assert_eq!(released.input, b"\x1b");
    }

    #[test]
    fn fresh_escape_after_an_invalid_candidate_can_start_a_reply() {
        for prefix in [b"\x1b".as_slice(), b"\x1b[?12".as_slice()] {
            let started = Instant::now();
            let mut router = TerminalReplyRouter::default();
            let query = router
                .register(
                    TerminalQueryKind::CursorPositionPrivate,
                    started,
                    Duration::from_secs(1),
                )
                .expect("cursor query");
            let mut bytes = prefix.to_vec();
            bytes.extend_from_slice(b"\x1b[?3;4R");

            let routed = router.route(&bytes, started);
            assert_eq!(routed.input, prefix);
            assert_eq!(
                routed.replies,
                vec![TerminalReply::CursorPosition {
                    query_id: query.id,
                    position: CellPos::new(2, 3),
                }]
            );
        }
    }

    #[test]
    fn keyboard_protocol_sequences_never_collide_with_private_cursor_reports() {
        let started = Instant::now();
        let mut router = TerminalReplyRouter::default();
        let query = router
            .register(
                TerminalQueryKind::CursorPositionPrivate,
                started,
                Duration::from_secs(1),
            )
            .expect("cursor query");

        for key in [
            b"\x00".as_slice(),
            b"\x03".as_slice(),
            b"\x11".as_slice(),
            b"\x13".as_slice(),
            b"\x1a".as_slice(),
            b"\x1c".as_slice(),
            b"\x1bx".as_slice(),
            b"\x1b[Z".as_slice(),
            b"\x1b[1;5A".as_slice(),
            b"\x1b[1;3D".as_slice(),
            b"\x1b[1;2H".as_slice(),
            b"\x1b[1;6F".as_slice(),
            b"\x1b[1;1R".as_slice(),
            b"\x1b[1;2R".as_slice(),
            b"\x1b[1;3R".as_slice(),
            b"\x1b[1;4R".as_slice(),
            b"\x1b[1;5R".as_slice(),
            b"\x1b[1;6R".as_slice(),
            b"\x1b[1;7R".as_slice(),
            b"\x1b[1;8R".as_slice(),
            b"\x1b[13;2u".as_slice(),
            b"\x1b[32;5u".as_slice(),
            b"\x1b[97;1:2u".as_slice(),
            b"\x1b[97;1:3u".as_slice(),
            b"\x1b[27;5;99~".as_slice(),
        ] {
            let mut forwarded = Vec::new();
            for byte in key {
                let routed = router.route(std::slice::from_ref(byte), started);
                forwarded.extend(routed.input);
                assert!(routed.replies.is_empty());
            }
            assert_eq!(forwarded, key);
        }

        let routed = router.route(b"\x1b[?12;34R", started);
        assert!(routed.input.is_empty());
        assert_eq!(
            routed.replies,
            vec![TerminalReply::CursorPosition {
                query_id: query.id,
                position: CellPos::new(11, 33),
            }]
        );

        let mut late = TerminalReplyRouter::default();
        late.register(
            TerminalQueryKind::CursorPositionPrivate,
            started,
            Duration::from_millis(1),
        )
        .expect("cursor query");
        late.expire(started + Duration::from_millis(2));
        let routed = late.route(b"\x1b[1;5R", started + Duration::from_millis(2));
        assert_eq!(routed.input, b"\x1b[1;5R");
        assert!(routed.replies.is_empty());
    }
}
