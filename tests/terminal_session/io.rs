use super::*;

impl TerminalSession {
    pub(super) fn write(&mut self, bytes: &[u8]) {
        let writer = self.writer.as_mut().expect("PTY writer");
        if let Err(error) = writer.write_all(bytes).and_then(|()| writer.flush()) {
            let status = self.try_wait();
            // Drain whatever the dying child flushed so its error report
            // (`hokan: …`) reaches the transcript before we panic.
            self.settle(Duration::from_millis(500));
            panic!(
                "write terminal input failed: {error}; child status={status:?}; transcript tail={:?}",
                tail(&self.transcript, 4_096)
            );
        }
    }

    pub(super) fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .as_ref()
            .expect("PTY master")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize outer PTY");
        self.rows = rows;
        self.cols = cols;
        self.terminal.screen_mut().set_size(rows, cols);
    }

    pub(super) fn process_chunk(&mut self, chunk: &[u8]) {
        self.transcript.extend_from_slice(chunk);
        if self.try_wait().is_some() {
            // Drain the final output without sending delayed query responses
            // into a PTY whose child has exited and restored canonical echo.
            self.terminal.process(chunk);
            return;
        }
        for &byte in chunk {
            self.terminal.process(std::slice::from_ref(&byte));
            self.probe_tail.push(byte);
            let max_query_len = SYNC_QUERY
                .len()
                .max(HOKAN_CPR_QUERY.len())
                .max(STATUS_QUERY.len())
                .max(STANDARD_CPR_QUERY.len())
                .max(KITTY_KEYBOARD_QUERY.len())
                .max(DEVICE_ATTRIBUTES_QUERY.len());
            if self.probe_tail.len() > max_query_len {
                let trim = self.probe_tail.len() - max_query_len;
                self.probe_tail.drain(..trim);
            }
            if self.probe_tail.ends_with(SYNC_QUERY) {
                let reply = format!("\x1b[?2026;{}$y", self.sync_status);
                self.write(reply.as_bytes());
                self.sync_replies += 1;
                self.probe_tail.clear();
            } else if self.probe_tail.ends_with(KITTY_KEYBOARD_QUERY) {
                self.write(b"\x1b[?7u");
                self.probe_tail.clear();
            } else if self.probe_tail.ends_with(DEVICE_ATTRIBUTES_QUERY) {
                self.write(b"\x1b[?1;2c");
                self.probe_tail.clear();
            } else if self.probe_tail.ends_with(HOKAN_CPR_QUERY) && self.private_cpr_supported {
                let (row, col) = self.terminal.screen().cursor_position();
                let reply = format!("\x1b[?{};{}R", row + 1, col + 1);
                self.write(reply.as_bytes());
                self.cpr_replies += 1;
                self.probe_tail.clear();
            } else if self.probe_tail.ends_with(STATUS_QUERY) {
                self.write(b"\x1b[0n");
                self.probe_tail.clear();
            } else if self.probe_tail.ends_with(STANDARD_CPR_QUERY) {
                let (row, col) = self.terminal.screen().cursor_position();
                let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                self.write(reply.as_bytes());
                self.cpr_replies += 1;
                self.probe_tail.clear();
            }
        }
    }

    pub(super) fn receive_once(&mut self, timeout: Duration) {
        if let Ok(chunk) = self.chunks.recv_timeout(timeout) {
            self.process_chunk(&chunk);
        }
    }

    pub(super) fn wait_for_screen(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.screen_text().contains(needle) {
                return;
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "screen did not contain {needle:?}; screen=\n{}\ntranscript tail={:?}",
            self.screen_text(),
            tail(&self.transcript, 2_048)
        );
    }

    pub(super) fn wait_for_bare_row(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.screen_text().lines().any(|line| line.trim() == needle) {
                return;
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "screen had no bare row {needle:?}; screen=\n{}\ntranscript tail={:?}",
            self.screen_text(),
            tail(&self.transcript, 2_048)
        );
    }

    pub(super) fn wait_for_selected_history(&mut self, command: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self
                .screen_text()
                .lines()
                .any(|row| row.contains('▶') && row.contains(command))
            {
                return;
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "history selection did not reach {command:?}:\n{}",
            self.screen_text()
        );
    }

    pub(super) fn wait_for_screen_absent(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if !self.screen_text().contains(needle) {
                return;
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "screen still contained {needle:?}; screen=\n{}",
            self.screen_text()
        );
    }

    pub(super) fn wait_for_overlay_candidate(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self
                .screen_text()
                .lines()
                .any(|line| line.trim_start().starts_with('│') && line.contains(needle))
                && self.border_strays().is_empty()
            {
                return;
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "overlay did not contain {needle:?}; screen=\n{}",
            self.screen_text()
        );
    }

    pub(super) fn wait_for_bytes_since(&mut self, start: usize, needle: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.transcript[start..]
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return;
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "PTY output did not contain {:?}; transcript tail={:?}",
            needle,
            tail(&self.transcript, 2_048)
        );
    }

    pub(super) fn settle(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            self.receive_once(READ_POLL);
        }
    }

    pub(super) fn wait_for_sync_replies(&mut self, count: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline && self.sync_replies < count {
            self.receive_once(READ_POLL);
        }
        assert!(
            self.sync_replies >= count,
            "DECRQM probe was not answered; transcript tail={:?}",
            tail(&self.transcript, 2_048)
        );
    }

    pub(super) fn wait_for_cpr_replies(&mut self, count: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline && self.cpr_replies < count {
            self.receive_once(READ_POLL);
        }
        assert!(
            self.cpr_replies >= count,
            "CPR probe was not answered; transcript tail={:?}",
            tail(&self.transcript, 2_048)
        );
    }

    /// Waits until the edit line is present AND every border glyph on screen
    /// belongs to exactly one rectangular overlay box. Retries ride out
    /// mid-paint transients; a persistent smear never satisfies this.
    pub(super) fn wait_for_clean_overlay(&mut self, edit_line: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.screen_text().contains(edit_line) {
                let strays = self.border_strays();
                if strays.is_empty() {
                    return;
                }
            }
            self.receive_once(READ_POLL);
        }
        panic!(
            "overlay did not settle cleanly for {edit_line:?}; strays={:?}; screen=\n{}\ntranscript tail={:?}",
            self.border_strays(),
            self.screen_text(),
            tail(&self.transcript, 2_048)
        );
    }

    /// Describes every border glyph that cannot belong to a single current
    /// overlay box. An empty result means the screen holds exactly one box:
    /// one top edge, one bottom edge below it, matching corners, and side
    /// pipes only on the box's left/right columns between the edges.
    pub(super) fn border_strays(&self) -> Vec<String> {
        let screen = self.terminal.screen();
        let mut tops = Vec::new();
        let mut top_rights = Vec::new();
        let mut bottoms = Vec::new();
        let mut bottom_rights = Vec::new();
        let mut sides = Vec::new();
        for row in 0..self.rows {
            for col in 0..self.cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                match cell.contents() {
                    "╭" => tops.push((row, col)),
                    "╮" => top_rights.push((row, col)),
                    "╰" => bottoms.push((row, col)),
                    "╯" => bottom_rights.push((row, col)),
                    "│" => sides.push((row, col)),
                    _ => {}
                }
            }
        }
        let mut strays = Vec::new();
        if tops.len() != 1
            || top_rights.len() != 1
            || bottoms.len() != 1
            || bottom_rights.len() != 1
        {
            strays.push(format!(
                "corner count ╭={} ╮={} ╰={} ╯={} (tops={tops:?} top_rights={top_rights:?} bottoms={bottoms:?} bottom_rights={bottom_rights:?} sides={sides:?})",
                tops.len(),
                top_rights.len(),
                bottoms.len(),
                bottom_rights.len()
            ));
            return strays;
        }
        let (top, left) = tops[0];
        let (bottom, _) = bottoms[0];
        let right = top_rights[0].1;
        if top_rights[0].0 != top {
            strays.push(format!(
                "top-right corner at {:?}, expected row {top}",
                top_rights[0]
            ));
        }
        if bottoms[0].1 != left {
            strays.push(format!(
                "bottom-left corner at {:?}, expected ({bottom}, {left})",
                bottoms[0]
            ));
        }
        if bottom_rights[0] != (bottom, right) {
            strays.push(format!(
                "bottom-right corner at {:?}, expected ({bottom}, {right})",
                bottom_rights[0]
            ));
        }
        if bottom <= top + 1 {
            strays.push(format!("box edges too close: top={top} bottom={bottom}"));
        }
        for (row, col) in sides {
            if row <= top || row >= bottom || (col != left && col != right) {
                strays.push(format!("stray side border at ({row}, {col})"));
            }
        }
        strays
    }

    /// Every visible cell carrying a non-default background color. Hokan's
    /// colored overlay is the only painter in these fixtures, so painted
    /// cells after dismissal are residue.
    pub(super) fn painted_screen_cells(&self) -> Vec<String> {
        let screen = self.terminal.screen();
        let mut painted = Vec::new();
        for row in 0..self.rows {
            for col in 0..self.cols {
                if let Some(cell) = screen.cell(row, col)
                    && cell.bgcolor() != vt100::Color::Default
                {
                    painted.push(format!(
                        "({row}, {col}) bg={:?} {:?}",
                        cell.bgcolor(),
                        cell.contents()
                    ));
                }
            }
        }
        painted
    }

    /// Painted cells in the scrollback rows above the current screen. Once a
    /// row is pushed there no terminal sequence can reach it, so overlay
    /// cells found here are permanently visible when scrolling up.
    pub(super) fn painted_scrollback_cells(&mut self) -> Vec<String> {
        let screen = self.terminal.screen_mut();
        // Clamping the view offset to the maximum reports the scrollback
        // length; restoring offset 0 returns the view to the live screen.
        screen.set_scrollback(usize::MAX);
        let count = screen.scrollback();
        let mut painted = Vec::new();
        for offset in 1..=count {
            screen.set_scrollback(offset);
            for col in 0..self.cols {
                if let Some(cell) = screen.cell(0, col)
                    && cell.bgcolor() != vt100::Color::Default
                {
                    painted.push(format!(
                        "scrollback[{offset}] (0, {col}) bg={:?} {:?}",
                        cell.bgcolor(),
                        cell.contents()
                    ));
                }
            }
        }
        screen.set_scrollback(0);
        painted
    }

    pub(super) fn screen_text(&self) -> String {
        let mut text = String::new();
        for row in 0..self.rows {
            for col in 0..self.cols {
                if let Some(cell) = self.terminal.screen().cell(row, col) {
                    if cell.has_contents() {
                        text.push_str(cell.contents());
                    } else if !cell.is_wide_continuation() {
                        // Erased cells occupy columns too. Omitting them
                        // joins words after a perfectly valid ZLE redraw.
                        text.push(' ');
                    }
                } else {
                    text.push(' ');
                }
            }
            text.push('\n');
        }
        text
    }

    pub(super) fn screen_rows_containing(&self, needle: &str) -> Vec<u16> {
        self.screen_text()
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(needle))
            .map(|(index, _)| index as u16)
            .collect()
    }

    pub(super) fn screen_line(&self, row: u16) -> String {
        self.screen_text()
            .lines()
            .nth(row as usize)
            .expect("row index within screen")
            .to_owned()
    }

    pub(super) fn pid(&self) -> i32 {
        i32::try_from(self.child.process_id().expect("hokan pid")).expect("pid fits")
    }

    pub(super) fn try_wait(&mut self) -> Option<portable_pty::ExitStatus> {
        self.child.try_wait().expect("child status")
    }

    pub(super) fn wait_until_exit(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline && self.try_wait().is_none() {
            self.receive_once(READ_POLL);
        }
        assert!(
            self.try_wait().is_some(),
            "PTY child did not exit; transcript tail={:?}",
            tail(&self.transcript, 2_048)
        );
        self.settle(Duration::from_millis(50));
    }

    pub(super) fn exit_shell(&mut self) {
        self.write(b"\x15exit\r");
    }
}
