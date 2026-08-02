use unicode_segmentation::UnicodeSegmentation;

use crate::{
    completion::SyncQuality,
    terminal::{BufferRevision, InputKind},
};

#[derive(Clone, Debug)]
pub struct EditableBuffer {
    pub text: String,
    pub cursor: usize,
    pub revision: BufferRevision,
    pub sync: SyncQuality,
}

impl EditableBuffer {
    #[must_use]
    pub fn new(sync: SyncQuality) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            revision: BufferRevision::ZERO,
            sync,
        }
    }

    pub fn set_exact(&mut self, text: String, cursor: usize) -> crate::Result<bool> {
        if cursor > text.len() || !text.is_char_boundary(cursor) {
            return Err(crate::Error::Parse("invalid exact shell cursor".into()));
        }
        let changed = self.text != text || self.cursor != cursor || self.sync != SyncQuality::Exact;
        self.text = text;
        self.cursor = cursor;
        self.sync = SyncQuality::Exact;
        if changed {
            self.advance_revision()?;
        }
        Ok(changed)
    }

    pub fn reset_prompt(&mut self, sync: SyncQuality) -> crate::Result<()> {
        self.text.clear();
        self.cursor = 0;
        self.sync = sync;
        self.advance_revision()
    }

    pub fn replace_mirrored(&mut self, text: String, cursor: usize) -> crate::Result<()> {
        if cursor > text.len() || !text.is_char_boundary(cursor) {
            return Err(crate::Error::Parse("invalid mirrored cursor".into()));
        }
        self.text = text;
        self.cursor = cursor;
        self.sync = SyncQuality::Mirrored;
        self.advance_revision()
    }

    pub fn apply_mirrored(&mut self, kind: &InputKind) -> crate::Result<MirrorOutcome> {
        if self.sync == SyncQuality::Uncertain {
            return Ok(MirrorOutcome::Uncertain);
        }
        let mut changed = false;
        match kind {
            InputKind::Text(text) => {
                self.text.insert_str(self.cursor, text);
                self.cursor += text.len();
                changed = true;
            }
            InputKind::Paste(bytes) => {
                let Ok(text) = std::str::from_utf8(bytes) else {
                    self.sync = SyncQuality::Uncertain;
                    return Ok(MirrorOutcome::Uncertain);
                };
                self.text.insert_str(self.cursor, text);
                self.cursor += text.len();
                changed = true;
            }
            InputKind::Backspace => {
                if let Some(previous) = previous_boundary(&self.text, self.cursor) {
                    self.text.replace_range(previous..self.cursor, "");
                    self.cursor = previous;
                    changed = true;
                }
            }
            InputKind::Delete => {
                if let Some(next) = next_boundary(&self.text, self.cursor) {
                    self.text.replace_range(self.cursor..next, "");
                    changed = true;
                }
            }
            InputKind::Left => {
                if let Some(previous) = previous_boundary(&self.text, self.cursor) {
                    self.cursor = previous;
                    changed = true;
                }
            }
            InputKind::Right => {
                if let Some(next) = next_boundary(&self.text, self.cursor) {
                    self.cursor = next;
                    changed = true;
                }
            }
            InputKind::Home => {
                changed = self.cursor != 0;
                self.cursor = 0;
            }
            InputKind::End => {
                changed = self.cursor != self.text.len();
                self.cursor = self.text.len();
            }
            InputKind::CtrlA => {
                changed = self.cursor != 0;
                self.cursor = 0;
            }
            InputKind::CtrlE => {
                changed = self.cursor != self.text.len();
                self.cursor = self.text.len();
            }
            InputKind::CtrlK => {
                if self.cursor < self.text.len() {
                    self.text.truncate(self.cursor);
                    changed = true;
                }
            }
            InputKind::CtrlU => {
                if self.cursor > 0 {
                    self.text.replace_range(..self.cursor, "");
                    self.cursor = 0;
                    changed = true;
                }
            }
            InputKind::CtrlW => {
                let start = previous_word_boundary(&self.text, self.cursor);
                if start < self.cursor {
                    self.text.replace_range(start..self.cursor, "");
                    self.cursor = start;
                    changed = true;
                }
            }
            InputKind::CtrlL => {}
            InputKind::Enter => return Ok(MirrorOutcome::Submitted),
            InputKind::CtrlC => {
                self.text.clear();
                self.cursor = 0;
                changed = true;
            }
            InputKind::Up
            | InputKind::Down
            | InputKind::PageUp
            | InputKind::PageDown
            | InputKind::Tab
            | InputKind::BackTab
            | InputKind::Escape
            | InputKind::CtrlD
            | InputKind::CtrlR
            | InputKind::Raw => {
                self.sync = SyncQuality::Uncertain;
                return Ok(MirrorOutcome::Uncertain);
            }
        }
        if changed {
            self.advance_revision()?;
            Ok(MirrorOutcome::Changed)
        } else {
            Ok(MirrorOutcome::Unchanged)
        }
    }

    pub fn mark_uncertain(&mut self) {
        self.sync = SyncQuality::Uncertain;
    }

    fn advance_revision(&mut self) -> crate::Result<()> {
        self.revision = self
            .revision
            .checked_next()
            .ok_or_else(|| crate::Error::Runtime("buffer revision exhausted".into()))?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MirrorOutcome {
    Changed,
    Unchanged,
    Submitted,
    Uncertain,
}

fn previous_boundary(text: &str, cursor: usize) -> Option<usize> {
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index)
}

fn next_boundary(text: &str, cursor: usize) -> Option<usize> {
    text[cursor..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(index, _)| cursor + index)
        .or_else(|| (cursor < text.len()).then_some(text.len()))
}

fn previous_word_boundary(text: &str, cursor: usize) -> usize {
    let prefix = &text[..cursor];
    let mut start = cursor;
    let mut saw_word = false;
    for (index, grapheme) in prefix.grapheme_indices(true).rev() {
        if grapheme.chars().all(char::is_whitespace) {
            if saw_word {
                break;
            }
        } else {
            saw_word = true;
        }
        start = index;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_edits_graphemes_not_bytes() {
        let mut buffer = EditableBuffer::new(SyncQuality::Mirrored);
        buffer
            .apply_mirrored(&InputKind::Text("a中".into()))
            .expect("insert");
        buffer
            .apply_mirrored(&InputKind::Backspace)
            .expect("backspace");
        assert_eq!(buffer.text, "a");
        assert_eq!(buffer.cursor, 1);
    }

    #[test]
    fn mirror_supports_standard_emacs_line_edits() {
        let mut buffer = EditableBuffer::new(SyncQuality::Mirrored);
        buffer
            .apply_mirrored(&InputKind::Text("alpha beta".into()))
            .expect("insert");
        buffer
            .apply_mirrored(&InputKind::CtrlW)
            .expect("word erase");
        assert_eq!(buffer.text, "alpha ");
        buffer.apply_mirrored(&InputKind::CtrlA).expect("home");
        buffer
            .apply_mirrored(&InputKind::CtrlK)
            .expect("kill suffix");
        assert!(buffer.text.is_empty());
    }
}
