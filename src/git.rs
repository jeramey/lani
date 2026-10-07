//! Read-only git access through the `git` CLI (docs/design.md §8).
//!
//! Safety rules, applied to every invocation:
//! - `--no-optional-locks`, so `status` and `diff` never rewrite a worktree's index.
//! - `GIT_LITERAL_PATHSPECS=1`, so paths are never interpreted as glob patterns.
//! - Refs supplied by users are only ever passed to `rev-parse` after `--end-of-options`;
//!   every other command receives full SHAs that `rev-parse` produced, plus paths after `--`.
//! - Environment variables that would redirect git to another repository are removed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use crate::error::{Error, Result};
use crate::text::quoted;

/// Environment variables that could make git operate on a different repository or index than
/// the one LANI names explicitly.
const REDIRECTING_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// The oldest git LANI supports: 2.41 added the `%(ahead-behind:…)` format atom used for
/// parent-branch detection; 2.31 added `rev-parse --path-format` (design §8).
pub const MIN_GIT_VERSION: (u32, u32) = (2, 41);

/// The result of checking the `git` on PATH, computed once per process.
static GIT_CHECK: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// A line-range change from a unified diff hunk header `@@ -old_start,old_count +new_start,new_count @@`.
///
/// When `old_count` is 0 the hunk inserts lines *after* old line `old_start`
/// (and likewise for `new_count`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
}

impl Hunk {
    /// The same change seen from the other side: maps new line numbers back to old ones.
    #[must_use]
    pub fn reversed(self) -> Self {
        Self {
            old_start: self.new_start,
            old_count: self.new_count,
            new_start: self.old_start,
            new_count: self.old_count,
        }
    }
}

/// A git worktree and the repository it belongs to.
#[derive(Clone, Debug)]
pub struct Worktree {
    /// Canonical absolute path of the worktree's top-level directory.
    pub toplevel: PathBuf,
    /// Canonical absolute path of the common git dir, shared by all worktrees of the repo.
    pub git_dir: PathBuf,
}

/// The state of a worktree as reported by `git status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeState {
    /// The commit checked out, or `None` on an unborn branch.
    pub head: Option<String>,
    /// The branch checked out (short name), or `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Tracked files with staged or unstaged changes, relative to the top-level directory.
    pub modified: Vec<String>,
}

/// What happened to a file between two commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathChange {
    Unchanged,
    Modified,
    Renamed(String),
    Deleted,
}

/// How far a branch is from a commit, as computed by `%(ahead-behind:<commit>)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchDistance {
    /// Full ref name, e.g. `refs/heads/main`.
    pub refname: String,
    /// Commits on the branch that the commit does not contain.
    pub ahead: u64,
    /// Commits in the commit's history that the branch does not contain.
    pub behind: u64,
}

/// A repository, addressed by its common git dir.
#[derive(Clone, Debug)]
pub struct Git {
    git_dir: PathBuf,
}

impl Git {
    #[must_use]
    pub fn new(git_dir: impl Into<PathBuf>) -> Self {
        Self {
            git_dir: git_dir.into(),
        }
    }

    fn command(&self) -> Command {
        let mut command = git_command();
        command.arg("--git-dir").arg(&self.git_dir);
        command
    }

    /// Resolve a user-supplied revision to a full commit SHA, or `None` if it does not exist.
    pub fn resolve_commit(&self, rev: &str) -> Result<Option<String>> {
        let mut command = self.command();
        command.args(["rev-parse", "--verify", "--quiet", "--end-of-options"]);
        command.arg(format!("{rev}^{{commit}}"));
        let output = run(command, &[0, 1])?;
        if output.status.success() {
            Ok(Some(stdout_line(&output)?))
        } else {
            Ok(None)
        }
    }

    /// The full ref name a user-supplied name refers to (`main` → `refs/heads/main`), or `None`
    /// if it is not a ref (for example a raw SHA, or nothing at all).
    pub fn full_ref_name(&self, name: &str) -> Result<Option<String>> {
        let mut command = self.command();
        command.args([
            "rev-parse",
            "--symbolic-full-name",
            "--verify",
            "--quiet",
            "--end-of-options",
            name,
        ]);
        let output = run(command, &[0, 1])?;
        let full = stdout_line(&output)?;
        Ok((output.status.success() && full.starts_with("refs/")).then_some(full))
    }

    /// The target of a symbolic ref, or `None` if it does not exist or is not symbolic.
    pub fn symbolic_ref(&self, name: &str) -> Result<Option<String>> {
        let mut command = self.command();
        command.args(["symbolic-ref", "-q", name]);
        let output = run(command, &[0, 1])?;
        if output.status.success() {
            Ok(Some(stdout_line(&output)?))
        } else {
            Ok(None)
        }
    }

