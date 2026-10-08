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

/// Options for every diff whose hunks LANI parses, so that the hunks describe exactly the changed
/// lines whatever the file or the user's configuration:
/// - `--text`: a file git considers binary (`-diff` or `binary` in `.gitattributes`, or a NUL
///   byte near the start) would otherwise produce no hunks at all, which reads as "unchanged".
/// - `--inter-hunk-context=0`: `diff.interHunkContext` in the user's config would otherwise
///   merge nearby hunks, so unchanged lines between them would count as changed.
/// - No colour, external diff tools or textconv filters, which would change the output.
const EXACT_HUNKS: [&str; 5] = [
    "--text",
    "--inter-hunk-context=0",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
];

/// Refs that each worktree has its own copy of (git-worktree(1), "REFS").
const PER_WORKTREE_REF_PREFIXES: [&str; 3] = ["refs/bisect/", "refs/worktree/", "refs/rewritten/"];

/// Set once the `git` on PATH has passed [`check_git_version`]. Only success is remembered: after
/// a failure the next git call checks again, so installing or upgrading git takes effect without
/// restarting a long-running `lani serve` or `lani mcp`.
static GIT_SUPPORTED: OnceLock<()> = OnceLock::new();

/// A line-range change from a unified diff hunk header
/// `@@ -old_start,old_count +new_start,new_count @@`.
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

/// What a tree entry is, from its git mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file, executable or not.
    File,
    Symlink,
    Directory,
    /// A submodule: a commit of another repository.
    Submodule,
}

/// One entry of a tree, as `git ls-tree --long` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// The full path from the repository root.
    pub path: String,
    pub kind: EntryKind,
    /// The blob, tree or commit SHA.
    pub object: String,
    /// The blob's size in bytes; `None` for directories and submodules.
    pub size: Option<u64>,
}

impl TreeEntry {
    /// The last component of the path.
    #[must_use]
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// One side of a changed file in a diff between two commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileVersion {
    pub path: String,
    /// The blob SHA (or commit SHA for a submodule).
    pub object: String,
    pub mode: String,
}

/// A file that differs between two commits, as `git diff --raw` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// git's status letter: `A`dded, `D`eleted, `M`odified, `R`enamed or `T`ype changed.
    pub status: char,
    /// The file before the change; `None` when it was added.
    pub old: Option<FileVersion>,
    /// The file after the change; `None` when it was deleted.
    pub new: Option<FileVersion>,
}

impl FileChange {
    /// The path after the change, or before it for a deleted file.
    #[must_use]
    pub fn path(&self) -> &str {
        self.new
            .as_ref()
            .or(self.old.as_ref())
            .map_or("", |version| version.path.as_str())
    }
}

/// How one row of a unified diff relates the two versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Context,
    Removed,
    Added,
}

/// One row of a unified diff: a line number on each side it exists on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffRow {
    pub kind: RowKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

