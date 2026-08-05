use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum QuoteContext {
    #[default]
    Unquoted,
    Single,
    Double,
    Opaque,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Word,
    Whitespace,
    Pipe,
    AndIf,
    OrIf,
    Separator,
    Redirect,
    Comment,
    Opaque,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
    pub cooked_prefix: String,
    pub quote: QuoteContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedLine {
    pub tokens: Vec<Token>,
    pub active_segment: Range<usize>,
    pub replacement: Range<usize>,
    pub quote: QuoteContext,
    pub command: Option<String>,
    /// Token range of the effective command word. Callers replacing whole
    /// lines must start the edit here so a wrapper/assignment prefix
    /// (`sudo …`, `FOO=bar …`) is preserved.
    pub command_range: Option<Range<usize>>,
    pub current_prefix: String,
}

pub fn parse_line(text: &str, cursor: usize) -> Result<ParsedLine, crate::Error> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return Err(crate::Error::Parse(
            "cursor does not fall on a UTF-8 boundary".into(),
        ));
    }
    let tokens = lex(text);
    let segment_start = tokens
        .iter()
        .filter(|token| token.range.end <= cursor && is_segment_boundary(token.kind))
        .map(|token| token.range.end)
        .next_back()
        .unwrap_or(0);
    let segment_end = tokens
        .iter()
        .find(|token| token.range.start >= cursor && is_segment_boundary(token.kind))
        .map_or(text.len(), |token| token.range.start);
    let active_segment = segment_start..segment_end;

    let current = tokens.iter().find(|token| {
        token.kind == TokenKind::Word
            && token.range.start <= cursor
            && cursor <= token.range.end
            && token.range.start >= active_segment.start
            && token.range.end <= active_segment.end
    });
    let (replacement, quote, current_prefix) = current.map_or_else(
        || (cursor..cursor, quote_at(text, cursor), String::new()),
        |token| {
            let prefix = cook_word(&text[token.range.start..cursor]);
            (token.range.clone(), quote_at(text, cursor), prefix)
        },
    );
    let command_token = {
        let words: Vec<&Token> = tokens
            .iter()
            .filter(|token| {
                token.kind == TokenKind::Word
                    && token.range.start >= active_segment.start
                    && token.range.end <= active_segment.end
            })
            .collect();
        let cooked: Vec<&str> = words
            .iter()
            .map(|token| token.cooked_prefix.as_str())
            .collect();
        effective_command_index(&cooked).map(|index| words[index])
    };
    let command = command_token.map(|token| token.cooked_prefix.clone());
    let command_range = command_token.map(|token| token.range.clone());

    Ok(ParsedLine {
        tokens,
        active_segment,
        replacement,
        quote,
        command,
        command_range,
        current_prefix,
    })
}

/// Wrappers that run another command: the word after them is the effective
/// command, unless an option (`-…`) sits in between — then peeling stops and
/// the wrapper itself stays the command (`sudo -u root ls` completes sudo's
/// own slots, not ls's).
const COMMAND_WRAPPERS: &[&str] = &[
    "sudo", "doas", "command", "builtin", "nohup", "time", "watch", "env",
];

/// Index of the effective command word within `words` (the cooked words of a
/// segment in order): leading `NAME=value` assignments and wrapper words
/// (`sudo`, `env`, …) are skipped. `env` may itself be followed by
/// assignments. `None` when only assignments/wrappers have been typed so far
/// (`sudo `) — there is no effective command yet.
pub(crate) fn effective_command_index(words: &[&str]) -> Option<usize> {
    let mut index = 0;
    while words
        .get(index)
        .is_some_and(|word| is_assignment_word(word))
    {
        index += 1;
    }
    loop {
        let word = words.get(index).copied()?;
        if !COMMAND_WRAPPERS.contains(&word) {
            return Some(index);
        }
        let wrapper = index;
        index += 1;
        if word == "env" {
            while words
                .get(index)
                .is_some_and(|next| is_assignment_word(next))
            {
                index += 1;
            }
        }
        match words.get(index) {
            None => return None,
            // An option between wrapper and command ends the peeling.
            Some(next) if next.starts_with('-') => return Some(wrapper),
            Some(_) => {}
        }
    }
}

