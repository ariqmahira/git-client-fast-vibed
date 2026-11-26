use std::ops::Range;
use std::path::PathBuf;

use crate::Oid;

/// What to compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffTarget {
    /// index → worktree (unstaged changes).
    IndexToWorkdir { include_untracked: bool },
    /// HEAD → index (staged changes).
    HeadToIndex,
    /// A commit against its first parent (root commits diff against empty).
    Commit(Oid),
    /// Two arbitrary commits.
    Commits { old: Oid, new: Oid },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSpec {
    pub target: DiffTarget,
    /// Restrict to these paths; empty means everything.
    pub paths: Vec<PathBuf>,
    pub context_lines: u32,
    pub detect_renames: bool,
}

impl DiffSpec {
    pub fn new(target: DiffTarget) -> Self {
        DiffSpec {
            target,
            paths: Vec::new(),
            context_lines: 3,
            detect_renames: true,
        }
    }

    pub fn paths(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.paths = paths.into_iter().collect();
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileChange {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    TypeChange,
    Untracked,
    Unreadable,
    Conflicted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineKind {
    Context,
    Addition,
    Deletion,
    /// "\ No newline at end of file" markers.
    NoNewline,
}

/// One diff line. Text lives in [`Diff::text`]; `text` is a byte range into it
/// (without the trailing newline). Flat storage keeps a 100k-line diff to a
/// couple of allocations, which matters for virtualized rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    pub text: Range<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub header: Box<str>,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Range into [`Diff::lines`].
    pub lines: Range<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub change: FileChange,
    pub binary: bool,
    pub additions: u32,
    pub deletions: u32,
    /// Range into [`Diff::hunks`].
    pub hunks: Range<u32>,
}

impl FileDiff {
    /// The path to display (new path, falling back to old for deletions).
    pub fn path(&self) -> Option<&PathBuf> {
        self.new_path.as_ref().or(self.old_path.as_ref())
    }
}

/// An immutable, flattened diff.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff {
    pub files: Vec<FileDiff>,
    pub hunks: Vec<Hunk>,
    pub lines: Vec<DiffLine>,
    /// Concatenated line contents (lossily UTF-8 decoded).
    pub text: String,
}

impl Diff {
    pub fn file_hunks(&self, file: &FileDiff) -> &[Hunk] {
        &self.hunks[file.hunks.start as usize..file.hunks.end as usize]
    }

    pub fn hunk_lines(&self, hunk: &Hunk) -> &[DiffLine] {
        &self.lines[hunk.lines.start as usize..hunk.lines.end as usize]
    }

    pub fn line_text(&self, line: &DiffLine) -> &str {
        &self.text[line.text.start as usize..line.text.end as usize]
    }

    pub fn total_additions(&self) -> u64 {
        self.files.iter().map(|f| f.additions as u64).sum()
    }

    pub fn total_deletions(&self) -> u64 {
        self.files.iter().map(|f| f.deletions as u64).sum()
    }
}
