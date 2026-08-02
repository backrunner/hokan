use std::{fmt, ops::Range};

use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditError {
    OutOfBounds,
    NotUtf8Boundary,
    NotGraphemeBoundary,
}

impl fmt::Display for EditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds => formatter.write_str("edit range is outside the buffer"),
            Self::NotUtf8Boundary => formatter.write_str("edit range splits a UTF-8 code point"),
            Self::NotGraphemeBoundary => formatter.write_str("edit range splits a grapheme"),
        }
    }
}

pub fn apply_edit(text: &str, range: Range<usize>, replacement: &str) -> Result<String, EditError> {
    if range.start > range.end || range.end > text.len() {
        return Err(EditError::OutOfBounds);
    }
    if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
        return Err(EditError::NotUtf8Boundary);
    }
    let boundaries: Vec<_> = text
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect();
    if !boundaries.contains(&range.start) || !boundaries.contains(&range.end) {
        return Err(EditError::NotGraphemeBoundary);
    }
    let mut output =
        String::with_capacity(text.len() - (range.end - range.start) + replacement.len());
    output.push_str(&text[..range.start]);
    output.push_str(replacement);
    output.push_str(&text[range.end..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_splits_combining_graphemes() {
        let text = "a\u{301}b";
        assert_eq!(
            apply_edit(text, 0..3, "x").expect("whole grapheme can be replaced"),
            "xb"
        );
        assert_eq!(
            apply_edit(text, 1..3, "x"),
            Err(EditError::NotGraphemeBoundary)
        );
    }
}