/// A leading `NAME=value` environment assignment word.
fn is_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn lex(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            push_token(
                &mut tokens,
                TokenKind::Whitespace,
                start..index,
                text,
                QuoteContext::Unquoted,
            );
            continue;
        }
        if matches!(bytes[index..], [b'<', b'(', ..] | [b'>', b'(', ..]) {
            index = consume_opaque(bytes, index + 2);
            push_token(
                &mut tokens,
                TokenKind::Opaque,
                start..index,
                text,
                QuoteContext::Opaque,
            );
            continue;
        }
        let (kind, width) = match bytes[index..] {
            [b'&', b'&', ..] => (Some(TokenKind::AndIf), 2),
            // `&>` redirects both streams; lexing it as `&` + `>` would
            // invent a background separator that is not there.
            [b'&', b'>', ..] => (Some(TokenKind::Redirect), 2),
            [b'&', ..] => (Some(TokenKind::Separator), 1),
            [b'|', b'|', ..] => (Some(TokenKind::OrIf), 2),
            [b'|', ..] => (Some(TokenKind::Pipe), 1),
            [b';', ..] => (Some(TokenKind::Separator), 1),
            // `>&` (fd duplication / both-streams redirect) and `>>&` are
            // single operators, not `>` followed by a background `&`.
            [b'>', b'>', b'&', ..] => (Some(TokenKind::Redirect), 3),
            [b'>', b'&', ..] => (Some(TokenKind::Redirect), 2),
            [b'<', ..] | [b'>', ..] => (Some(TokenKind::Redirect), 1),
            [b'#', ..] => (Some(TokenKind::Comment), bytes.len() - index),
            _ => (None, 0),
        };
        if let Some(kind) = kind {
            index += width;
            push_token(
                &mut tokens,
                kind,
                start..index,
                text,
                QuoteContext::Unquoted,
            );
            continue;
        }

        let mut quote = QuoteContext::Unquoted;
        let mut token_quote = QuoteContext::Unquoted;
        while index < bytes.len() {
            let byte = bytes[index];
            match quote {
                QuoteContext::Unquoted => match byte {
                    b'\'' => {
                        quote = QuoteContext::Single;
                        if token_quote != QuoteContext::Opaque {
                            token_quote = QuoteContext::Single;
                        }
                        index += 1;
                    }
                    b'"' => {
                        quote = QuoteContext::Double;
                        if token_quote != QuoteContext::Opaque {
                            token_quote = QuoteContext::Double;
                        }
                        index += 1;
                    }
                    b'\\' => index = (index + 2).min(bytes.len()),
                    b'$' if bytes.get(index + 1) == Some(&b'(') => {
                        index = consume_opaque(bytes, index + 2);
                        token_quote = QuoteContext::Opaque;
                    }
                    b'$' if is_zsh_eval_expansion(bytes, index) => {
                        index += 1;
                        token_quote = QuoteContext::Opaque;
                    }
                    b'`' => {
                        index = consume_backticks(bytes, index + 1);
                        token_quote = QuoteContext::Opaque;
                    }
                    b if b.is_ascii_whitespace() || b"|;&<>".contains(&b) => break,
                    _ => index += utf8_width_at(bytes, index),
                },
                QuoteContext::Single => {
                    index += utf8_width_at(bytes, index);
                    if byte == b'\'' {
                        quote = QuoteContext::Unquoted;
                    }
                }
                QuoteContext::Double => match byte {
                    b'"' => {
                        quote = QuoteContext::Unquoted;
                        index += 1;
                    }
                    b'\\' => index = (index + 2).min(bytes.len()),
                    b'$' if bytes.get(index + 1) == Some(&b'(') => {
                        index = consume_opaque(bytes, index + 2);
                        token_quote = QuoteContext::Opaque;
                    }
                    b'$' if is_zsh_eval_expansion(bytes, index) => {
                        index += 1;
                        token_quote = QuoteContext::Opaque;
                    }
                    b'`' => {
                        index = consume_backticks(bytes, index + 1);
                        token_quote = QuoteContext::Opaque;
                    }
                    _ => index += utf8_width_at(bytes, index),
                },
                QuoteContext::Opaque => index += utf8_width_at(bytes, index),
            }
        }
        push_token(
            &mut tokens,
            TokenKind::Word,
            start..index,
            text,
            token_quote,
        );
    }
    tokens
}

