use super::*;

pub(super) fn contextual_flag_slot<'a>(
    command: &str,
    words: &[&str],
    position: usize,
    prefix: &'a str,
) -> Option<FilesystemSlot<'a>> {
    // `kubectl logs -f` means follow, while `kubectl get -f` takes a file.
    if command != "kubectl"
        || !positional_words_before(command, words, position).is_some_and(|words| {
            matches!(
                words.first().copied(),
                Some(
                    "get"
                        | "apply"
                        | "create"
                        | "replace"
                        | "delete"
                        | "diff"
                        | "describe"
                        | "edit"
                        | "label"
                        | "annotate"
                        | "patch"
                        | "scale"
                        | "wait"
                )
            )
        })
    {
        return None;
    }
    if words.get(position).copied() == Some("-f") {
        return Some(FilesystemSlot::plain(SlotKind::Path, prefix));
    }
    prefix
        .strip_prefix("-f")
        .filter(|value| !value.is_empty())
        .map(|value| FilesystemSlot {
            defer_to_recipes: false,
            kind: SlotKind::Path,
            prefix: value,
            edit_prefix: "-f",
            base_dir: None,
        })
}

pub(super) fn rooted(prefix: &str) -> bool {
    [
        "./", "../", "~/", "$HOME/", "${HOME}/", "$PWD/", "${PWD}/", "/",
    ]
    .iter()
    .any(|root| prefix.starts_with(root))
}

/// Never execute shell expansion to discover a scan directory. Unknown
/// variables, substitutions, named homes, and globbed parent components are
/// not literal directories. Quoted or escaped glob characters remain usable.
pub(super) fn directory_intent(
    context: &CompletionContext,
    slot: &FilesystemSlot<'_>,
) -> Option<bool> {
    let (directory, _) = split_prefix(slot.prefix);
    if directory.is_empty() {
        return Some(false);
    }
    if directory.split_once("://").is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    }) {
        return None;
    }
    let raw = &context.buffer.text[context.parsed.replacement.start..context.buffer.cursor];
    let start = context
        .parsed
        .current_prefix
        .len()
        .saturating_sub(slot.prefix.len());
    let end = start + directory.len();
    let known_root = ["~/", "$HOME/", "${HOME}/", "$PWD/", "${PWD}/"]
        .iter()
        .find(|root| directory.starts_with(**root))
        .map_or(0, |root| root.len());
    let mut quote = crate::parser::QuoteContext::Unquoted;
    let mut cooked = 0;
    let mut literal_root = false;
    let mut chars = raw.char_indices().peekable();
    while let Some((raw_index, c)) = chars.next() {
        if cooked >= end {
            break;
        }
        match c {
            '\'' if quote != crate::parser::QuoteContext::Double => {
                quote = if quote == crate::parser::QuoteContext::Single {
                    crate::parser::QuoteContext::Unquoted
                } else {
                    crate::parser::QuoteContext::Single
                };
                continue;
            }
            '"' if quote != crate::parser::QuoteContext::Single => {
                quote = if quote == crate::parser::QuoteContext::Double {
                    crate::parser::QuoteContext::Unquoted
                } else {
                    crate::parser::QuoteContext::Double
                };
                continue;
            }
            '\\' if quote != crate::parser::QuoteContext::Single => {
                // Match the parser's cooked offsets: inside double quotes a
                // backslash only escapes $, `, ", backslash, and newline.
                let escapes = quote == crate::parser::QuoteContext::Unquoted
                    || chars
                        .peek()
                        .is_some_and(|(_, next)| matches!(next, '$' | '`' | '"' | '\\' | '\n'));
                if escapes {
                    if let Some((_, escaped)) = chars.next()
                        && escaped != '\n'
                    {
                        if cooked == start && known_root > 0 {
                            literal_root = true;
                        }
                        cooked += escaped.len_utf8();
                    }
                    continue;
                }
            }
            _ => {}
        }
        if cooked == start && known_root > 0 {
            literal_root = (c == '$' && quote == crate::parser::QuoteContext::Single)
                || (c == '~' && quote != crate::parser::QuoteContext::Unquoted);
            if !literal_root {
                // Cooking removes quotes and escapes. `$HO""ME` may cook to
                // `$HOME` but expands a different variable. Authority requires
                // the variable token itself to be contiguous in the raw input.
                let marker = &directory[..known_root - 1];
                if !raw[raw_index..].starts_with(marker)
                    || (c == '~' && (raw_index != start || !raw[raw_index..].starts_with("~/")))
                {
                    return None;
                }
            }
        }
        if cooked >= start && cooked >= start + known_root {
            if quote != crate::parser::QuoteContext::Single && matches!(c, '$' | '`') {
                return None;
            }
            if quote == crate::parser::QuoteContext::Unquoted
                && (matches!(c, '*' | '?' | '[' | '{') || (c == '~' && cooked == start))
            {
                return None;
            }
        }
        cooked += c.len_utf8();
    }
    Some(literal_root)
}
