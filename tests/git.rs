//! The git reads behind the code views, against real temporary repositories.

mod common;

use common::TestRepo;
use lani::git::{EntryKind, Git, RowKind};

fn git_of(repo: &TestRepo) -> Git {
    Git::new(repo.path.join(".git"))
}

#[test]
fn directories_and_entries_are_read_from_the_commit() {
    let repo = TestRepo::new(&[
        ("README.md", "readme\n"),
        ("src/main.rs", "fn main() {}\n"),
        ("src/a b/c.txt", "spaced\n"),
    ]);
    let readme = repo.path.join("README.md");
    std::fs::set_permissions(&readme, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("README.md", repo.path.join("link")).unwrap();
    let head = repo.commit_all("executable readme and a link");
    let git = git_of(&repo);

    let root = git.list_directory(&head, "").unwrap();
    let names: Vec<_> = root.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(names, ["README.md", "link", "src"]);
    assert_eq!(root[0].kind, EntryKind::File, "executable files are files");
    assert_eq!(root[0].size, Some(7));
    assert_eq!(root[1].kind, EntryKind::Symlink);
    assert_eq!(git.read_blob(&root[1].object).unwrap(), b"README.md");
    assert_eq!(root[2].kind, EntryKind::Directory);

    let src = git.list_directory(&head, "src").unwrap();
    let names: Vec<_> = src.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(names, ["src/a b", "src/main.rs"]);

    let spaced = git.tree_entry(&head, "src/a b/c.txt").unwrap().unwrap();
    assert_eq!(spaced.kind, EntryKind::File);
    assert_eq!(git.read_blob(&spaced.object).unwrap(), b"spaced\n");
    let dir = git.tree_entry(&head, "src").unwrap().unwrap();
    assert_eq!(dir.kind, EntryKind::Directory);
    assert_eq!(git.tree_entry(&head, "src/missing.rs").unwrap(), None);
    assert_eq!(
        git.tree_entry(&head, "src/*").unwrap(),
        None,
        "paths are literal, not patterns"
    );
    assert_eq!(git.list_directory(&head, "nowhere").unwrap(), []);
}

#[test]
fn file_changes_and_rows_between_two_commits() {
    let lines = common::numbered(10);
    let repo = TestRepo::new(&[
        ("keep.txt", &lines),
        ("old.txt", "moved content\nthat stays\nthe same\n"),
        ("gone.txt", "bye\n"),
    ]);
    let from = repo.head();
    repo.write(
        "keep.txt",
        &lines.replace("line 5\n", "line five\nline 5.5\n"),
    );
    repo.git(&["mv", "old.txt", "new.txt"]);
    repo.git(&["rm", "-q", "gone.txt"]);
    repo.write("added.txt", "hello\n");
    let to = repo.commit_all("changes");
    let git = git_of(&repo);

    let changes = git.file_changes(&from, &to).unwrap();
    let summary: Vec<_> = changes
        .iter()
        .map(|change| (change.status, change.path()))
        .collect();
    assert_eq!(
        summary,
        [
            ('A', "added.txt"),
            ('D', "gone.txt"),
            ('M', "keep.txt"),
            ('R', "new.txt")
        ]
    );
    let renamed = &changes[3];
    assert_eq!(renamed.old.as_ref().unwrap().path, "old.txt");

    let keep = git
        .diff_file(&from, &to, Some("keep.txt"), Some("keep.txt"))
        .unwrap();
    assert_eq!(keep.len(), 1);
    let rows: Vec<_> = keep[0]
        .rows
        .iter()
        .map(|row| (row.kind, row.old_line, row.new_line))
        .collect();
    assert_eq!(
        rows,
        [
            (RowKind::Context, Some(2), Some(2)),
            (RowKind::Context, Some(3), Some(3)),
            (RowKind::Context, Some(4), Some(4)),
            (RowKind::Removed, Some(5), None),
            (RowKind::Added, None, Some(5)),
            (RowKind::Added, None, Some(6)),
            (RowKind::Context, Some(6), Some(7)),
            (RowKind::Context, Some(7), Some(8)),
            (RowKind::Context, Some(8), Some(9)),
        ]
    );

    let added = git.diff_file(&from, &to, None, Some("added.txt")).unwrap();
    assert_eq!(added[0].rows[0].kind, RowKind::Added);
    let gone = git.diff_file(&from, &to, Some("gone.txt"), None).unwrap();
    assert_eq!(gone[0].rows[0].kind, RowKind::Removed);
    let moved = git
        .diff_file(&from, &to, Some("old.txt"), Some("new.txt"))
        .unwrap();
    assert!(moved.is_empty(), "a pure rename has no hunks: {moved:?}");
}