/// A hunk of a unified diff with its rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub header: Hunk,
    pub rows: Vec<DiffRow>,
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
        let Some(entry) = self.tree_entry(commit, path)? else {
            return Ok(None);
        };
        if entry.kind != EntryKind::File {
            return Err(Error::Invalid(format!(
                "{} is not a file at commit {commit} (it is a {})",
                quoted(path),
                match entry.kind {
                    EntryKind::Directory => "directory",
                    EntryKind::Symlink => "symbolic link",
                    EntryKind::Submodule => "submodule",
                    EntryKind::File => "file",
                }
            )));
        }
        self.read_blob(&entry.object).map(Some)
    }

    /// The entry at `path` in `commit`, or `None` if nothing is there. `path` may name a file or
    /// a directory.
    pub fn tree_entry(&self, commit: &str, path: &str) -> Result<Option<TreeEntry>> {
        let mut command = self.command();
        command.args(["ls-tree", "-z", "--long", "--full-tree", commit, "--", path]);
        let entries = parse_tree_entries(&stdout_text(&run(command, &[0])?)?)?;
        Ok(entries.into_iter().find(|entry| entry.path == path))
    }

    /// The entries of directory `dir` in `commit` (`""` for the root), in git's order.
    pub fn list_directory(&self, commit: &str, dir: &str) -> Result<Vec<TreeEntry>> {
        let mut command = self.command();
        command.args(["ls-tree", "-z", "--long", "--full-tree", commit]);
        if !dir.is_empty() {
            command.arg("--").arg(format!("{dir}/"));
        }
        parse_tree_entries(&stdout_text(&run(command, &[0])?)?)
    }

    /// The sizes of objects in bytes, without reading them, in one `cat-file --batch-check`.
    pub fn object_sizes(&self, objects: &[&str]) -> Result<HashMap<String, u64>> {
        if objects.is_empty() {
            return Ok(HashMap::new());
        }
        let mut command = self.command();
        command.args(["cat-file", "--batch-check"]);
        let output = run_with_input(command, batch_input(objects), &[0])?;
        stdout_text(&output)?
            .lines()
            .map(|line| {
                let invalid = || Error::Git(format!("unexpected cat-file output {line:?}"));
                match line.split(' ').collect::<Vec<_>>()[..] {
                    [object, _kind, size] => {
                        Ok((object.to_string(), size.parse().map_err(|_| invalid())?))
                    }
                    [object, "missing"] => Err(Error::Git(format!("object {object} is missing"))),
                    _ => Err(invalid()),
                }
            })
            .collect()
    }

    /// The contents of blobs, in one `cat-file --batch`.
    pub fn read_blobs(&self, objects: &[&str]) -> Result<HashMap<String, Vec<u8>>> {
        if objects.is_empty() {
            return Ok(HashMap::new());
        }
        let mut command = self.command();
        command.args(["cat-file", "--batch"]);
        let output = run_with_input(command, batch_input(objects), &[0])?;
        parse_batch(&output.stdout)
    }

    /// The content of a blob.
    pub fn read_blob(&self, object: &str) -> Result<Vec<u8>> {
        let mut command = self.command();
        command.args(["cat-file", "blob", object]);
        Ok(run(command, &[0])?.stdout)
    }

    /// Every file that differs between two commits, following renames, in git's order.
    pub fn file_changes(&self, from: &str, to: &str) -> Result<Vec<FileChange>> {
        let mut command = self.command();
        command.args([
            "diff",
            "--raw",
            "-z",
            "-M",
            "--no-abbrev",
            "--no-ext-diff",
            from,
            to,
        ]);
        parse_raw_changes(&stdout_text(&run(command, &[0])?)?)
    }

    /// The unified diff, with 3 lines of context, of one file between two commits. Pass the
    /// paths the file has at `from` and `to`; either is `None` when the file does not exist
    /// there.
    pub fn diff_file(
        &self,
        from: &str,
        to: &str,
        old_path: Option<&str>,
        new_path: Option<&str>,
    ) -> Result<Vec<DiffHunk>> {
        let mut command = self.command();
        command.args(["diff", "-U3", "-M"]);
        command.args(EXACT_HUNKS);
        command.args([from, to, "--"]);
        command.args(old_path);
        if new_path != old_path {
            command.args(new_path);
        }
        let output = run(command, &[0])?;
        parse_diff(&String::from_utf8_lossy(&output.stdout))
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
        command.args(["diff", "-U0", "-M"]);
        command.args(EXACT_HUNKS);
        command.args([from, to, "--", old_path]);
        if new_path != old_path {
            command.arg(new_path);
        }
        let output = run(command, &[0])?;
        parse_hunks(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Whether git resolves `name` through one worktree's own state: its `HEAD` (`HEAD`, `@`), or
/// reflog and upstream syntax (`@{-1}`, `@{u}`, `main@{1}`). LANI resolves names with
/// `--git-dir`, where these mean the main worktree's, whichever worktree the name came from.
///
/// `HEAD` is matched in any case: on a case-insensitive filesystem (the macOS default), `head`
/// opens the common git dir's `HEAD` file, even from a linked worktree. No ref can be named `@`
/// or contain `@{` (git-check-ref-format(1)); a branch named `head` in any case is still
/// reachable as `refs/heads/head`.
#[must_use]
pub fn is_worktree_relative(name: &str) -> bool {
    name.eq_ignore_ascii_case("HEAD") || name == "@" || name.contains("@{")
}

/// Whether `full_name` is a ref each worktree has its own copy of, such as `refs/worktree/x`.
/// With `--git-dir`, git reads the main worktree's copy.
#[must_use]
pub fn is_per_worktree_ref(full_name: &str) -> bool {
    PER_WORKTREE_REF_PREFIXES
        .iter()
        .any(|prefix| full_name.starts_with(prefix))
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

/// The commit checked out in `worktree`, or `None` on an unborn branch. Running in the worktree
/// (`-C`) makes `HEAD` that worktree's own; in the common git dir it is the main worktree's.
pub fn worktree_head(worktree: &Path) -> Result<Option<String>> {
    let mut command = git_command();
    command
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]);
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
    command
        .arg("-C")
        .arg(toplevel)
        .args(["diff-index", "-p", "-U0", "--no-renames"]);
    command.args(EXACT_HUNKS);
    command.args([commit, "--", path]);
    let output = run(command, &[0])?;
    parse_hunks(&String::from_utf8_lossy(&output.stdout))
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        // Run in `/`, never in this process's working directory: commands with `--git-dir` treat
        // the working directory as the work tree, so they fail outright once it is deleted (the
        // directory of the Claude session that started LANI often is) and would read its
        // `.gitattributes`. `-C` paths are absolute, so they are unaffected.
        .current_dir("/")
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
fn run(command: Command, accepted_exit_codes: &[i32]) -> Result<Output> {
    run_with_input(command, Vec::new(), accepted_exit_codes)
}

/// Run `command` with `input` on its stdin (nothing when empty), accepting only the listed exit
/// codes.
fn run_with_input(
    mut command: Command,
    input: Vec<u8>,
    accepted_exit_codes: &[i32],
) -> Result<Output> {
    require_supported_git()?;
    let could_not_run = |error: std::io::Error| Error::Git(format!("could not run git: {error}"));
    let output = if input.is_empty() {
        command.output().map_err(could_not_run)?
    } else {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(could_not_run)?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Git("git's stdin was not captured".to_string()))?;
        // Write from another thread: git answers before it has read all of its input, and once
        // its output pipe is full it waits for us to read, so writing everything first could
        // block both processes. A write error means git exited early; its status says why.
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input);
        });
        let output = child.wait_with_output().map_err(could_not_run)?;
        let _ = writer.join();
        output
    };
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
    check_until_passed(&GIT_SUPPORTED, check_git_version)
}

