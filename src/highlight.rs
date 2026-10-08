//! Syntax highlighting with syntect and the two-face syntax and theme set (docs/design.md §13.6).
//!
//! Output uses CSS classes (`hl-…`) rather than inline styles, so it works under the web UI's
//! Content-Security-Policy and follows the light or dark theme through [`theme_css`]. Each line is
//! a balanced HTML fragment: the scopes open at its start are re-opened there and every span is
//! closed at its end, so the file view, diff rows and snippets can show any line on its own.

use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use lru::LruCache;
use syntect::html::{ClassStyle, css_for_theme_with_class_style, line_tokens_to_classed_spans};
use syntect::parsing::{ParseState, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet};
use two_face::theme::EmbeddedThemeName;

/// Files larger than this are shown as plain text.
pub const MAX_HIGHLIGHT_BYTES: usize = 1 << 20;
/// Files with more lines than this are shown as plain text.
pub const MAX_HIGHLIGHT_LINES: usize = 20_000;
/// Upper bound on the memory held by cached highlighted files.
const CACHE_BYTES: usize = 32 << 20;

const STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "hl-" };
const LIGHT_THEME: EmbeddedThemeName = EmbeddedThemeName::InspiredGithub;
const DARK_THEME: EmbeddedThemeName = EmbeddedThemeName::OneHalfDark;

/// Loaded on first use, once per process; the MCP server never needs it.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(Cache::new(CACHE_BYTES)));

static THEME_CSS: LazyLock<String> = LazyLock::new(build_theme_css);

/// A file's lines as HTML, one balanced fragment per line, numbered like
/// [`crate::anchor::split_lines`].
#[derive(Debug)]
pub struct HighlightedLines {
    pub lines: Vec<String>,
    /// Why the lines are plain text, e.g. "too large to highlight"; `None` when highlighted
    /// (including files in a language LANI does not know, which are plain text by nature).
    pub plain_reason: Option<&'static str>,
    /// The detected language, e.g. "Rust", or "Plain Text".
    pub language: String,
}

/// Highlight `lines` of the file named `file_name`, whose content is the git blob `blob_sha`.
///
/// Results are cached by blob and file name: a blob's content never changes, and the name (with
/// the content's first line) decides the language.
#[must_use]
pub fn highlight_file(blob_sha: &str, file_name: &str, lines: &[String]) -> Arc<HighlightedLines> {
    let key = format!("{blob_sha}\0{file_name}");
    if let Some(cached) = lock_cache().get(&key) {
        return cached;
    }
    let highlighted = Arc::new(highlight_lines(file_name, lines));
    lock_cache().insert(key, Arc::clone(&highlighted));
    highlighted
}

/// Highlight `lines` of a file named `file_name`, without caching.
#[must_use]
pub fn highlight_lines(file_name: &str, lines: &[String]) -> HighlightedLines {
    let first_line = lines.first().map_or("", String::as_str);
    let syntax = syntax_for_file(file_name, first_line);
    let total_bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
    let plain_reason = if total_bytes > MAX_HIGHLIGHT_BYTES {
        Some("larger than 1 MiB, so it is not highlighted")
    } else if lines.len() > MAX_HIGHLIGHT_LINES {
        Some("longer than 20,000 lines, so it is not highlighted")
    } else {
        None
    };
    let highlighted = match plain_reason {
        Some(_) => None,
        None => highlight_with(syntax, lines.iter().map(String::as_str)),
    };
    match highlighted {
        Some(lines) => HighlightedLines {
            lines,
            plain_reason: None,
            language: syntax.name.clone(),
        },
        None => HighlightedLines {
            lines: lines.iter().map(|line| escape(line)).collect(),
            plain_reason: plain_reason.or(Some("could not be highlighted")),
            language: syntax.name.clone(),
        },
    }
}

/// Highlight a Markdown code block whose fence names `language` (e.g. "rust" or "py"). Unknown
/// languages are shown as escaped plain text.
#[must_use]
pub fn highlight_snippet(language: &str, code: &str) -> String {
    let syntax = SYNTAXES
        .find_syntax_by_token(language.trim())
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());
    let body = code.strip_suffix('\n').unwrap_or(code);
    let lines = body.split('\n');
    let html = highlight_with(syntax, lines.clone()).unwrap_or_else(|| lines.map(escape).collect());
    let mut out = html.join("\n");
    if code.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// The stylesheet for highlighted code: the light theme, and the dark theme under
