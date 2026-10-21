//! Color replies share stdin with keys. Track queries before they reach the
//! terminal and retain response framing across a foreground program's exit.
//! Never classify a bare color literal or bracketed paste as a reply.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 4096;
const MAX_QUERIES: usize = 256;
const QUERY_LIFETIME: Duration = Duration::from_secs(30);
const LATE_GRACE: Duration = Duration::from_secs(2);
const PREFIX_TIMEOUT: Duration = Duration::from_millis(32);
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Debug, Eq, PartialEq)]
enum ColorKey {
    Palette(u16),
    Dynamic(u8),
    Named(String),
}

#[derive(Debug)]
struct Query {
    key: ColorKey,
    owner: Option<u64>,
    deadline: Instant,
    retired: bool,
}

#[derive(Debug, Default)]
struct InputFrame {
    bytes: Vec<u8>,
    forwarded: usize,
    started: Option<Instant>,
    // Once attributed, ownership cannot change when the request expires.
    owner: Option<Option<u64>>,
    retired: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ColorReplyFilter {
    output: OscScanner,
    queries: VecDeque<Query>,
    generation: u64,
    foreground: Option<u64>,
    seen_prompt: bool,
    input: InputFrame,
    paste: bool,
    paste_tail: Vec<u8>,
}

impl ColorReplyFilter {
    pub(crate) fn begin_foreground(&mut self) {
        if self.foreground.is_none() {
            self.generation = self.generation.wrapping_add(1);
            self.foreground = Some(self.generation);
        }
    }

    /// Called at the authenticated prompt marker in the output byte stream,
    /// not at the independently delivered shell control message.
    pub(crate) fn prompt(&mut self, now: Instant) {
        // Enter queued before the first prompt belongs to the upcoming
        // command. Shell startup queries, however, are finished at that prompt.
        let preserve_queued_command = !self.seen_prompt;
        self.seen_prompt = true;
        for query in &mut self.queries {
            if !query.retired && (query.owner.is_none() || !preserve_queued_command) {
                query.retired = true;
                query.deadline = now + LATE_GRACE;
            }
        }
        if self
            .input
            .owner
            .is_some_and(|owner| owner.is_none() || !preserve_queued_command)
        {
            self.input.retired = true;
        }
        if !preserve_queued_command {
            self.foreground = None;
        }
        // An unterminated child control string must not hide shell queries.
        self.output = OscScanner::default();
    }

    pub(crate) fn observe_output(&mut self, bytes: &[u8], now: Instant) {
        self.queries.retain(|query| now < query.deadline);
        for &byte in bytes {
            if let Some(body) = self.output.push(byte) {
                for key in color_keys(&body, true) {
                    if self.queries.len() == MAX_QUERIES {
                        self.queries.pop_front();
                    }
                    self.queries.push_back(Query {
                        key,
                        owner: self.foreground,
                        deadline: now + QUERY_LIFETIME,
                        retired: false,
                    });
                }
            }
        }
    }

    pub(crate) fn route(&mut self, bytes: &[u8], foreground: bool, now: Instant) -> Vec<u8> {
        let mut output = self.expire(foreground, now);
        for &byte in bytes {
            if self.paste {
                output.push(byte);
                self.paste_tail.push(byte);
                if self.paste_tail.ends_with(PASTE_END) {
                    self.paste = false;
                    self.paste_tail.clear();
                } else if self.paste_tail.len() > PASTE_END.len() {
                    self.paste_tail.remove(0);
                }
                continue;
            }
            if self.input.bytes.is_empty() {
                if byte != 0x1b {
                    output.push(byte);
                    continue;
                }
                self.input.started = Some(now);
            }
            self.input.bytes.push(byte);
            if self.input.bytes == PASTE_START {
                self.release(&mut output);
                self.paste = true;
                continue;
            }
            if PASTE_START.starts_with(&self.input.bytes) {
                // With pending replies, retain the short ambiguous prefix so
                // a retired reply cannot leave a half-open OSC in a new app.
                // Standalone Esc is still released by PREFIX_TIMEOUT.
                if self.queries.is_empty() {
                    self.forward(&mut output);
                }
                continue;
            }
            if !self.input.bytes.starts_with(b"\x1b]") {
                // Preserve a second Esc as the start of a fresh frame.
                let fresh_escape = byte == 0x1b && self.input.bytes.len() > 1;
                if fresh_escape {
                    self.input.bytes.pop();
                }
                self.release(&mut output);
                if fresh_escape {
                    self.input.bytes.push(byte);
                    self.input.started = Some(now);
                }
                continue;
            }
            let complete =
                self.input.bytes.ends_with(b"\x07") || self.input.bytes.ends_with(b"\x1b\\");
            let body_end = self.input.bytes.len()
                - if complete {
                    if byte == b'\\' { 2 } else { 1 }
                } else {
                    0
                };
            let body = &self.input.bytes[2..body_end];
            if self.input.owner.is_none()
                && let Some(key) = first_response_key(body)
                // Replies contain no request ID. When a new owner asks for
                // the same color, let its first valid reply satisfy that live
                // request; a retired request must not starve the new program.
                && let Some(query) = self
                    .queries
                    .iter()
                    .find(|query| query.key == key && !query.retired)
                    .or_else(|| self.queries.iter().find(|query| query.key == key))
            {
                self.input.owner = Some(query.owner);
                self.input.retired = query.retired;
            }
            let stale = self.stale(foreground);
            if complete {
                let keys = color_keys(body, false);
                let valid = !keys.is_empty() && self.input.owner.is_some();
                if valid {
                    for key in keys {
                        if let Some(index) = self.queries.iter().position(|query| {
                            query.key == key
                                && Some(query.owner) == self.input.owner
                                && query.retired == self.input.retired
                        }) {
                            self.queries.remove(index);
                        }
                    }
                }
                if valid && stale {
                    self.input = InputFrame::default();
                } else {
                    self.release(&mut output);
                }
            } else if self.input.bytes.len() >= MAX_FRAME
                || byte == 0x18
                || byte == 0x1a
                || !potential_color_body(body)
            {
                self.release(&mut output);
            } else if !stale && (self.input.owner.is_some() || self.queries.is_empty()) {
                self.forward(&mut output);
            }
        }
        output
    }