/// Run `check` on every call until it passes once, and skip it on later calls.
fn check_until_passed(
    passed: &OnceLock<()>,
    check: impl FnOnce() -> std::result::Result<(), String>,
) -> Result<()> {
    if passed.get().is_none() {
        check().map_err(Error::Unavailable)?;
        passed.get_or_init(|| ());
    }
    Ok(())
}

fn check_git_version() -> std::result::Result<(), String> {
    let (min_major, min_minor) = MIN_GIT_VERSION;
    let output = Command::new("git")
        .arg("version")
        .current_dir("/")
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
/// A binary diff has no hunks, which would read as "nothing changed", so it is an error.
/// `--text` (in [`EXACT_HUNKS`]) keeps git from producing one.
pub fn parse_hunks(diff: &str) -> Result<Vec<Hunk>> {
    if diff.lines().any(|line| line.starts_with("Binary files ")) {
        return Err(Error::Git(
            "git reported a binary diff, so changed lines cannot be followed".to_string(),
        ));
    }
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

/// One object name per line, for `cat-file --batch` and `--batch-check`.
fn batch_input(objects: &[&str]) -> Vec<u8> {
    let mut input = objects.join("\n").into_bytes();
    input.push(b'\n');
    input
}

/// Parse `git cat-file --batch` output: for each object a `<sha> <type> <size>` line, then
/// exactly `size` bytes of content and a newline.
fn parse_batch(mut output: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    let truncated = || Error::Git("cat-file output ended early".to_string());
    let mut blobs = HashMap::new();
    while !output.is_empty() {
        let end = output
            .iter()
            .position(|&byte| byte == b'\n')
            .ok_or_else(truncated)?;
        let header = String::from_utf8_lossy(&output[..end]).into_owned();
        output = &output[end + 1..];
        let invalid = || Error::Git(format!("unexpected cat-file output {header:?}"));
        let (object, size) = match header.split(' ').collect::<Vec<_>>()[..] {
            [object, _kind, size] => (
                object.to_string(),
                size.parse::<usize>().map_err(|_| invalid())?,
            ),
            [object, "missing"] => {
                return Err(Error::Git(format!("object {object} is missing")));
            }
            _ => return Err(invalid()),
        };
        let content = output.get(..size).ok_or_else(truncated)?;
        blobs.insert(object, content.to_vec());
        output = output.get(size + 1..).ok_or_else(truncated)?;
    }
    Ok(blobs)
}

/// Parse `git ls-tree -z --long` output: `<mode> SP <type> SP <object> SP+ <size> TAB <path>`.
fn parse_tree_entries(text: &str) -> Result<Vec<TreeEntry>> {
    text.split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let invalid = || Error::Git(format!("unexpected ls-tree output {entry:?}"));
            let (meta, path) = entry.split_once('\t').ok_or_else(invalid)?;
            let fields: Vec<&str> = meta.split_whitespace().collect();
            let [mode, _kind, object, size] = fields[..] else {
                return Err(invalid());
            };
            let kind = match mode {
                "040000" => EntryKind::Directory,
                "120000" => EntryKind::Symlink,
                "160000" => EntryKind::Submodule,
                _ => EntryKind::File,
            };
            Ok(TreeEntry {
                path: path.to_string(),
                kind,
                object: object.to_string(),
                size: size.parse().ok(),
            })
        })
        .collect()
}

