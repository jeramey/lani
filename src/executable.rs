//! The `lani` executable a long-running process started from, to notice when a newer lani is
//! installed over it (docs/design.md §12.5, §13.5).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The `lani` executable a process started from, to notice when it is replaced on disk, as
/// `cargo install` does.
#[derive(Clone, Debug)]
pub struct Executable {
    path: PathBuf,
    identity: FileIdentity,
}

/// What changes when a file is replaced or rewritten.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
}

impl FileIdentity {
    fn of(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
        })
    }
}

impl Executable {
    /// This process's executable, as it is on disk now. Call it at startup.
    pub fn current() -> std::io::Result<Self> {
        Self::at(&std::env::current_exe()?)
    }

    /// The executable at `path`, as it is on disk now.
    pub fn at(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            identity: FileIdentity::of(path)?,
        })
    }

    /// Whether the file this process started from has been replaced, rewritten or removed.
    #[must_use]
    pub fn replaced(&self) -> bool {
        FileIdentity::of(&self.path).ok().as_ref() != Some(&self.identity)
    }
}