    pub(crate) fn expire(&mut self, foreground: bool, now: Instant) -> Vec<u8> {
        self.queries.retain(|query| now < query.deadline);
        let mut output = Vec::new();
        if let Some(started) = self.input.started {
            // An OSC header can be fragmented before its owner key (the
            // semicolon) arrives. Keep that framing while queries are pending;
            // only the ambiguous bare Escape uses the keyboard timeout.
            let color_header = self.input.bytes.starts_with(b"\x1b]")
                && potential_color_body(&self.input.bytes[2..])
                && !self.queries.is_empty();
            let timeout = if self.input.owner.is_some() || color_header {
                LATE_GRACE
            } else {
                PREFIX_TIMEOUT
            };
            if now.saturating_duration_since(started) >= timeout {
                if self.stale(foreground) {
                    self.input = InputFrame::default();
                } else {
                    self.release(&mut output);
                }
            }
        }
        output
    }

    fn stale(&self, foreground: bool) -> bool {
        self.input.retired
            || self
                .input
                .owner
                .is_some_and(|owner| owner.is_some() && (!foreground || owner != self.foreground))
    }

    fn forward(&mut self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.input.bytes[self.input.forwarded..]);
        self.input.forwarded = self.input.bytes.len();
    }

    fn release(&mut self, output: &mut Vec<u8>) {
        self.forward(output);
        self.input = InputFrame::default();
    }
}

fn first_response_key(body: &[u8]) -> Option<ColorKey> {
    let text = std::str::from_utf8(body).ok()?;
    let (code, rest) = text.split_once(';')?;
    match code.parse::<u8>().ok()? {
        4 => Some(ColorKey::Palette(rest.split_once(';')?.0.parse().ok()?)),
        code @ 10..=19 => Some(ColorKey::Dynamic(code)),
        21 => Some(ColorKey::Named(rest.split_once('=')?.0.to_owned())),
        _ => None,
    }
}

fn potential_color_body(body: &[u8]) -> bool {
    let Some(end) = body.iter().position(|byte| *byte == b';') else {
        return body.len() <= 2 && body.iter().all(u8::is_ascii_digit);
    };
    matches!(
        &body[..end],
        b"4" | b"10"
            | b"11"
            | b"12"
            | b"13"
            | b"14"
            | b"15"
            | b"16"
            | b"17"
            | b"18"
            | b"19"
            | b"21"
    ) && body[end + 1..]
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == 0x1b)
}

fn color_keys(body: &[u8], query: bool) -> Vec<ColorKey> {
    let Ok(text) = std::str::from_utf8(body) else {
        return Vec::new();
    };
    let mut parts = text.split(';');
    let Some(code) = parts.next().and_then(|code| code.parse::<u8>().ok()) else {
        return Vec::new();
    };
    let valid = |value: &str| {
        if query {
            value == "?"
        } else {
            valid_color(value)
        }
    };
    let mut keys = Vec::new();
    match code {
        4 => {
            while let (Some(index), Some(value)) = (parts.next(), parts.next()) {
                if valid(value)
                    && let Ok(index @ 0..=255) = index.parse()
                {
                    keys.push(ColorKey::Palette(index));
                }
            }
        }
        10..=19 => {
            for (offset, value) in parts.enumerate().take(usize::from(20 - code)) {
                if valid(value) {
                    keys.push(ColorKey::Dynamic(code + offset as u8));
                }
            }
        }
        21 => {
            for part in parts {
                if let Some((key, value)) = part.split_once('=')
                    && key.len() <= 40
                    && !key.is_empty()
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    && (valid(value) || (!query && value.is_empty()))
                {
                    keys.push(ColorKey::Named(key.to_owned()));
                }
            }
        }
        _ => {}
    }
    keys
}