    /// A configuration value, or `None` if it is not set.
    pub fn config_get(&self, key: &str) -> Result<Option<String>> {
        let mut command = self.command();
        command.args(["config", "--get", key]);
        let output = run(command, &[0, 1])?;
        if output.status.success() {
            Ok(Some(stdout_line(&output)?))
        } else {
            Ok(None)
        }
    }

    /// The best common ancestor of two commits, or `None` if their histories are unrelated.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        let mut command = self.command();
        command.args(["merge-base", a, b]);
        let output = run(command, &[0, 1])?;
        if output.status.success() {
            Ok(Some(stdout_line(&output)?))
        } else {
            Ok(None)
        }
    }

    /// Whether `ancestor` is `descendant` or one of its ancestors.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let mut command = self.command();
        command.args(["merge-base", "--is-ancestor", ancestor, descendant]);
        Ok(run(command, &[0, 1])?.status.success())
    }

    /// The number of commits reachable from `to` but not from `from`.
    pub fn commit_count(&self, from: &str, to: &str) -> Result<u64> {
        let mut command = self.command();
        command.args(["rev-list", "--count", &format!("{from}..{to}")]);
        let output = run(command, &[0])?;
        let text = stdout_line(&output)?;
        text.parse()
            .map_err(|_| Error::Git(format!("unexpected rev-list --count output {text:?}")))
    }

    /// Distances from every local branch to `commit` (requires git ≥ 2.41).
    pub fn branch_distances(&self, commit: &str) -> Result<Vec<BranchDistance>> {
        let mut command = self.command();
        command.args([
            "for-each-ref",
            &format!("--format=%(refname)%00%(ahead-behind:{commit})"),
            "refs/heads/",
        ]);
        let output = run(command, &[0])?;
        parse_branch_distances(&stdout_text(&output)?)
    }

    /// Full names of the refs under the given prefixes, e.g. `refs/heads/`.
    pub fn list_refs(&self, prefixes: &[&str]) -> Result<Vec<String>> {
        let mut command = self.command();
        command.args(["for-each-ref", "--format=%(refname)"]);
        command.args(prefixes);
        let output = run(command, &[0])?;
        Ok(stdout_text(&output)?
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// The content of `path` at `commit`, or `None` if no such path exists there.
    /// Fails if the path names something other than a regular file (a directory or submodule).
    pub fn read_file(&self, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let mut command = self.command();
        command.args(["ls-tree", "-z", "--full-tree", commit, "--", path]);
        let listing = stdout_text(&run(command, &[0])?)?;
        let Some(entry) = listing.split('\0').find(|entry| !entry.is_empty()) else {
            return Ok(None);
        };
        // Format: "<mode> SP <type> SP <object> TAB <path>"
        let (meta, _) = entry
            .split_once('\t')
            .ok_or_else(|| Error::Git(format!("unexpected ls-tree output {entry:?}")))?;
        let fields: Vec<&str> = meta.split(' ').collect();
        let [_mode, kind, object] = fields[..] else {
            return Err(Error::Git(format!("unexpected ls-tree output {entry:?}")));
        };
        if kind != "blob" {
            return Err(Error::Invalid(format!(
                "{} is not a file at commit {commit} (it is a {kind})",
                quoted(path)
            )));
        }
        let mut command = self.command();
        command.args(["cat-file", "blob", object]);
        Ok(Some(run(command, &[0])?.stdout))
    }

    /// What happened to every changed path between two commits, following renames, keyed by
    /// the path at `from`. Paths missing from the map are unchanged. Rename detection needs the
    /// whole tree, so callers should ask once per pair of commits, not once per path.
    pub fn path_changes(&self, from: &str, to: &str) -> Result<HashMap<String, PathChange>> {
        let mut command = self.command();
        command.args([
            "diff",
            "--name-status",
            "-z",
            "-M",
            "--no-ext-diff",
            from,
            to,
        ]);
        let output = run(command, &[0])?;
        parse_path_changes(&stdout_text(&output)?)
    }

    /// The `-U0` hunks between `old_path` at `from` and `new_path` at `to`.
    pub fn diff_hunks(
        &self,
        from: &str,
        to: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<Vec<Hunk>> {
        let mut command = self.command();
        command.args([
            "diff",
            "-U0",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
        ]);
        command.args([from, to, "--", old_path]);
        if new_path != old_path {
            command.arg(new_path);
        }
        let output = run(command, &[0])?;
        parse_hunks(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Find the worktree containing `path` and the repository it belongs to.
pub fn discover(path: &Path) -> Result<Worktree> {
    if !path.is_absolute() {
        return Err(Error::Invalid(format!(
            "{} is not an absolute path",
            quoted(&path.display().to_string())
        )));
    }
    let mut command = git_command();
    command.arg("-C").arg(path).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
        "--show-toplevel",
    ]);
    let output = run(command, &[0, 128])?;
    if !output.status.success() {
        return Err(Error::Invalid(format!(
            "{} is not inside a git worktree: {}",
            quoted(&path.display().to_string()),
            quoted(String::from_utf8_lossy(&output.stderr).trim())
        )));
    }
    let text = stdout_text(&output)?;
    let mut lines = text.lines();
    let (Some(git_dir), Some(toplevel)) = (lines.next(), lines.next()) else {
        return Err(Error::Git(format!("unexpected rev-parse output {text:?}")));
    };
    Ok(Worktree {
        toplevel: std::fs::canonicalize(toplevel)?,
        git_dir: std::fs::canonicalize(git_dir)?,
    })
}

/// The branch checked out in `worktree` as a full ref name, or `None` if HEAD is detached.
pub fn current_branch(worktree: &Path) -> Result<Option<String>> {
    let mut command = git_command();
    command
        .arg("-C")
        .arg(worktree)
        .args(["symbolic-ref", "-q", "HEAD"]);
    let output = run(command, &[0, 1])?;
    if output.status.success() {
        Ok(Some(stdout_line(&output)?))
    } else {
        Ok(None)
    }
}

/// The checked-out commit, branch and modified tracked files of `worktree`.
pub fn worktree_state(worktree: &Path) -> Result<WorktreeState> {
    let mut command = git_command();
    command.arg("-C").arg(worktree).args([
        "status",
        "--porcelain=v2",
        "-z",
        "--branch",
        "--untracked-files=no",
    ]);
    let output = run(command, &[0])?;
    parse_status(&stdout_text(&output)?)
}

/// The `-U0` hunks from `path` at `commit` to the file currently on disk in the worktree.
/// This covers staged and unstaged edits as well as a HEAD that moved after `commit`.
///
/// This uses plumbing `diff-index` rather than porcelain `git diff`: when a file's stat data is
/// stale, `git diff` refreshes and rewrites the worktree's index even under
/// `--no-optional-locks`, while `diff-index` never writes it.
pub fn diff_commit_to_disk(toplevel: &Path, commit: &str, path: &str) -> Result<Vec<Hunk>> {
    let mut command = git_command();
    command.arg("-C").arg(toplevel).args([
        "diff-index",
        "-p",
        "-U0",
        "--no-renames",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        commit,
        "--",
        path,
    ]);
    let output = run(command, &[0])?;
    parse_hunks(&String::from_utf8_lossy(&output.stdout))
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .args(["--no-optional-locks", "-c", "core.quotePath=false"])
        .env("LC_ALL", "C")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(Stdio::null());
    for variable in REDIRECTING_ENV_VARS {
        command.env_remove(variable);
    }
    command
}

/// Run `command`, accepting only the listed exit codes.
fn run(mut command: Command, accepted_exit_codes: &[i32]) -> Result<Output> {
    require_supported_git()?;
    let output = command
        .output()
        .map_err(|error| Error::Git(format!("could not run git: {error}")))?;
    match output.status.code() {
        Some(code) if accepted_exit_codes.contains(&code) => Ok(output),
        _ => {
            let arguments: Vec<String> = command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect();
            // Arguments and git's message can echo paths and refs from the repository.
            Err(Error::Git(format!(
                "git command {} failed ({}): {}",
                quoted(&format!("git {}", arguments.join(" "))),
                output.status,
                quoted(String::from_utf8_lossy(&output.stderr).trim())
            )))
        }
    }
}

/// Fail with an actionable message if the `git` on PATH is missing or older than
/// [`MIN_GIT_VERSION`], instead of letting a newer git feature fail with a raw error later.
fn require_supported_git() -> Result<()> {
    GIT_CHECK
        .get_or_init(check_git_version)
        .clone()
        .map_err(Error::Unavailable)
}

fn check_git_version() -> std::result::Result<(), String> {
    let (min_major, min_minor) = MIN_GIT_VERSION;
    let output = Command::new("git")
        .arg("version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            format!(
                "LANI needs git {min_major}.{min_minor} or newer, but could not run `git`: {error}"
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "`git version` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    evaluate_git_version(&String::from_utf8_lossy(&output.stdout), &git_location())
}

/// Accept `git version` output for a supported git, or explain what is wrong.
fn evaluate_git_version(output: &str, location: &str) -> std::result::Result<(), String> {
    let (min_major, min_minor) = MIN_GIT_VERSION;
    let found = output
        .trim()
        .strip_prefix("git version ")
        .unwrap_or(output.trim());
    match parse_git_version(output) {
        Some(version) if version >= MIN_GIT_VERSION => Ok(()),
        Some(_) => Err(format!(
            "LANI needs git {min_major}.{min_minor} or newer; found git {found} at {location}"
        )),
        None => Err(format!(
            "LANI needs git {min_major}.{min_minor} or newer, but could not read the version of the git at \
             {location} from {output:?}"
        )),
    }
}

/// Major and minor version from `git version` output, e.g. `git version 2.50.1 (Apple Git-155)`.
fn parse_git_version(output: &str) -> Option<(u32, u32)> {
    let number = output
        .trim()
        .strip_prefix("git version ")?
        .split_whitespace()
        .next()?;
    let mut parts = number.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Where the `git` on PATH lives, for messages.
fn git_location() -> String {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("git"))
                .find(|candidate| candidate.is_file())
        })
        .map_or_else(
            || "`git` on PATH".to_string(),
            |path| path.display().to_string(),
        )
}

fn stdout_text(output: &Output) -> Result<String> {
    String::from_utf8(output.stdout.clone())
        .map_err(|_| Error::Git("git printed output that is not UTF-8".to_string()))
}

fn stdout_line(output: &Output) -> Result<String> {
    Ok(stdout_text(output)?.trim_end_matches('\n').to_string())
}

/// Parse the `@@` headers of `-U0` unified diff output.
pub fn parse_hunks(diff: &str) -> Result<Vec<Hunk>> {
    diff.lines()
        .filter(|line| line.starts_with("@@ "))
        .map(parse_hunk_header)
        .collect()
}

fn parse_hunk_header(line: &str) -> Result<Hunk> {
    let invalid = || Error::Git(format!("unexpected diff hunk header {line:?}"));
    // "@@ -old_start[,old_count] +new_start[,new_count] @@ optional context"
    let mut fields = line.split(' ').skip(1);
    let old = fields
        .next()
        .and_then(|f| f.strip_prefix('-'))
        .ok_or_else(invalid)?;
    let new = fields
        .next()
        .and_then(|f| f.strip_prefix('+'))
        .ok_or_else(invalid)?;
    let (old_start, old_count) = parse_range(old).ok_or_else(invalid)?;
    let (new_start, new_count) = parse_range(new).ok_or_else(invalid)?;
    Ok(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

/// Parse `start[,count]`; a missing count means 1.
fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

fn parse_branch_distances(text: &str) -> Result<Vec<BranchDistance>> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let invalid = || Error::Git(format!("unexpected for-each-ref output {line:?}"));
            let (refname, counts) = line.split_once('\0').ok_or_else(invalid)?;
            let (ahead, behind) = counts.split_once(' ').ok_or_else(invalid)?;
            Ok(BranchDistance {
                refname: refname.to_string(),
                ahead: ahead.parse().map_err(|_| invalid())?,
                behind: behind.parse().map_err(|_| invalid())?,
            })
        })
        .collect()
}

/// Find what happened to `path` in `diff --name-status -z -M` output.
fn parse_path_changes(text: &str) -> Result<HashMap<String, PathChange>> {
    let mut changes = HashMap::new();
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    while let Some(status) = fields.next() {
        let invalid = || Error::Git(format!("unexpected name-status output near {status:?}"));
        let kind = status.chars().next().ok_or_else(invalid)?;
        let source = fields.next().ok_or_else(invalid)?;
        // Renames and copies list a source and a destination path.
        let destination = if matches!(kind, 'R' | 'C') {
            Some(fields.next().ok_or_else(invalid)?)
        } else {
            None
        };
        // A copy leaves its source in place, so it says nothing about the source path.
        if kind == 'C' {
            continue;
        }
        let change = match (kind, destination) {
            ('R', Some(destination)) => PathChange::Renamed(destination.to_string()),
            ('D', _) => PathChange::Deleted,
            _ => PathChange::Modified,
        };
        changes.insert(source.to_string(), change);
    }
    Ok(changes)
}

/// Parse `git status --porcelain=v2 -z --branch` output.
fn parse_status(text: &str) -> Result<WorktreeState> {
    let mut state = WorktreeState {
        head: None,
        branch: None,
        modified: Vec::new(),
    };
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        let invalid = || Error::Git(format!("unexpected status output {field:?}"));
        if let Some(header) = field.strip_prefix("# ") {
            if let Some(oid) = header.strip_prefix("branch.oid ") {
                state.head = (oid != "(initial)").then(|| oid.to_string());
            } else if let Some(head) = header.strip_prefix("branch.head ") {
                state.branch = (head != "(detached)").then(|| head.to_string());
            }
            continue;
        }
        // The path is the last of a fixed number of space-separated fields; it may itself
        // contain spaces.
        let path = match field.as_bytes().first() {
            Some(b'1') => field.splitn(9, ' ').nth(8),
            Some(b'2') => {
                // A rename or copy is followed by a separate field holding the original path.
                fields.next().ok_or_else(invalid)?;
                field.splitn(10, ' ').nth(9)
            }
            Some(b'u') => field.splitn(11, ' ').nth(10),
            _ => None,
        };
        state.modified.push(path.ok_or_else(invalid)?.to_string());
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_versions() {
        assert_eq!(parse_git_version("git version 2.54.0\n"), Some((2, 54)));
        assert_eq!(
            parse_git_version("git version 2.50.1 (Apple Git-155)"),
            Some((2, 50))
        );
        assert_eq!(
            parse_git_version("git version 2.45.1.windows.1"),
            Some((2, 45))
        );
        assert_eq!(parse_git_version("hub version 2.14.2"), None);
        assert_eq!(parse_git_version("git version x.y"), None);

        assert!(evaluate_git_version("git version 2.41.0", "/usr/bin/git").is_ok());
        assert!(evaluate_git_version("git version 3.0.0", "/usr/bin/git").is_ok());
        assert_eq!(
            evaluate_git_version("git version 2.39.2\n", "/usr/bin/git").unwrap_err(),
            "LANI needs git 2.41 or newer; found git 2.39.2 at /usr/bin/git"
        );
        assert!(
            evaluate_git_version("garbage", "/usr/bin/git")
                .unwrap_err()
                .contains("could not read the version")
        );
    }

    #[test]
    fn hunk_headers_with_and_without_counts() {
        let diff = "diff --git a/f b/f\n@@ -3,0 +4,3 @@ c\n+x\n@@ -10 +13 @@\n-y\n+z\n";
        assert_eq!(
            parse_hunks(diff).unwrap(),
            vec![
                Hunk {
                    old_start: 3,
                    old_count: 0,
                    new_start: 4,
                    new_count: 3
                },
                Hunk {
                    old_start: 10,
                    old_count: 1,
                    new_start: 13,
                    new_count: 1
                },
            ]
        );
    }

    #[test]
    fn malformed_hunk_header_is_an_error() {
        assert!(parse_hunks("@@ -x +1 @@\n").is_err());
    }

    #[test]
    fn branch_distances() {
        let text = "refs/heads/feat-a\x000 2\nrefs/heads/main\x001 3\n";
        assert_eq!(
            parse_branch_distances(text).unwrap(),
            vec![
                BranchDistance {
                    refname: "refs/heads/feat-a".into(),
                    ahead: 0,
                    behind: 2
                },
                BranchDistance {
                    refname: "refs/heads/main".into(),
                    ahead: 1,
                    behind: 3
                },
            ]
        );
    }

    #[test]
    fn path_changes() {
        let text = "M\0a.rs\0R087\0old.rs\0new.rs\0D\0gone.rs\0C075\0src.rs\0copy.rs\0";
        let changes = parse_path_changes(text).unwrap();
        assert_eq!(
            changes,
            HashMap::from([
                ("a.rs".to_string(), PathChange::Modified),
                ("old.rs".to_string(), PathChange::Renamed("new.rs".into())),
                ("gone.rs".to_string(), PathChange::Deleted),
            ]),
            "copies leave their source unchanged, and unlisted paths are unchanged"
        );
        assert!(parse_path_changes("R100\0only-source\0").is_err());
    }

    #[test]
    fn status_with_branch_renames_and_spaces() {
        let text = "# branch.oid abc123\0# branch.head feat\0\
                    1 .M N... 100644 100644 100644 aaa bbb dir/with space.rs\0\
                    2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0";
        assert_eq!(
            parse_status(text).unwrap(),
            WorktreeState {
                head: Some("abc123".into()),
                branch: Some("feat".into()),
                modified: vec!["dir/with space.rs".into(), "new.rs".into()],
            }
        );
    }

    #[test]
    fn status_detached_and_unborn() {
        let state = parse_status("# branch.oid (initial)\0# branch.head (detached)\0").unwrap();
        assert_eq!(state.head, None);
        assert_eq!(state.branch, None);
    }
}
