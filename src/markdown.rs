//! Markdown rendering for comments and summaries (docs/design.md §13.7).
//!
//! Raw HTML is never passed through: comrak escapes it (its default), and the same setting drops
//! dangerous link schemes such as `javascript:`. Remote images are additionally blocked by the
//! web server's Content-Security-Policy.

use comrak::{Options, markdown_to_html};

/// Render GitHub-flavored Markdown to HTML that is safe to embed in a page.
#[must_use]
pub fn render(markdown: &str) -> String {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    // Explicitly keep comrak's safe default: escape raw HTML and drop dangerous URLs.
    options.render.r#unsafe = false;
    markdown_to_html(markdown, &options)
}

/// Render a single line of Markdown as inline HTML, for one-line previews in lists. A line
/// that does not form a plain paragraph (a heading or a code fence, say) is shown as escaped text.
#[must_use]
pub fn render_inline(line: &str) -> String {
    let html = render(line);
    match html
        .trim_end()
        .strip_prefix("<p>")
        .and_then(|inner| inner.strip_suffix("</p>"))
    {
        Some(inner) if !inner.contains("<p>") => inner.to_string(),
        _ => escape(line),
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_gfm() {
        let html = render("**bold** ~~gone~~\n\n| a |\n|---|\n| b |\n\n- [x] done\n");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("<del>gone</del>"), "{html}");
        assert!(html.contains("<table>"), "{html}");
        assert!(html.contains("checkbox"), "{html}");
    }

    #[test]
    fn inline_rendering() {
        assert_eq!(
            render_inline("`add` is *nice*"),
            "<code>add</code> is <em>nice</em>"
        );
        assert_eq!(render_inline("# Heading <b>"), "# Heading &lt;b&gt;");
        assert_eq!(
            render_inline("<script>x</script>"),
            "&lt;script&gt;x&lt;/script&gt;"
        );
    }

    #[test]
    fn hostile_input_is_neutralized() {
        let hostile = [
            "<script>alert(1)</script>",
            "<img src=x onerror=alert(1)>",
            "<a href=\"javascript:alert(1)\">x</a>",
            "[click](javascript:alert(1))",
            "[click](vbscript:msgbox(1))",
            "<iframe src=\"https://example.com\"></iframe>",
            "<style>body{display:none}</style>",
            "{{ 7*7 }} {% raw %}",
        ];
        for input in hostile {
            let html = render(input).to_lowercase();
            assert!(!html.contains("<script"), "{input} → {html}");
            assert!(!html.contains("<img src=x"), "{input} → {html}");
            assert!(!html.contains("javascript:"), "{input} → {html}");
            assert!(!html.contains("vbscript:"), "{input} → {html}");
            assert!(!html.contains("<iframe"), "{input} → {html}");
            assert!(!html.contains("<style"), "{input} → {html}");
        }
    }
}
