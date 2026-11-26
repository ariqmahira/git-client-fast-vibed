use std::ops::Range;

use crate::{
    CommitDag, CommitMeta, Diff, DiffSpec, Error, HunkSel, NetworkOp, Oid, RefInfo, RepoInfo,
    Result, StatusOpts, StatusSnapshot,
};

/// One Git engine, or a router over several.
///
/// Every method is blocking and may be slow: call it from a worker thread,
/// never from the UI thread. Implementations must be `Send + Sync`; engines
/// whose handles are not `Sync` (libgit2's `Repository`) keep one handle per
/// worker thread internally.
///
/// Each method returns owned, immutable data so results can be published to
/// the UI as `Arc` snapshots without locking.
///
/// Write operations default to [`Error::Unsupported`] so read-only engines
/// and test doubles needn't implement them. Callers must serialize writes
/// (the repo actor runs them on a single writer thread).
pub trait GitBackend: Send + Sync {
    /// Basic facts about the repository (HEAD, bare, workdir).
    fn repo_info(&self) -> Result<RepoInfo>;

    /// All branches, remote branches and tags, peeled to commits.
    fn refs(&self) -> Result<Vec<RefInfo>>;

    /// Commits reachable from `tips`, newest first, children always before
    /// their parents along the path they were discovered through, sliced to
    /// `range` (indices into that ordering).
    ///
    /// Implementations should make sequential paging (`0..500`, `500..1000`,
    /// ...) cost proportional to the page, not to the offset.
    fn walk_commits(&self, tips: &[Oid], range: Range<usize>) -> Result<Vec<CommitMeta>>;

    /// Metadata for specific commits (the visible rows of the graph), in the
    /// order given.
    fn commit_metas(&self, oids: &[Oid]) -> Result<Vec<CommitMeta>>;

    /// The complete DAG reachable from `tips`. Must be fast enough to run on
    /// open for huge repos when a commit-graph file exists (linux.git:
    /// well under a second); without one it may take seconds, so callers
    /// show a provisional graph and call [`GitBackend::write_commit_graph`].
    fn commit_dag(&self, tips: &[Oid]) -> Result<CommitDag>;

    /// Writes/updates Git's commit-graph file (`git commit-graph write
    /// --reachable --split`). Background maintenance; may take tens of
    /// seconds on huge repos the first time.
    fn write_commit_graph(&self) -> Result<()>;

    /// Working-tree status.
    fn status(&self, opts: &StatusOpts) -> Result<StatusSnapshot>;

    /// A flattened diff.
    fn diff(&self, spec: &DiffSpec) -> Result<Diff>;

    // ---- write path (M4) ----

    /// `git add -- <paths>`: stages content, deletions and conflict resolutions.
    fn stage(&self, _paths: &[std::path::PathBuf]) -> Result<()> {
        Err(Error::Unsupported("stage".into()))
    }

    /// `git restore --staged -- <paths>`.
    fn unstage(&self, _paths: &[std::path::PathBuf]) -> Result<()> {
        Err(Error::Unsupported("unstage".into()))
    }

    /// Stages one hunk of the unstaged diff of `path`.
    fn stage_hunk(&self, _path: &std::path::Path, _hunk: HunkSel) -> Result<()> {
        Err(Error::Unsupported("stage_hunk".into()))
    }

    /// Unstages one hunk of the staged diff of `path`.
    fn unstage_hunk(&self, _path: &std::path::Path, _hunk: HunkSel) -> Result<()> {
        Err(Error::Unsupported("unstage_hunk".into()))
    }

    /// Commits the index (or amends HEAD). Returns the new commit.
    fn commit(&self, _message: &str, _amend: bool) -> Result<Oid> {
        Err(Error::Unsupported("commit".into()))
    }

    /// Creates a local branch at `at`, optionally checking it out.
    fn create_branch(&self, _name: &str, _at: Oid, _checkout: bool) -> Result<()> {
        Err(Error::Unsupported("create_branch".into()))
    }

    /// Checks out a local branch, or creates a tracking branch for a remote
    /// one (`origin/x` → `x`). Refuses to overwrite local changes.
    fn checkout_branch(&self, _name: &str) -> Result<()> {
        Err(Error::Unsupported("checkout_branch".into()))
    }

    /// Deletes a local branch. Without `force`, refuses unmerged branches.
    fn delete_branch(&self, _name: &str, _force: bool) -> Result<()> {
        Err(Error::Unsupported("delete_branch".into()))
    }

    /// `git stash push --include-untracked [-m message]`.
    fn stash_push(&self, _message: Option<&str>) -> Result<()> {
        Err(Error::Unsupported("stash_push".into()))
    }

    /// `git stash pop`.
    fn stash_pop(&self) -> Result<()> {
        Err(Error::Unsupported("stash_pop".into()))
    }

    // ---- network (M6) ----

    /// Runs a network operation, streaming git's progress lines to
    /// `progress`. Returns the command that ran.
    fn network(&self, _op: NetworkOp, _progress: &dyn Fn(&str)) -> Result<String> {
        Err(Error::Unsupported("network".into()))
    }
}