/// `prefers-color-scheme: dark`. Each theme is confined to its scheme, because one theme has
/// rules for scopes the other leaves alone, and a light colour must not leak into dark mode.
#[must_use]
pub fn theme_css() -> &'static str {
    &THEME_CSS
}

fn syntax_for_file(file_name: &str, first_line: &str) -> &'static SyntaxReference {
    let extension = file_name.rsplit_once('.').map(|(_, extension)| extension);
    // Whole names first, for files such as `Makefile` or `Dockerfile`, then the extension.
    SYNTAXES
        .find_syntax_by_extension(file_name)
        .or_else(|| extension.and_then(|extension| SYNTAXES.find_syntax_by_extension(extension)))
        .or_else(|| SYNTAXES.find_syntax_by_first_line(first_line))
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text())
}

/// One balanced HTML fragment per line, or `None` if syntect fails on the input.
fn highlight_with<'a>(
    syntax: &SyntaxReference,
    lines: impl Iterator<Item = &'a str>,
) -> Option<Vec<String>> {
    let mut parser = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for line in lines {
        // The syntaxes expect each line with its newline.
        let line = format!("{line}\n");
        let ops = parser.parse_line(&line, &SYNTAXES).ok()?;
        let mut html = reopen_spans(&stack)?;
        let (spans, _) = line_tokens_to_classed_spans(&line, &ops, STYLE, &mut stack).ok()?;
        // Only the final character can be a newline, and tags never contain one.
        html.push_str(&spans.replace('\n', ""));
        html.push_str(&"</span>".repeat(stack.len()));
        out.push(html);
    }
    Some(out)
}

/// Opening tags for the scopes in `stack`, outermost first, as syntect writes them.
fn reopen_spans(stack: &ScopeStack) -> Option<String> {
    let pushes: Vec<(usize, ScopeStackOp)> = stack
        .as_slice()
        .iter()
        .map(|scope| (0, ScopeStackOp::Push(*scope)))
        .collect();
    line_tokens_to_classed_spans("", &pushes, STYLE, &mut ScopeStack::new())
        .ok()
        .map(|(html, _)| html)
}

fn build_theme_css() -> String {
    let themes = two_face::theme::extra();
    let light = css_for_theme_with_class_style(themes.get(LIGHT_THEME), STYLE).unwrap_or_default();
    let dark = css_for_theme_with_class_style(themes.get(DARK_THEME), STYLE).unwrap_or_default();
    format!(
        "/* Highlighting: {} (light) and {} (dark), generated by syntect. */\n\
         @media not all and (prefers-color-scheme: dark) {{\n{light}\n}}\n\
         @media (prefers-color-scheme: dark) {{\n{dark}\n}}\n",
        LIGHT_THEME.as_name(),
        DARK_THEME.as_name()
    )
}

/// Escape text for HTML element content and attribute values.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn lock_cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An LRU cache of highlighted files, bounded by the bytes of HTML it holds.
struct Cache {
    entries: LruCache<String, Arc<HighlightedLines>>,
    bytes: usize,
    max_bytes: usize,
}

impl Cache {
    fn new(max_bytes: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            bytes: 0,
            max_bytes,
        }
    }

    fn get(&mut self, key: &str) -> Option<Arc<HighlightedLines>> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: String, value: Arc<HighlightedLines>) {
        let key_bytes = key.len();
        let size = entry_size(key_bytes, &value);
        if size > self.max_bytes {
            return;
        }
        if let Some(previous) = self.entries.put(key, value) {
            self.bytes -= entry_size(key_bytes, &previous);
        }
        self.bytes += size;
        while self.bytes > self.max_bytes {
            let Some((key, evicted)) = self.entries.pop_lru() else {
                break;
            };
            self.bytes -= entry_size(key.len(), &evicted);
        }
    }
}

