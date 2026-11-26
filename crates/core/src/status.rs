use std::path::PathBuf;

/// What happened to a path on one side (index vs HEAD, or worktree vs index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Change {
    #[default]
    Unmodified,
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChange,
    Untracked,
    Ignored,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Path relative to the workdir, using `/` separators as stored by git.
    pub path: PathBuf,
    /// For renames detected between HEAD and index: the original path.
    pub orig_path: Option<PathBuf>,
    /// HEAD → index ("staged").
    pub index: Change,
    /// index → worktree ("unstaged").
    pub worktree: Change,
}

impl StatusEntry {
    pub fn is_staged(&self) -> bool {
        !matches!(
            self.index,
            Change::Unmodified | Change::Untracked | Change::Ignored
        )
    }

    pub fn is_unstaged(&self) -> bool {
        self.worktree != Change::Unmodified
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusOpts {
    pub include_untracked: bool,
    /// List every file inside untracked directories instead of just the dir.
    pub recurse_untracked_dirs: bool,
    pub include_ignored: bool,
    /// Rename detection costs time on big changesets; opt in.
    pub detect_renames: bool,
    /// Restrict to these paths (targeted status after watcher events, §5).
    /// Empty means the whole repository.
    pub pathspec: Vec<PathBuf>,
}

impl Default for StatusOpts {
    fn default() -> Self {
        StatusOpts {
            include_untracked: true,
            recurse_untracked_dirs: true,
            include_ignored: false,
            detect_renames: false,
            pathspec: Vec::new(),
        }
    }
}

/// Immutable result of a status run, sorted by path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusSnapshot {
    pub entries: Vec<StatusEntry>,
}

impl StatusSnapshot {
    pub fn staged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_staged())
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_unstaged())
    }

    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}
