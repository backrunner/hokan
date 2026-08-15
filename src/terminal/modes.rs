//! Terminal input-mode bookkeeping shared by the screen model and cleanup.
//!
//! A shell is allowed to keep some modes enabled at its prompt (most notably
//! bracketed paste).  Full-screen programs, however, commonly change the
//! same terminal-global modes and can leave them behind when they crash.  The
//! model snapshots the shell state before a foreground child starts and uses
//! this value to restore only the changes made by that child.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TerminalInputModes {
    pub(crate) application_cursor: bool,
    pub(crate) application_keypad: bool,
    pub(crate) mouse_x10: bool,
    pub(crate) mouse_highlight: bool,
    pub(crate) mouse_button: bool,
    pub(crate) mouse_drag: bool,
    pub(crate) mouse_any: bool,
    pub(crate) focus_reporting: bool,
    pub(crate) mouse_utf8: bool,
    pub(crate) mouse_sgr: bool,
    pub(crate) alternate_scroll: bool,
    pub(crate) mouse_urxvt: bool,
    pub(crate) mouse_pixels: bool,
    pub(crate) eight_bit_input: bool,
    pub(crate) meta_sends_escape: bool,
    pub(crate) alt_sends_escape: bool,
    pub(crate) bracketed_paste: bool,
    pub(crate) mode_2027: bool,
    pub(crate) mode_2028: bool,
    pub(crate) mode_2031: bool,
    pub(crate) mode_8452: bool,
    /// xterm modifyOtherKeys level (0 means disabled).
    pub(crate) modify_other_keys: u16,
    /// Current kitty keyboard protocol flags and the number of pushes made on
    /// the terminal's per-screen keyboard-mode stack.
    pub(crate) kitty_flags: u32,
    pub(crate) kitty_stack_depth: u16,
}

impl TerminalInputModes {
    pub(crate) fn apply_dec_mode(&mut self, mode: u16, enabled: bool) {
        match mode {
            1 => self.application_cursor = enabled,
            9 => self.mouse_x10 = enabled,
            66 => self.application_keypad = enabled,
            1001 => self.mouse_highlight = enabled,
            1000 => self.mouse_button = enabled,
            1002 => self.mouse_drag = enabled,
            1003 => self.mouse_any = enabled,
            1004 => self.focus_reporting = enabled,
            1005 => self.mouse_utf8 = enabled,
            1006 => self.mouse_sgr = enabled,
            1007 => self.alternate_scroll = enabled,
            1015 => self.mouse_urxvt = enabled,
            1016 => self.mouse_pixels = enabled,
            1034 => self.eight_bit_input = enabled,
            1036 => self.meta_sends_escape = enabled,
            1039 => self.alt_sends_escape = enabled,
            2004 => self.bracketed_paste = enabled,
            2027 => self.mode_2027 = enabled,
            2028 => self.mode_2028 = enabled,
            2031 => self.mode_2031 = enabled,
            8452 => self.mode_8452 = enabled,
            _ => {}
        }
    }

    pub(crate) fn apply_modify_other_keys(&mut self, level: u16) {
        self.modify_other_keys = level;
    }

    pub(crate) fn push_kitty(&mut self, flags: u32) {
        self.kitty_stack_depth = self.kitty_stack_depth.saturating_add(1);
        self.kitty_flags = flags;
    }

    pub(crate) fn pop_kitty(&mut self, count: u16) {
        let count = count.max(1);
        self.kitty_stack_depth = self.kitty_stack_depth.saturating_sub(count);
        if self.kitty_stack_depth == 0 {
            self.kitty_flags = 0;
        }
    }

    pub(crate) fn set_kitty(&mut self, flags: u32) {
        self.kitty_flags = flags;
    }

    /// Return a conservative shell baseline when no foreground transition was
    /// observed (for example, a prompt marker arriving after a child crash).
    /// Shell editing modes are preserved; modes which are exclusively used for
    /// application input reporting are cleared.
    pub(crate) fn fallback_prompt_baseline(self) -> Self {
        Self {
            mouse_x10: false,
            mouse_highlight: false,
            mouse_button: false,
            mouse_drag: false,
            mouse_any: false,
            focus_reporting: false,
            mouse_utf8: false,
            mouse_sgr: false,
            alternate_scroll: false,
            mouse_urxvt: false,
            mouse_pixels: false,
            mode_2027: false,
            mode_2028: false,
            mode_2031: false,
            mode_8452: false,
            modify_other_keys: 0,
            kitty_flags: 0,
            kitty_stack_depth: 0,
            ..self
        }
    }

