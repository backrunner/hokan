use std::io::Write;

use super::{OutputActor, OutputError};

/// A fallback title belongs to one shell command. Child OSC titles win for
/// the rest of that command, including when a slow process-name lookup returns
/// after the application has already published its own title.
#[derive(Default)]
pub(super) struct TitleSync {
    pub(super) shell: Option<String>,
    pub(super) generation: u64,
    child_owned: bool,
    first_prompt_seen: bool,
    shell_owns_prompt: bool,
    pending: Option<String>,
    published: Option<String>,
}

impl TitleSync {
    pub(super) fn probe_generation(&self) -> Option<u64> {
        (self.shell.is_some() && !self.child_owned).then_some(self.generation)
    }

    pub(super) fn configure(&mut self, shell: Option<String>) {
        if self.shell == shell {
            return;
        }
        self.shell = shell;
        self.next_command();
        self.pending = self.shell.clone();
    }

    pub(super) fn next_command(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.child_owned = false;
        self.pending = None;
    }

    pub(super) fn child_title(&mut self, foreground: bool) {
        // A shell/theme that publishes a title during startup already owns
        // prompt titles (including fish's native fish_title). Leave those
        // alone when subsequent prompts race their FIFO notifications.
        if !self.first_prompt_seen && !foreground {
            self.shell_owns_prompt = true;
        }
        self.child_owned = true;
        self.pending = None;
        self.published = None;
    }

    pub(super) fn foreground_title(&mut self, generation: u64, title: String) {
        if self.shell.is_some() && generation == self.generation && !self.child_owned {
            self.pending = Some(title);
        }
    }

    pub(super) fn prompt(&mut self) {
        self.first_prompt_seen = true;
        self.next_command();
        if !self.shell_owns_prompt {
            self.pending = self.shell.clone();
        }
    }

    fn take_sequence(&mut self) -> Option<String> {
        let title = self.pending.take()?;
        // Never put process arguments, paths, control bytes or an unbounded
        // process name into an OSC payload. In particular BEL/ESC/C1 must not
        // terminate the title and turn the suffix into terminal instructions.
        let title: String = title
            .chars()
            .filter(|c| !c.is_control())
            .take(128)
            .collect();
        if title.is_empty() || self.published.as_ref() == Some(&title) {
            return None;
        }
        self.published = Some(title.clone());
        Some(format!("\x1b]0;{title}\x07"))
    }
}

impl<W: Write> OutputActor<W> {
    pub(super) fn flush_title(&mut self) -> Result<(), OutputError> {
        // The decoder may be withholding an ESC that has not reached the
        // scanner yet. Neither a partial marker nor an arbitrary child
        // UTF-8/control sequence may be interrupted by our title.
        if self.scanner.is_safe()
            && !self.decoder.has_pending_bytes()
            && let Some(sequence) = self.title.take_sequence()
        {
            self.guard.write_control(sequence.as_bytes())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::output::ControlCommand;
    use crate::terminal::{ChildOutputBatch, DrainState, SessionToken, TerminalSize};

    fn actor() -> OutputActor<Vec<u8>> {
        OutputActor::new(
            Vec::new(),
            SessionToken::parse("0123456789abcdef0123456789abcdef").expect("token"),
            TerminalSize::new(24, 80).expect("size"),
            8,
        )
    }

    fn child(actor: &mut OutputActor<Vec<u8>>, bytes: &[u8]) {
        actor
            .handle_child(ChildOutputBatch {
                bytes: bytes.to_vec(),
                read_cycle: 1,
                drain: DrainState::DrainedToEagain,
            })
            .expect("child output");
        actor.flush_title().expect("flush title");
    }

    #[test]
    fn title_waits_for_split_utf8_csi_osc_and_decoder_prefixes() {
        for (head, tail) in [
            (b"\xe4".as_slice(), b"\xb8\xad".as_slice()),
            (b"\x1b[31", b"m"),
            (b"\x1b]7;file:///tmp", b"\x07"),
            (b"\x1b", b"[0m"),
        ] {
            let mut actor = actor();
            child(&mut actor, head);
            actor.title.configure(Some("zsh".into()));
            actor.flush_title().expect("defer title");
            assert!(actor.title.pending.is_some());
            child(&mut actor, tail);
            assert!(actor.title.pending.is_none());
            let bytes = actor.guard.finish().expect("finish");
            let prefix = [head, tail, b"\x1b]0;zsh\x07"].concat();
            assert!(bytes.starts_with(&prefix), "{bytes:?}");
        }
    }

    #[test]
    fn child_title_wins_over_late_probe_and_pending_fallback() {
        let mut actor = actor();
        actor.title.configure(Some("zsh".into()));
        actor.title.prompt();
        actor
            .handle_control(ControlCommand::SetForeground(true))
            .expect("foreground");
        let generation = actor.title.generation;
        child(&mut actor, b"\x1b]2;application");
        actor.title.foreground_title(generation, "sleep".into());
        actor.flush_title().expect("defer");
        child(&mut actor, b"\x1b\\");
        actor.title.foreground_title(generation, "sleep".into());
        actor.flush_title().expect("child owns title");
        let bytes = actor.guard.finish().expect("finish");
        assert!(bytes.starts_with(b"\x1b]2;application\x1b\\"));
        assert!(!bytes.windows(7).any(|b| b == b";sleep\x07"));
    }

    #[test]
    fn stale_probe_cannot_replace_prompt_or_next_command() {
        let mut title = TitleSync::default();
        title.configure(Some("bash".into()));
        title.prompt();
        title.next_command();
        let generation = title.generation;
        title.foreground_title(generation, "sleep".into());
        assert_eq!(title.take_sequence().as_deref(), Some("\x1b]0;sleep\x07"));
        title.prompt();
        title.foreground_title(generation, "sleep".into());
        assert_eq!(title.take_sequence().as_deref(), Some("\x1b]0;bash\x07"));
        title.next_command();
        title.foreground_title(generation, "sleep".into());
        assert!(title.take_sequence().is_none());
    }

    #[test]
    fn native_shell_title_survives_prompt_recovery() {
        let mut actor = actor();
        actor.title.configure(Some("fish".into()));
        child(&mut actor, b"\x1b]0;fish ~/project\x07");
        actor.recover_prompt_terminal_state().expect("prompt");
        actor.flush_title().expect("native title");
        let bytes = actor.guard.finish().expect("finish");
        assert!(!bytes.windows(9).any(|b| b == b"\x1b]0;fish\x07"));
    }

    #[test]
    fn title_is_bounded_sanitized_deduplicated_and_can_be_disabled() {
        let mut title = TitleSync::default();
        title.configure(Some("zsh".into()));
        title.next_command();
        title.foreground_title(
            title.generation,
            format!("\x1b\x07\n\u{009c}{}", "中".repeat(200)),
        );
        let expected = format!("\x1b]0;{}\x07", "中".repeat(128));
        assert_eq!(title.take_sequence(), Some(expected));
        title.foreground_title(title.generation, "中".repeat(200));
        assert!(title.take_sequence().is_none());
        title.configure(None);
        title.foreground_title(title.generation, "sleep".into());
        title.prompt();
        assert!(title.take_sequence().is_none());
    }
}