fn push_token(
    tokens: &mut Vec<Token>,
    kind: TokenKind,
    range: Range<usize>,
    text: &str,
    quote: QuoteContext,
) {
    tokens.push(Token {
        kind,
        cooked_prefix: if kind == TokenKind::Word {
            cook_word(&text[range.clone()])
        } else {
            String::new()
        },
        range,
        quote,
    });
}

fn quote_at(text: &str, cursor: usize) -> QuoteContext {
    let prefix = &text[..cursor];
    let mut quote = QuoteContext::Unquoted;
    let mut escaped = false;
    for character in prefix.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, character) {
            (QuoteContext::Unquoted | QuoteContext::Double, '\\') => escaped = true,
            (QuoteContext::Unquoted, '\'') => quote = QuoteContext::Single,
            (QuoteContext::Unquoted, '"') => quote = QuoteContext::Double,
            (QuoteContext::Single, '\'') | (QuoteContext::Double, '"') => {
                quote = QuoteContext::Unquoted;
            }
            _ => {}
        }
    }
    quote
}

fn cook_word(raw: &str) -> String {
    let mut output = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' | '"' => {}
            '\\' => {
                if let Some(next) = chars.next() {
                    output.push(next);
                }
            }
            _ => output.push(character),
        }
    }
    output
}

const fn is_segment_boundary(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Pipe | TokenKind::AndIf | TokenKind::OrIf | TokenKind::Separator
    )
}

fn consume_opaque(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 1_u32;
    while index < bytes.len() && depth > 0 {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'\\' => index = (index + 1).min(bytes.len()),
            _ => {}
        }
        index += 1;
    }
    index
}

fn consume_backticks(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        match bytes[index] {
            b'`' => return index + 1,
            b'\\' => index = (index + 2).min(bytes.len()),
            _ => index += 1,
        }
    }
    index
}

fn is_zsh_eval_expansion(bytes: &[u8], index: usize) -> bool {
    let Some(flags) = bytes.get(index.saturating_add(3)..) else {
        return false;
    };
    if bytes.get(index..index.saturating_add(3)) != Some(b"${(".as_slice()) {
        return false;
    }
    let Some(end) = flags.iter().position(|byte| *byte == b')') else {
        return false;
    };
    flags[..end].contains(&b'e')
}