/// Parse `git diff --raw -z --no-abbrev` output. Each record is
/// `:<old mode> <new mode> <old sha> <new sha> <status>\0<path>\0`, with a second path for
/// renames and copies. An all-zero SHA marks a side that does not exist.
fn parse_raw_changes(text: &str) -> Result<Vec<FileChange>> {
    let mut changes = Vec::new();
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    while let Some(meta) = fields.next() {
        let invalid = || Error::Git(format!("unexpected raw diff output near {meta:?}"));
        let parts: Vec<&str> = meta
            .strip_prefix(':')
            .ok_or_else(invalid)?
            .split(' ')
            .collect();
        let [old_mode, new_mode, old_object, new_object, status] = parts[..] else {
            return Err(invalid());
        };
        let letter = status.chars().next().ok_or_else(invalid)?;
        let source = fields.next().ok_or_else(invalid)?;
        let destination = if matches!(letter, 'R' | 'C') {
            fields.next().ok_or_else(invalid)?
        } else {
            source
        };
        let version = |path: &str, object: &str, mode: &str| {
            (!object.bytes().all(|byte| byte == b'0')).then(|| FileVersion {
                path: path.to_string(),
                object: object.to_string(),
                mode: mode.to_string(),
            })
        };
        changes.push(FileChange {
            status: letter,
            old: version(source, old_object, old_mode),
            new: version(destination, new_object, new_mode),
        });
    }
    Ok(changes)
}

