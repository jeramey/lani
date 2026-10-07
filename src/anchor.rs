//! Anchor mechanics: path validation, file lines, snippets, and following a line range across
//! commits (docs/design.md §10). Everything here is pure; the service supplies git data.

use crate::error::{Error, Result};
use crate::git::Hunk;

/// Context lines captured on each side of an anchored range.
pub const SNIPPET_CONTEXT_LINES: u32 = 3;
/// Upper bound on the lines stored in a snippet.
pub const SNIPPET_MAX_LINES: u32 = 200;
/// Files with a NUL byte in this many leading bytes are treated as binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Require a clean path relative to the repository root, such as `src/main.rs`.
pub fn validate_path(path: &str) -> Result<()> {
    let problem = if path.is_empty() {
        Some("it is empty")
    } else if path.starts_with('/') {
        Some("it is absolute")
    } else if path.contains('\0') {
        Some("it contains a NUL byte")
    } else if path
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        Some("it contains an empty, `.` or `..` component")
    } else {
        None
    };
    match problem {
        Some(problem) => Err(Error::Invalid(format!(
            "invalid path {path:?}: {problem}; give a path relative to the repository root, like src/main.rs"
        ))),
        None => Ok(()),
    }
}

/// A text file split into lines. Line numbers are 1-based.
#[derive(Clone, Debug)]
pub struct FileLines {
    lines: Vec<String>,
}

impl FileLines {
    /// Split file content into lines, without line terminators (`\n` or `\r\n`).
    /// Binary files are rejected because issues cannot be anchored to them.
    pub fn from_bytes(path: &str, bytes: &[u8]) -> Result<Self> {
        let sniffed = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
        if sniffed.contains(&0) {
            return Err(Error::Invalid(format!(
                "{} is a binary file; issues can only be anchored to text files",
                crate::text::quoted(path)
            )));
        }
        let text = String::from_utf8_lossy(bytes);
        let body = text.strip_suffix('\n').unwrap_or(&text);
        let lines = if text.is_empty() {
            Vec::new()
        } else {
            body.split('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
                .collect()
        };
        Ok(Self { lines })
    }

    #[must_use]
    pub fn len(&self) -> u32 {
        u32::try_from(self.lines.len()).unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The text of line `number`, if it exists.
    #[must_use]
    pub fn line(&self, number: u32) -> Option<&str> {
        let index = usize::try_from(number.checked_sub(1)?).ok()?;
        self.lines.get(index).map(String::as_str)
    }

    /// Line numbers (1-based) whose trimmed text equals `text` trimmed.
    #[must_use]
    pub fn find(&self, text: &str) -> Vec<u32> {
        let wanted = text.trim();
        (1..=self.len())
            .filter(|&number| self.line(number).is_some_and(|line| line.trim() == wanted))
            .collect()
    }

    /// The range `start..=end` plus context lines, capped at [`SNIPPET_MAX_LINES`].
    /// Returns the first line's number and the lines. `start..=end` must be within the file.
    #[must_use]
    pub fn excerpt(&self, start: u32, end: u32) -> (u32, Vec<&str>) {
        let first = start.saturating_sub(SNIPPET_CONTEXT_LINES).max(1);
        let last = end
            .saturating_add(SNIPPET_CONTEXT_LINES)
            .min(self.len())
            .min(first + SNIPPET_MAX_LINES - 1);
        let lines = (first..=last)
            .filter_map(|number| self.line(number))
            .collect();
        (first, lines)
    }
}

/// Where a line range ends up after a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relocation {
    /// The lines are unchanged and now sit at these line numbers.
    Moved { start_line: u32, end_line: u32 },
    /// The change touched the lines themselves (or inserted lines inside the range).
    Changed,
}

/// Follow the inclusive range `start..=end` through `hunks` (ordered by `old_start`, as git
/// prints them), from old line numbers to new ones.
#[must_use]
pub fn relocate(hunks: &[Hunk], start: u32, end: u32) -> Relocation {
    let mut offset = 0_i64;
    for hunk in hunks {
        if hunk.old_count == 0 {
            // A pure insertion after old line `old_start`.
            if hunk.old_start < start {
                offset += i64::from(hunk.new_count);
            } else if hunk.old_start >= end {
                break;
            } else {
                return Relocation::Changed;
            }
        } else {
            let old_end = hunk.old_start + hunk.old_count - 1;
            if old_end < start {
                offset += i64::from(hunk.new_count) - i64::from(hunk.old_count);
            } else if hunk.old_start > end {
                break;
            } else {
                return Relocation::Changed;
            }
        }
    }
    let shift = |line: u32| {
        u32::try_from(i64::from(line) + offset)
            .ok()
            .filter(|&shifted| shifted >= 1)
    };
    match (shift(start), shift(end)) {
        (Some(start_line), Some(end_line)) => Relocation::Moved {
            start_line,
            end_line,
        },
        // Only reachable with inconsistent hunks; treat the range as unplaceable.
        _ => Relocation::Changed,
    }
}

/// The outcome of comparing a committed file with the copy on disk for an anchored range
/// (docs/design.md §10.1, step 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskCheck {
    /// The range has the same line numbers and content on disk as in the commit.
    Consistent,
    /// Uncommitted changes touch the range itself.
    RangeChanged,
    /// Changes above the range shift its line numbers, so it is ambiguous which version the
    /// caller's numbers came from. `if_read_from_disk` maps the caller's numbers, taken as
    /// on-disk numbers, back to the commit.
    Shifted { if_read_from_disk: Relocation },
}

