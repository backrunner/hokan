use std::{
    ffi::OsString,
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};

use crate::{
    platform::run_bounded,
    terminal::{BufferRevision, CellPos, ScreenEpoch, ScreenRevision, TerminalSize},
};

const TMUX_CURSOR_TIMEOUT: Duration = Duration::from_millis(250);
const TMUX_CURSOR_MAX_OUTPUT: usize = 64;
const TMUX_CURSOR_FORMAT: &str = "#{cursor_y};#{cursor_x}";
pub(super) const TMUX_CURSOR_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CursorProbeBackend {
    TerminalPrivate,
    TerminalStandardGuarded,
    Tmux,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PendingTmuxCursor {
    pub(super) generation: u64,
    pub(super) buffer_revision: BufferRevision,
    pub(super) screen_revision: ScreenRevision,
    pub(super) screen_epoch: ScreenEpoch,
    pub(super) terminal_size: TerminalSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TmuxCursorRequest {
    generation: u64,
    terminal_size: TerminalSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TmuxCursorResult {
    pub(super) generation: u64,
    pub(super) position: Option<CellPos>,
}

pub(super) struct TmuxCursorProbe {
    sender: Option<Sender<TmuxCursorRequest>>,
    results: Receiver<TmuxCursorResult>,
    join: Option<JoinHandle<()>>,
}

impl TmuxCursorProbe {
    pub(super) fn start_from_env() -> Option<Self> {
        let pane = tmux_pane_from_env()?;
        let (sender, receiver) = bounded::<TmuxCursorRequest>(1);
        let (result_sender, results) = unbounded();
        let join = thread::Builder::new()
            .name("hokan-tmux-cursor".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let position = query_tmux_cursor(&pane, request.terminal_size);
                    if result_sender
                        .send(TmuxCursorResult {
                            generation: request.generation,
                            position,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            sender: Some(sender),
            results,
            join: Some(join),
        })
    }

    pub(super) fn schedule(&self, generation: u64, terminal_size: TerminalSize) -> bool {
        let Some(sender) = self.sender.as_ref() else {
            return false;
        };
        match sender.try_send(TmuxCursorRequest {
            generation,
            terminal_size,
        }) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }

    pub(super) const fn results(&self) -> &Receiver<TmuxCursorResult> {
        &self.results
    }
}

impl Drop for TmuxCursorProbe {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn tmux_pane_from_env() -> Option<OsString> {
    let tmux = std::env::var_os("TMUX")?;
    if tmux.is_empty() {
        return None;
    }
    let pane = std::env::var_os("TMUX_PANE")?;
    let text = pane.to_str()?;
    let digits = text.strip_prefix('%')?;
    if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(pane)
}

fn query_tmux_cursor(pane: &OsString, terminal_size: TerminalSize) -> Option<CellPos> {
    let args = [
        OsString::from("display-message"),
        OsString::from("-p"),
        OsString::from("-t"),
        pane.clone(),
        OsString::from(TMUX_CURSOR_FORMAT),
    ];
    let output = run_bounded("tmux", args, TMUX_CURSOR_TIMEOUT, TMUX_CURSOR_MAX_OUTPUT).ok()?;
    output
        .status
        .success()
        .then(|| parse_tmux_cursor(&output.stdout, terminal_size))
        .flatten()
}

fn parse_tmux_cursor(bytes: &[u8], terminal_size: TerminalSize) -> Option<CellPos> {
    let text = std::str::from_utf8(bytes)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    let (row, col) = text.split_once(';')?;
    if col.contains(';')
        || row.is_empty()
        || col.is_empty()
        || !row.bytes().all(|byte| byte.is_ascii_digit())
        || !col.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let row = row.parse::<u16>().ok()?;
    let col = col.parse::<u16>().ok()?;
    (row < terminal_size.rows && col < terminal_size.cols).then_some(CellPos::new(row, col))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> TerminalSize {
        TerminalSize::new(24, 80).expect("terminal size")
    }

    #[test]
    fn parses_zero_based_tmux_cursor_coordinates() {
        assert_eq!(
            parse_tmux_cursor(b"12;34\n", size()),
            Some(CellPos::new(12, 34))
        );
        assert_eq!(
            parse_tmux_cursor(b"0;0\r\n", size()),
            Some(CellPos::new(0, 0))
        );
    }

    #[test]
    fn rejects_malformed_or_out_of_bounds_tmux_coordinates() {
        for bytes in [
            b"".as_slice(),
            b"1".as_slice(),
            b"1;2;3".as_slice(),
            b"-1;2".as_slice(),
            b"1;-2".as_slice(),
            b"+1;2".as_slice(),
            b"24;0".as_slice(),
            b"0;80".as_slice(),
            b"65536;1".as_slice(),
            b"1;2 payload".as_slice(),
        ] {
            assert_eq!(parse_tmux_cursor(bytes, size()), None, "bytes={bytes:?}");
        }
    }
}
