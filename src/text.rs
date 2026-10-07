//! Line breaks in text that people (or repositories) supply.
//!
//! LANI's MCP output marks every line of a comment or summary with a prefix so that text someone
//! wrote cannot pass for one of LANI's own lines (docs/design.md §12.4). That only works if every
//! character a reader might treat as a line break is handled, not just `\n`.

/// The character shown in place of a line break inside one of LANI's own single-line fields.
pub const LINE_BREAK_MARKER: char = '␤';

/// Whether some reader (a terminal, an editor, Unicode-aware text processing, a language model)
/// may treat `c` as a line break.
#[must_use]
pub fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// Split `text` at every line break, counting `\r\n` as one. Like [`str::lines`], a final line
/// break does not start another, empty line.
#[must_use]
pub fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if !is_line_break(c) {
            continue;
        }
        lines.push(&text[start..index]);
        start = index + c.len_utf8();
        if c == '\r' && matches!(chars.peek(), Some((_, '\n'))) {
            chars.next();
            start += 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Replace every line break other than `\n` with [`LINE_BREAK_MARKER`], so that the lines LANI
/// prints cannot be split further by a reader that honours other line-break characters.
#[must_use]
pub fn neutralize_line_breaks(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c != '\n' && is_line_break(c) {
                LINE_BREAK_MARKER
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEPARATORS: [&str; 7] = [
        "\n", "\r", "\u{0B}", "\u{0C}", "\u{85}", "\u{2028}", "\u{2029}",
    ];

    #[test]
    fn splits_at_every_line_break() {
        for separator in SEPARATORS {
            assert_eq!(
                split_lines(&format!("one{separator}two")),
                vec!["one", "two"],
                "{separator:?}"
            );
        }
        assert_eq!(
            split_lines("one\r\ntwo"),
            vec!["one", "two"],
            "CRLF is one break"
        );
        assert_eq!(split_lines("one\r\rtwo"), vec!["one", "", "two"]);
        assert_eq!(split_lines("a\n\nb\n"), vec!["a", "", "b"]);
        assert_eq!(split_lines(""), Vec::<&str>::new());
        assert_eq!(
            split_lines("é\u{2028}ü"),
            vec!["é", "ü"],
            "multi-byte characters"
        );
    }

    #[test]
    fn neutralizes_all_but_newline() {
        for separator in SEPARATORS.iter().skip(1) {
            assert_eq!(
                neutralize_line_breaks(&format!("a{separator}b\nc")),
                "a␤b\nc"
            );
        }
    }
}