fn entry_size(key_bytes: usize, value: &HighlightedLines) -> usize {
    key_bytes + value.lines.iter().map(String::len).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        crate::anchor::split_lines(text.as_bytes())
    }

    /// Whether every `<span` in `html` is closed within it.
    fn balanced(html: &str) -> bool {
        html.matches("<span").count() == html.matches("</span>").count()
    }

    #[test]
    fn every_line_is_a_balanced_fragment_even_inside_multi_line_constructs() {
        let source =
            lines("fn main() {\n    /* a comment\n       over lines */\n    let s = \"x\";\n}\n");
        let highlighted = highlight_lines("main.rs", &source);
        assert_eq!(highlighted.language, "Rust");
        assert_eq!(highlighted.plain_reason, None);
        assert_eq!(highlighted.lines.len(), 5);
        for line in &highlighted.lines {
            assert!(balanced(line), "{line}");
            assert!(!line.contains('\n'), "{line:?}");
        }
        // The comment's second line is still inside the comment scope.
        assert!(
            highlighted.lines[2].contains("hl-comment"),
            "{}",
            highlighted.lines[2]
        );
    }

    #[test]
    fn languages_are_detected_by_name_extension_and_first_line() {
        assert_eq!(
            highlight_lines("Makefile", &lines("all:\n")).language,
            "Makefile"
        );
        assert_eq!(
            highlight_lines("x.py", &lines("x = 1\n")).language,
            "Python"
        );
        assert_eq!(
            highlight_lines("run", &lines("#!/bin/bash\necho hi\n")).language,
            "Bourne Again Shell (bash)"
        );
        assert_eq!(
            highlight_lines("notes", &lines("hello\n")).language,
            "Plain Text"
        );
    }

    #[test]
    fn text_is_escaped() {
        let highlighted = highlight_lines("notes", &lines("<script>alert('x') & \"y\"</script>\n"));
        assert_eq!(highlighted.lines.len(), 1);
        assert!(
            !highlighted.lines[0].contains("<script"),
            "{}",
            highlighted.lines[0]
        );
        assert!(
            highlighted.lines[0].contains("&lt;script&gt;"),
            "{}",
            highlighted.lines[0]
        );

        let rust = highlight_lines("a.rs", &lines("let s = \"</span><script>\";\n"));
        assert!(!rust.lines[0].contains("<script"), "{}", rust.lines[0]);
        assert!(balanced(&rust.lines[0]), "{}", rust.lines[0]);
    }

    #[test]
    fn large_files_are_plain_text() {
        let many = vec!["x".to_string(); MAX_HIGHLIGHT_LINES + 1];
        let highlighted = highlight_lines("a.rs", &many);
        assert_eq!(highlighted.lines.len(), MAX_HIGHLIGHT_LINES + 1);
        assert!(highlighted.plain_reason.unwrap().contains("20,000 lines"));

        let wide = vec!["y".repeat(MAX_HIGHLIGHT_BYTES)];
        assert!(
            highlight_lines("a.rs", &wide)
                .plain_reason
                .unwrap()
                .contains("1 MiB")
        );
    }

    #[test]
    fn snippets_use_the_fence_language() {
        let html = highlight_snippet("rust", "fn f() {}\n");
        assert!(html.contains("hl-"), "{html}");
        assert!(html.ends_with('\n'));
        let unknown = highlight_snippet("no-such-language", "<b>\n");
        assert!(
            unknown.contains("&lt;b&gt;") && !unknown.contains("<b>"),
            "{unknown}"
        );
    }

    #[test]
    fn the_cache_is_bounded_by_bytes_and_evicts_the_least_recently_used() {
        let entry = |text: &str| {
            Arc::new(HighlightedLines {
                lines: vec![text.to_string()],
                plain_reason: None,
                language: String::new(),
            })
        };
        let mut cache = Cache::new(30);
        cache.insert("a".into(), entry("0123456789"));
        cache.insert("b".into(), entry("0123456789"));
        assert!(cache.get("a").is_some(), "touch a, so b is least recent");
        cache.insert("c".into(), entry("0123456789"));
        assert!(cache.get("b").is_none());
        assert!(cache.get("a").is_some() && cache.get("c").is_some());
        assert!(cache.bytes <= 30);
        cache.insert("huge".into(), entry(&"x".repeat(100)));
        assert!(
            cache.get("huge").is_none(),
            "an entry over the cap is not cached"
        );
    }

    #[test]
    fn theme_css_has_light_and_dark_rules() {
        let css = theme_css();
        assert!(css.contains(".hl-"), "{}", &css[..css.len().min(400)]);
        assert!(css.contains("@media not all and (prefers-color-scheme: dark) {"));
        assert!(css.contains("@media (prefers-color-scheme: dark) {"));
    }
}
