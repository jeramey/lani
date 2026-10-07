//! Shared helpers for integration tests: throwaway git repositories and LANI homes.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A git repository in a temporary directory, with a `main` branch.
pub struct TestRepo {
    _dir: TempDir,
    pub path: PathBuf,
}

impl TestRepo {
    /// A repository whose `main` branch has one commit containing `files`.
    pub fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        // Canonicalize so paths match what git reports (macOS /var → /private/var).
        let path = std::fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let repo = Self { _dir: dir, path };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        for (file, content) in files {
            repo.write(file, content);
        }
        repo.commit_all("initial");
        repo
    }

    /// Run git in the repository and return its trimmed stdout; panics on failure.
    pub fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf-8")
            .trim()
            .to_string()
    }

    pub fn write(&self, file: &str, content: &str) {
        let path = self.path.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, content).expect("write file");
    }

    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn switch_new(&self, branch: &str) {
        self.git(&["switch", "-q", "-c", branch]);
    }

    pub fn switch(&self, branch: &str) {
        self.git(&["switch", "-q", branch]);
    }

    pub fn index_bytes(&self) -> Vec<u8> {
        std::fs::read(self.path.join(".git/index")).expect("read index")
    }
}

/// A private LANI data directory.
pub struct TestHome {
    _dir: TempDir,
    pub path: PathBuf,
}

impl TestHome {
    /// A data directory with mode 0700, as LANI requires.
    pub fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = std::fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make temp dir private");
        Self { _dir: dir, path }
    }

    pub fn db_path(&self) -> PathBuf {
        self.path.join("lani.db")
    }
}

/// Lines `1`..=`count` of the form "line N", newline-terminated.
pub fn numbered(count: u32) -> String {
    (1..=count).fold(String::new(), |mut text, n| {
        text.push_str("line ");
        text.push_str(&n.to_string());
        text.push('\n');
        text
    })
}

pub fn path_of(repo: &TestRepo) -> &Path {
    &repo.path
}
