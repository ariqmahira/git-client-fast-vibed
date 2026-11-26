use std::ops::Range;

use crate::{
    CommitDag, CommitMeta, Diff, DiffSpec, Oid, RefInfo, RepoInfo, Result, StatusOpts,
    StatusSnapshot,
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
/// Write operations (stage, commit, branch, stash) arrive in M4 and network
/// operations (fetch/push via the git CLI) in M6.
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
}