/// Check `start..=end` given the hunks from the commit to the file on disk.
#[must_use]
pub fn check_disk(commit_to_disk: &[Hunk], start: u32, end: u32) -> DiskCheck {
    match relocate(commit_to_disk, start, end) {
        Relocation::Changed => DiskCheck::RangeChanged,
        Relocation::Moved { start_line, .. } if start_line == start => DiskCheck::Consistent,
        Relocation::Moved { .. } => {
            let disk_to_commit: Vec<Hunk> = commit_to_disk.iter().map(|h| h.reversed()).collect();
            DiskCheck::Shifted {
                if_read_from_disk: relocate(&disk_to_commit, start, end),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> Hunk {
        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        }
    }

    /// "1\n2\n…count\n"
    fn numbered_lines(count: u32) -> String {
        (1..=count).fold(String::new(), |mut text, n| {
            text.push_str(&n.to_string());
            text.push('\n');
            text
        })
    }

    fn moved(start_line: u32, end_line: u32) -> Relocation {
        Relocation::Moved {
            start_line,
            end_line,
        }
    }

    #[test]
    fn paths() {
        assert!(validate_path("src/main.rs").is_ok());
        assert!(validate_path("a b/c-d.txt").is_ok());
        for bad in ["", "/etc/passwd", "./a", "a/../b", "a//b", "a/", ".."] {
            assert!(validate_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn lines_handle_terminators() {
        let file = FileLines::from_bytes("f", b"one\r\ntwo\nthree\n").unwrap();
        assert_eq!(file.len(), 3);
        assert_eq!(file.line(1), Some("one"));
        assert_eq!(file.line(3), Some("three"));
        assert_eq!(file.line(4), None);
        assert_eq!(file.line(0), None);
        assert_eq!(FileLines::from_bytes("f", b"no newline").unwrap().len(), 1);
        assert_eq!(FileLines::from_bytes("f", b"").unwrap().len(), 0);
        assert_eq!(FileLines::from_bytes("f", b"\n").unwrap().len(), 1);
    }

    #[test]
    fn binary_files_are_rejected() {
        assert!(FileLines::from_bytes("f", b"PNG\0\x01").is_err());
    }

    #[test]
    fn find_matches_trimmed_text() {
        let file = FileLines::from_bytes("f", b"  let x = 1;\nlet y = 2;\nlet x = 1;\n").unwrap();
        assert_eq!(file.find("let x = 1;  "), vec![1, 3]);
    }

    #[test]
    fn excerpt_adds_context_and_clamps() {
        let text = numbered_lines(10);
        let file = FileLines::from_bytes("f", text.as_bytes()).unwrap();
        assert_eq!(
            file.excerpt(5, 5),
            (2, vec!["2", "3", "4", "5", "6", "7", "8"])
        );
        assert_eq!(file.excerpt(1, 2), (1, vec!["1", "2", "3", "4", "5"]));
        assert_eq!(file.excerpt(9, 10), (6, vec!["6", "7", "8", "9", "10"]));
    }

    #[test]
    fn excerpt_is_capped() {
        let text = numbered_lines(500);
        let file = FileLines::from_bytes("f", text.as_bytes()).unwrap();
        let (first, lines) = file.excerpt(10, 400);
        assert_eq!(first, 7);
        assert_eq!(lines.len(), SNIPPET_MAX_LINES as usize);
    }

    #[test]
    fn relocate_unchanged() {
        assert_eq!(relocate(&[], 10, 12), moved(10, 12));
    }

    #[test]
    fn relocate_shifts_for_changes_before() {
        // Two lines inserted after line 3; one line deleted at line 5 → net +1.
        let hunks = [hunk(3, 0, 4, 2), hunk(5, 1, 6, 0)];
        assert_eq!(relocate(&hunks, 10, 12), moved(11, 13));
    }

    #[test]
    fn relocate_insertion_at_top() {
        assert_eq!(relocate(&[hunk(0, 0, 1, 3)], 1, 2), moved(4, 5));
    }

    #[test]
    fn relocate_ignores_changes_after() {
        assert_eq!(relocate(&[hunk(13, 2, 13, 5)], 10, 12), moved(10, 12));
        // Insertion right after the last line of the range.
        assert_eq!(relocate(&[hunk(12, 0, 13, 1)], 10, 12), moved(10, 12));
    }

    #[test]
    fn relocate_detects_overlap_and_inner_insertion() {
        assert_eq!(relocate(&[hunk(12, 1, 12, 1)], 10, 12), Relocation::Changed);
        assert_eq!(relocate(&[hunk(8, 3, 8, 1)], 10, 12), Relocation::Changed);
        // Insertion after line 10 lands inside 10..=12.
        assert_eq!(relocate(&[hunk(10, 0, 11, 1)], 10, 12), Relocation::Changed);
        // ...but after the only line of a one-line range it does not.
        assert_eq!(relocate(&[hunk(10, 0, 11, 1)], 10, 10), moved(10, 10));
    }

    #[test]
    fn disk_check_outcomes() {
        assert_eq!(check_disk(&[], 10, 12), DiskCheck::Consistent);
        // Changes after the range, or a same-size change before it, keep the numbering.
        assert_eq!(
            check_disk(&[hunk(20, 1, 20, 4)], 10, 12),
            DiskCheck::Consistent
        );
        assert_eq!(
            check_disk(&[hunk(2, 1, 2, 1)], 10, 12),
            DiskCheck::Consistent
        );
        assert_eq!(
            check_disk(&[hunk(11, 1, 11, 1)], 10, 12),
            DiskCheck::RangeChanged
        );
        // Three lines added on disk after line 2: disk lines 10..=12 are commit lines 7..=9.
        assert_eq!(
            check_disk(&[hunk(2, 0, 3, 3)], 10, 12),
            DiskCheck::Shifted {
                if_read_from_disk: moved(7, 9)
            }
        );
    }
}
