use std::path::PathBuf;

use crate::Oid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
    Stash,
    Other,
}

/// A resolved reference. `target` is the commit it peels to (annotated tags
/// are peeled), so the graph can attach labels to rows directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInfo {
    /// Full name, e.g. `refs/heads/main`.
    pub name: Box<str>,
    /// Display name, e.g. `main`, `origin/main`, `v1.0`.
    pub shorthand: Box<str>,
    pub kind: RefKind,
    pub target: Oid,
    /// True for the branch HEAD points at.
    pub is_head: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// The `.git` directory (or the repo itself when bare).
    pub git_dir: PathBuf,
    pub workdir: Option<PathBuf>,
    pub is_bare: bool,
    /// Commit HEAD resolves to; `None` on an unborn branch.
    pub head: Option<Oid>,
    /// Branch HEAD points at; `None` when detached.
    pub head_branch: Option<Box<str>>,
    pub is_shallow: bool,
}