fn utf8_width_at(bytes: &[u8], index: usize) -> usize {
    let width = match bytes[index] {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    width.min(bytes.len() - index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_segment_command_and_replacement() {
        let text = "cat data | rg 'fo";
        let parsed = parse_line(text, text.len()).expect("line should parse");
        assert_eq!(&text[parsed.active_segment], " rg 'fo");
        assert_eq!(parsed.command.as_deref(), Some("rg"));
        assert_eq!(&text[parsed.replacement], "'fo");
        assert_eq!(parsed.current_prefix, "fo");
        assert_eq!(parsed.quote, QuoteContext::Single);
    }

    #[test]
    fn accepts_incomplete_unicode_quotes_and_opaque_substitutions() {
        let text = "echo \"中 $(date";
        let parsed = parse_line(text, text.len()).expect("incomplete input should parse");
        assert_eq!(parsed.command.as_deref(), Some("echo"));
        assert!(
            parsed
                .tokens
                .iter()
                .any(|token| token.quote == QuoteContext::Opaque)
        );
    }

    #[test]
    fn marks_executable_substitutions_as_opaque_in_every_executable_context() {
        for text in [
            "echo $(rm -rf /)",
            "echo `rm -rf /`",
            "echo \"$(rm -rf /)\"",
            "echo \"`rm -rf /`\"",
            "cat <(rm -rf /)",
            "cat >(rm -rf /)",
            "echo ${(e)payload}",
            "echo \"${(Xe)payload}\"",
            "echo ${${(e)name}}",
        ] {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            assert!(
                parsed
                    .tokens
                    .iter()
                    .any(|token| token.quote == QuoteContext::Opaque),
                "missing opaque token for {text:?}"
            );
        }
    }

    #[test]
    fn fd_duplication_redirects_lex_as_single_redirect_tokens() {
        for (text, redirect) in [
            ("echo hi 2>&1", ">&"),
            ("echo hi &> file", "&>"),
            ("echo hi >>& file", ">>&"),
        ] {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            let redirects: Vec<_> = parsed
                .tokens
                .iter()
                .filter(|token| token.kind == TokenKind::Redirect)
                .map(|token| &text[token.range.clone()])
                .collect();
            assert!(
                redirects.contains(&redirect),
                "missing {redirect:?} redirect token in {text:?}: {redirects:?}"
            );
            assert!(
                !parsed
                    .tokens
                    .iter()
                    .any(|token| token.kind == TokenKind::Separator),
                "unexpected separator for {text:?}"
            );
        }
        // `&&` must still lex as AndIf, not `&` + `>&`.
        let parsed = parse_line("a && b", 5).expect("line should parse");
        assert!(
            parsed
                .tokens
                .iter()
                .any(|token| token.kind == TokenKind::AndIf)
        );
    }

    #[test]
    fn bare_background_operator_terminates_the_word() {
        // A lone `&` (background operator) must not wedge the lexer: it is a
        // separator, so `sleep 1 &` and a trailing `&` both tokenize.
        for text in ["sleep 1 &", "sleep 1 & wait", "echo &"] {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            assert!(
                parsed
                    .tokens
                    .iter()
                    .any(|token| token.kind == TokenKind::Separator),
                "missing separator for {text:?}"
            );
        }
        let parsed = parse_line("sleep 1 & wait", 13).expect("line should parse");
        assert_eq!(parsed.command.as_deref(), Some("wait"));
    }

    #[test]
    fn effective_command_skips_assignments_and_wrappers() {
        let cases: &[(&str, Option<&str>)] = &[
            ("FOO=bar ls ", Some("ls")),
            ("FOO=bar BAZ=qux ls -la", Some("ls")),
            ("sudo vim f", Some("vim")),
            ("sudo git checkout ", Some("git")),
            ("env FOO=bar ls ", Some("ls")),
            ("env FOO=bar sudo ls ", Some("ls")),
            ("time ls -la", Some("ls")),
            ("nohup make ", Some("make")),
            // Only wrappers/assignments so far: no effective command yet.
            ("sudo ", None),
            ("sudo", None),
            ("FOO=bar ", None),
            ("env FOO=bar ", None),
            // A dash-word between wrapper and command stops the peeling.
            ("sudo -u root ls ", Some("sudo")),
            ("env -i ls ", Some("env")),
            ("watch -n1 ls ", Some("watch")),
        ];
        for (text, expected) in cases {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            assert_eq!(
                parsed.command.as_deref(),
                *expected,
                "effective command for {text:?}"
            );
            assert_eq!(
                parsed
                    .command_range
                    .as_ref()
                    .map(|range| &text[range.clone()]),
                expected.map(|command| {
                    let start = text.rfind(command).expect("command in text");
                    &text[start..start + command.len()]
                }),
                "command range for {text:?}"
            );
        }
    }
}