    /// Encode only the transitions needed to move the terminal from `self`
    /// to `target`.  This keeps a shell's own prompt modes intact while
    /// repairing a crashed foreground application.
    pub(crate) fn restore_bytes(self, target: Self) -> Vec<u8> {
        let mut bytes = Vec::new();
        append_dec(
            &mut bytes,
            1,
            self.application_cursor,
            target.application_cursor,
        );
        append_dec(
            &mut bytes,
            66,
            self.application_keypad,
            target.application_keypad,
        );
        append_dec(&mut bytes, 9, self.mouse_x10, target.mouse_x10);
        append_dec(
            &mut bytes,
            1001,
            self.mouse_highlight,
            target.mouse_highlight,
        );
        append_dec(&mut bytes, 1000, self.mouse_button, target.mouse_button);
        append_dec(&mut bytes, 1002, self.mouse_drag, target.mouse_drag);
        append_dec(&mut bytes, 1003, self.mouse_any, target.mouse_any);
        append_dec(
            &mut bytes,
            1004,
            self.focus_reporting,
            target.focus_reporting,
        );
        append_dec(&mut bytes, 1005, self.mouse_utf8, target.mouse_utf8);
        append_dec(&mut bytes, 1006, self.mouse_sgr, target.mouse_sgr);
        append_dec(
            &mut bytes,
            1007,
            self.alternate_scroll,
            target.alternate_scroll,
        );
        append_dec(&mut bytes, 1015, self.mouse_urxvt, target.mouse_urxvt);
        append_dec(&mut bytes, 1016, self.mouse_pixels, target.mouse_pixels);
        append_dec(
            &mut bytes,
            1034,
            self.eight_bit_input,
            target.eight_bit_input,
        );
        append_dec(
            &mut bytes,
            1036,
            self.meta_sends_escape,
            target.meta_sends_escape,
        );
        append_dec(
            &mut bytes,
            1039,
            self.alt_sends_escape,
            target.alt_sends_escape,
        );
        append_dec(
            &mut bytes,
            2004,
            self.bracketed_paste,
            target.bracketed_paste,
        );
        append_dec(&mut bytes, 2027, self.mode_2027, target.mode_2027);
        append_dec(&mut bytes, 2028, self.mode_2028, target.mode_2028);
        append_dec(&mut bytes, 2031, self.mode_2031, target.mode_2031);
        append_dec(&mut bytes, 8452, self.mode_8452, target.mode_8452);

        if self.modify_other_keys != target.modify_other_keys {
            bytes.extend_from_slice(b"\x1b[>4;");
            append_decimal(&mut bytes, target.modify_other_keys as u32);
            bytes.extend_from_slice(b"m");
        }

        if self.kitty_stack_depth > target.kitty_stack_depth {
            let count = self.kitty_stack_depth - target.kitty_stack_depth;
            bytes.extend_from_slice(b"\x1b[<");
            if count != 1 {
                append_decimal(&mut bytes, count as u32);
            }
            bytes.push(b'u');
        } else if self.kitty_stack_depth < target.kitty_stack_depth {
            let count = target.kitty_stack_depth - self.kitty_stack_depth;
            for _ in 0..count {
                bytes.extend_from_slice(b"\x1b[>");
                append_decimal(&mut bytes, target.kitty_flags);
                bytes.push(b'u');
            }
        }
        if self.kitty_flags != target.kitty_flags {
            bytes.extend_from_slice(b"\x1b[=");
            append_decimal(&mut bytes, target.kitty_flags);
            bytes.push(b'u');
        }

        bytes
    }
}

fn append_dec(bytes: &mut Vec<u8>, mode: u16, current: bool, target: bool) {
    if current == target {
        return;
    }
    bytes.extend_from_slice(b"\x1b[?");
    append_decimal(bytes, mode as u32);
    bytes.push(if target { b'h' } else { b'l' });
}

fn append_decimal(bytes: &mut Vec<u8>, value: u32) {
    use std::fmt::Write as _;
    write!(bytes_as_string(bytes), "{value}").expect("writing to a byte vector cannot fail");
}

// `Vec<u8>` implements `Write` only through an adapter in the standard
// library; keeping the adapter local avoids allocating a temporary String for
// every mode transition.
fn bytes_as_string(bytes: &mut Vec<u8>) -> ByteWriter<'_> {
    ByteWriter(bytes)
}

struct ByteWriter<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for ByteWriter<'_> {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        self.0.extend_from_slice(value.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_kimi_input_modes_and_keyboard_stack() {
        let mut current = TerminalInputModes::default();
        current.apply_dec_mode(1004, true);
        current.apply_dec_mode(2031, true);
        current.push_kitty(7);
        let bytes = current.restore_bytes(TerminalInputModes::default());
        assert!(
            bytes
                .windows(b"\x1b[?1004l".len())
                .any(|w| w == b"\x1b[?1004l")
        );
        assert!(
            bytes
                .windows(b"\x1b[?2031l".len())
                .any(|w| w == b"\x1b[?2031l")
        );
        assert!(bytes.windows(b"\x1b[<u".len()).any(|w| w == b"\x1b[<u"));
    }

    #[test]
    fn restores_shell_bracketed_paste_baseline() {
        let mut current = TerminalInputModes::default();
        current.apply_dec_mode(2004, false);
        let mut shell = TerminalInputModes::default();
        shell.apply_dec_mode(2004, true);
        assert_eq!(current.restore_bytes(shell), b"\x1b[?2004h");
    }
}