/// Parse unified diff output into hunks with their rows. Only `@@` headers and the first
/// character of each hunk line are read; the text of lines comes from the blobs.
pub fn parse_diff(diff: &str) -> Result<Vec<DiffHunk>> {
    if diff.lines().any(|line| line.starts_with("Binary files ")) {
        return Err(Error::Git(
            "git reported a binary diff instead of lines".to_string(),
        ));
    }
    let mut hunks: Vec<DiffHunk> = Vec::new();
    // Lines still expected in the current hunk on each side; file headers between hunks (as in
    // a type change, which git shows as a deletion and an addition) are skipped.
    let (mut old_left, mut new_left) = (0_u32, 0_u32);
    let (mut old_line, mut new_line) = (0_u32, 0_u32);
    for line in diff.lines() {
        if line.starts_with("@@ ") {
            let header = parse_hunk_header(line)?;
            (old_left, new_left) = (header.old_count, header.new_count);
            (old_line, new_line) = (header.old_start, header.new_start);
            hunks.push(DiffHunk {
                header,
                rows: Vec::new(),
            });
            continue;
        }
        if old_left == 0 && new_left == 0 {
            continue;
        }
        let Some(hunk) = hunks.last_mut() else {
            continue;
        };
        let row = match line.as_bytes().first() {
            Some(b' ') if old_left > 0 && new_left > 0 => {
                (old_left, new_left) = (old_left - 1, new_left - 1);
                (old_line, new_line) = (old_line + 1, new_line + 1);
                DiffRow {
                    kind: RowKind::Context,
                    old_line: Some(old_line - 1),
                    new_line: Some(new_line - 1),
                }
            }
            Some(b'-') if old_left > 0 => {
                old_left -= 1;
                old_line += 1;
                DiffRow {
                    kind: RowKind::Removed,
                    old_line: Some(old_line - 1),
                    new_line: None,
                }
            }
            Some(b'+') if new_left > 0 => {
                new_left -= 1;
                new_line += 1;
                DiffRow {
                    kind: RowKind::Added,
                    old_line: None,
                    new_line: Some(new_line - 1),
                }
            }
            // "\ No newline at end of file"
            Some(b'\\') => continue,
            _ => {
                return Err(Error::Git(format!(
                    "unexpected line in a diff hunk: {:?}",
                    line.chars().take(80).collect::<String>()
                )));
            }
        };
        hunk.rows.push(row);
    }
    Ok(hunks)
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
    fn only_a_passing_git_check_is_remembered() {
        let passed = OnceLock::new();
        let checks = std::cell::Cell::new(0);
        let failing = || {
            checks.set(checks.get() + 1);
            Err("LANI needs git 2.41 or newer".to_string())
        };
        assert!(matches!(
            check_until_passed(&passed, failing),
            Err(Error::Unavailable(message)) if message == "LANI needs git 2.41 or newer"
        ));
        assert!(check_until_passed(&passed, failing).is_err());
        assert_eq!(
            checks.get(),
            2,
            "a failed check runs again on the next call"
        );

        let passing = || {
            checks.set(checks.get() + 1);
            Ok(())
        };
        check_until_passed(&passed, passing).unwrap();
        check_until_passed(&passed, failing).unwrap();
        assert_eq!(checks.get(), 3, "a passed check does not run again");
    }

    #[test]
    fn batch_output_is_split_by_sizes_not_lines() {
        let output = b"aaa blob 6\nline\n\n\nbbb blob 0\n\nccc blob 3\nx\0y\n";
        let blobs = parse_batch(output).unwrap();
        assert_eq!(blobs["aaa"], b"line\n\n");
        assert_eq!(blobs["bbb"], b"");
        assert_eq!(blobs["ccc"], b"x\0y");
        assert!(parse_batch(b"aaa blob 10\nshort\n").is_err());
        assert!(parse_batch(b"aaa missing\n").is_err());
    }

    #[test]
    fn tree_entries_with_kinds_and_sizes() {
        let listing = "100644 blob aaa     1234\tsrc/main.rs\0\
                       100755 blob bbb       10\trun me.sh\0\
                       120000 blob ccc        7\tlink\0\
                       040000 tree ddd        -\tsrc\0\
                       160000 commit eee       -\tvendor/lib\0";
        let entries = parse_tree_entries(listing).unwrap();
        let kinds: Vec<_> = entries.iter().map(|entry| entry.kind).collect();
        assert_eq!(
            kinds,
            [
                EntryKind::File,
                EntryKind::File,
                EntryKind::Symlink,
                EntryKind::Directory,
                EntryKind::Submodule
            ]
        );
        assert_eq!(entries[0].size, Some(1234));
        assert_eq!(entries[1].path, "run me.sh");
        assert_eq!(entries[3].size, None);
        assert_eq!(entries[4].name(), "lib");
        assert!(parse_tree_entries("garbage\0").is_err());
    }

    #[test]
    fn raw_changes_with_renames_additions_and_deletions() {
        let zero = "0".repeat(40);
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let raw = format!(
            ":100644 100644 {a} {b} M\0src/lib.rs\0\
             :000000 100644 {zero} {b} A\0new file.txt\0\
             :100644 000000 {a} {zero} D\0gone.rs\0\
             :100644 100644 {a} {b} R087\0old/name.rs\0new/name.rs\0"
        );
        let changes = parse_raw_changes(&raw).unwrap();
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0].status, 'M');
        assert_eq!(changes[0].old.as_ref().unwrap().object, a);
        assert_eq!(changes[1].old, None);
        assert_eq!(changes[1].path(), "new file.txt");
        assert_eq!(changes[2].new, None);
        assert_eq!(changes[2].path(), "gone.rs");
        assert_eq!(changes[3].status, 'R');
        assert_eq!(changes[3].old.as_ref().unwrap().path, "old/name.rs");
        assert_eq!(changes[3].path(), "new/name.rs");
    }

    #[test]
    fn diff_rows_follow_both_sides() {
        let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                    @@ -1,4 +1,4 @@ fn x\n a\n-b\n+B\n+B2\n c\n-d\n\
                    @@ -10 +10,0 @@\n-z\n\\ No newline at end of file\n";
        let hunks = parse_diff(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        let rows: Vec<_> = hunks[0]
            .rows
            .iter()
            .map(|row| (row.kind, row.old_line, row.new_line))
            .collect();
        assert_eq!(
            rows,
            [
                (RowKind::Context, Some(1), Some(1)),
                (RowKind::Removed, Some(2), None),
                (RowKind::Added, None, Some(2)),
                (RowKind::Added, None, Some(3)),
                (RowKind::Context, Some(3), Some(4)),
                (RowKind::Removed, Some(4), None),
            ]
        );
        assert_eq!(hunks[1].rows.len(), 1);
        assert_eq!(hunks[1].rows[0].old_line, Some(10));
    }

    #[test]
    fn diff_rows_skip_file_headers_between_sections() {
        // A type change: git shows a deletion and an addition of the same path.
        let diff = "diff --git a/x b/x\ndeleted file mode 100644\n--- a/x\n+++ /dev/null\n\
                    @@ -1 +0,0 @@\n-old\n\
                    diff --git a/x b/x\nnew file mode 120000\n--- /dev/null\n+++ b/x\n\
                    @@ -0,0 +1 @@\n+target\n";
        let hunks = parse_diff(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].rows[0].kind, RowKind::Removed);
        assert_eq!(hunks[1].rows[0].kind, RowKind::Added);
        assert_eq!(hunks[1].rows[0].new_line, Some(1));
        assert!(parse_diff("Binary files a/x and b/x differ\n").is_err());
        assert!(parse_diff("@@ -1 +1 @@\n?what\n").is_err());
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
    fn binary_diffs_are_an_error_not_an_empty_diff() {
        let diff = "diff --git a/f b/f\nBinary files a/f and b/f differ\n";
        assert!(parse_hunks(diff).is_err());
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