fn valid_color(value: &str) -> bool {
    if let Some(hex) = value.strip_prefix('#') {
        return matches!(hex.len(), 3 | 6 | 9 | 12)
            && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    if let Some(rgb) = value.strip_prefix("rgb:") {
        let parts: Vec<_> = rgb.split('/').collect();
        return parts.len() == 3
            && parts.iter().all(|part| {
                (1..=4).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
    }
    if let Some(rgb) = value.strip_prefix("rgbi:") {
        let parts: Vec<_> = rgb.split('/').collect();
        return parts.len() == 3
            && parts
                .iter()
                .all(|part| part.parse::<f32>().is_ok_and(|value| value.is_finite()));
    }
    false
}

/// Bounded output scanner. Other strings are opaque, so text inside a title,
/// hyperlink or DCS payload cannot register a color query.
#[derive(Debug, Default)]
struct OscScanner {
    state: u8,
    body: Vec<u8>,
}

impl OscScanner {
    fn push(&mut self, byte: u8) -> Option<Vec<u8>> {
        match (self.state, byte) {
            (_, 0x18 | 0x1a) => {
                self.state = 0;
                self.body.clear();
            }
            (0, 0x1b) => self.state = 1,
            (1, b']') => {
                self.state = 2;
                self.body.clear();
            }
            (1, b'P' | b'X' | b'^' | b'_') => self.state = 4,
            (1, 0x1b) => {}
            (1, _) => self.state = 0,
            (2, 0x07) | (3, b'\\') => {
                self.state = 0;
                return Some(std::mem::take(&mut self.body));
            }
            (2, 0x1b) => self.state = 3,
            (2, _) if self.body.len() < MAX_FRAME => self.body.push(byte),
            (2, _) | (3, _) => {
                self.state = 4;
                self.body.clear();
            }
            (4, 0x07) | (5, b'\\') => self.state = 0,
            (4, 0x1b) => self.state = 5,
            (5, _) => self.state = 4,
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(query: &[u8], now: Instant) -> ColorReplyFilter {
        let mut filter = ColorReplyFilter::default();
        filter.prompt(now);
        filter.begin_foreground();
        filter.observe_output(query, now);
        filter
    }

    #[test]
    fn late_colors_are_consumed_at_every_split_and_terminator() {
        for (query, body) in [
            ("4;15;?", "4;15;rgb:ffff/ffff/ffff"),
            ("10;?", "10;#ffffff"),
            ("11;?", "11;rgb:ff/ff/ff"),
            ("12;?", "12;rgbi:1/1/1"),
            ("21;foreground=?;cursor=?", "21;foreground=#ffffff;cursor="),
        ] {
            for end in ["\x07", "\x1b\\"] {
                let request = format!("\x1b]{query}{end}");
                let reply = format!("\x1b]{body}{end}");
                for split in 0..=reply.len() {
                    let now = Instant::now();
                    let mut filter = fixture(request.as_bytes(), now);
                    filter.prompt(now);
                    assert!(
                        filter
                            .route(&reply.as_bytes()[..split], false, now)
                            .is_empty()
                    );
                    assert!(
                        filter
                            .route(&reply.as_bytes()[split..], false, now)
                            .is_empty()
                    );
                    assert!(filter.queries.is_empty());
                    assert_eq!(filter.route(b"echo #ffffff", false, now), b"echo #ffffff");
                }
            }
        }
    }

    #[test]
    fn delayed_color_headers_stay_framed_before_the_owner_is_known() {
        let reply = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
        for split in 2..reply.len() {
            let now = Instant::now();
            let mut filter = fixture(b"\x1b]11;?\x07", now);
            filter.prompt(now);
            assert!(filter.route(&reply[..split], false, now).is_empty());
            let later = now + Duration::from_millis(350);
            assert!(filter.expire(false, later).is_empty(), "split {split}");
            assert!(
                filter.route(&reply[split..], false, later).is_empty(),
                "split {split}"
            );
            assert_eq!(
                filter.route(b"ordinary input", false, later),
                b"ordinary input"
            );
        }
    }

    #[test]
    fn live_fragments_pass_through_but_exit_quarantines_the_suffix() {
        let reply = b"\x1b]11;#ffffff\x1b\\";
        for split in 5..reply.len() {
            let now = Instant::now();
            let mut filter = fixture(b"\x1b]11;?\x07", now);
            let mut prefix = Vec::new();
            for byte in &reply[..split] {
                prefix.extend(filter.route(&[*byte], true, now));
            }
            assert_eq!(prefix, reply[..split]);
            filter.prompt(now);
            let later = now + Duration::from_millis(350);
            assert!(filter.expire(false, later).is_empty());
            assert!(filter.route(&reply[split..], false, later).is_empty());
            assert_eq!(filter.route(b"#ffffff", false, later), b"#ffffff");
        }
    }

    #[test]
    fn active_program_shell_queries_and_unknown_osc_remain_byte_exact() {
        let now = Instant::now();
        for foreground in [false, true] {
            let mut filter = ColorReplyFilter::default();
            if foreground {
                filter.begin_foreground();
            }
            filter.observe_output(b"\x1b]11;?\x07", now);
            let bytes = b"\x1b]11;#ffffff\x1b\\";
            let mut actual = Vec::new();
            for byte in bytes {
                actual.extend(filter.route(&[*byte], foreground, now));
            }
            assert_eq!(actual, bytes);
        }
        let mut filter = fixture(b"\x1b]11;?\x07", now);
        filter.prompt(now);
        for bytes in [
            b"#ffffff".as_slice(),
            b"\x1b]52;c;aGVsbG8=\x07",
            b"\x1b]12;#ffffff\x07",
            b"\x1b[A",
            b"\x1bx",
        ] {
            assert_eq!(filter.route(bytes, false, now), bytes);
        }
    }

    #[test]
    fn bracketed_paste_preserves_even_literal_reply_sequences() {
        let now = Instant::now();
        let mut filter = fixture(b"\x1b]11;?\x07", now);
        filter.prompt(now);
        let bytes = b"\x1b[200~#ffffff\x1b]11;#ffffff\x07\x1b[201~";
        let mut actual = Vec::new();
        for byte in bytes {
            actual.extend(filter.route(&[*byte], false, now));
        }
        assert_eq!(actual, bytes);
        assert!(filter.route(b"\x1b]11;#ffffff\x07", false, now).is_empty());
    }

    #[test]
    fn query_scanner_ignores_opaque_strings_and_handles_fragmentation() {
        let now = Instant::now();
        let mut filter = ColorReplyFilter::default();
        filter.observe_output(b"\x1bPtext \x1b]11;?\x1b\\\x1b]0;title 11;?\x07", now);
        assert!(filter.queries.is_empty());
        for byte in b"\x1b]4;0;?;1;?\x1b\\\x1b]10;?;?\x07" {
            filter.observe_output(&[*byte], now);
        }
        assert_eq!(filter.queries.len(), 4);
    }

    #[test]
    fn escape_is_prompt_and_expired_queries_do_not_consume_input() {
        let now = Instant::now();
        let mut filter = fixture(b"\x1b]11;?\x07", now);
        filter.prompt(now);
        assert!(filter.route(b"\x1b", false, now).is_empty());
        assert_eq!(filter.expire(false, now + PREFIX_TIMEOUT), b"\x1b");
        let later = now + LATE_GRACE;
        assert_eq!(
            filter.route(b"\x1b]11;#ffffff\x07", false, later),
            b"\x1b]11;#ffffff\x07"
        );
    }

    #[test]
    fn prompt_retires_shell_startup_queries_but_not_new_widget_queries() {
        let now = Instant::now();
        let mut filter = ColorReplyFilter::default();
        filter.observe_output(b"\x1b]11;?\x07", now);
        filter.prompt(now);
        assert!(filter.route(b"\x1b]11;#ffffff\x07", false, now).is_empty());
        filter.observe_output(b"\x1b]11;?\x07", now);
        assert_eq!(
            filter.route(b"\x1b]11;#ffffff\x07", false, now),
            b"\x1b]11;#ffffff\x07"
        );
    }

    #[test]
    fn live_query_takes_precedence_over_retired_query_for_the_same_color() {
        let now = Instant::now();
        let reply = b"\x1b]11;#ffffff\x07";
        for foreground in [false, true] {
            let mut filter = ColorReplyFilter::default();
            filter.observe_output(b"\x1b]11;?\x07", now);
            filter.prompt(now);
            if foreground {
                filter.begin_foreground();
            }
            filter.observe_output(b"\x1b]11;?\x07", now);
            assert_eq!(filter.route(reply, foreground, now), reply);
            assert_eq!(filter.queries.len(), 1);
            assert!(filter.queries[0].retired);
            assert!(filter.route(reply, foreground, now).is_empty());
        }
    }
}
