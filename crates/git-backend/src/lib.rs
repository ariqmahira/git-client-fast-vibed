//! libgit2 implementation of [`GitBackend`].
//!
//! libgit2's `Repository` is `Send` but not `Sync`, so [`Git2Backend`] keeps
//! one `Repository` per worker thread in a thread-local cache. The backend
//! itself is cheap to share (`Arc<Git2Backend>`) across a worker pool.

mod cli;
mod commit_graph;
mod convert;
mod dag;
mod walk;
mod write;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use fgc_core::{
    Change, CommitDag, CommitMeta, Diff, DiffLine, DiffSpec, DiffTarget, Error, FileChange,
    FileDiff, GitBackend, Hunk, LineKind, Oid, RefInfo, RefKind, RepoInfo, Result, StatusEntry,
    StatusOpts, StatusSnapshot,
};
use git2::{DiffOptions, Repository, StatusOptions};

use convert::{bytes_to_path, map_err, path_to_bytes, to_core, to_git};
use walk::WalkCache;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// libgit2's object cache limit applies to each `Repository` handle, and we
/// keep one handle per worker thread, so the default (256 MB) would let
/// every worker grow its own cache while the user scrolls through random
/// history. The graph engine doesn't use it (it reads the commit-graph), and
/// metadata/diff lookups rarely revisit objects, so a small cap per handle
/// keeps memory flat (plan §1: idle < 150 MB with linux.git open).
pub const DEFAULT_OBJECT_CACHE_BYTES: usize = 8 * 1024 * 1024;

static CACHE_LIMIT: std::sync::Once = std::sync::Once::new();

/// Pack files are memory-mapped in windows; touched pages count as resident
/// memory. libgit2's default mapped limit is 8 GB on 64-bit, so browsing
/// random history in a 2 GB pack (linux.git) would keep growing RSS. Windows
/// beyond this limit are unmapped least-recently-used first.
pub const DEFAULT_MWINDOW_MAPPED_LIMIT: usize = 128 * 1024 * 1024;
pub const DEFAULT_MWINDOW_SIZE: usize = 32 * 1024 * 1024;

/// Overrides the libgit2 object cache limit (process-wide).
pub fn set_object_cache_limit(bytes: usize) {
    // SAFETY: sets a libgit2 global option; libgit2 synchronizes it internally.
    if let Err(e) = unsafe { git2::opts::set_cache_max_size(bytes as _) } {
        tracing::warn!("cannot set libgit2 cache size: {e}");
    }
}

thread_local! {
    /// Per-thread repository handles, keyed by backend id. The `Weak` lets a
    /// thread drop handles whose backend is gone (closed repo tab) on its next
    /// access, so pack file descriptors aren't held forever.
    static REPOS: RefCell<HashMap<u64, (Weak<()>, Repository)>> = RefCell::new(HashMap::new());
}

pub struct Git2Backend {
    id: u64,
    alive: Arc<()>,
    git_dir: PathBuf,
    walks: Mutex<WalkCache>,
}

impl std::fmt::Debug for Git2Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Git2Backend")
            .field("git_dir", &self.git_dir)
            .finish_non_exhaustive()
    }
}

impl Git2Backend {
    /// Opens the repository containing `path` (searching upwards).
    #[tracing::instrument(level = "debug", skip_all, fields(path = %path.as_ref().display()))]
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        CACHE_LIMIT.call_once(|| {
            set_object_cache_limit(DEFAULT_OBJECT_CACHE_BYTES);
            // SAFETY: libgit2 global options, set once before any pack is opened by us.
            unsafe {
                let _ = git2::opts::set_mwindow_size(DEFAULT_MWINDOW_SIZE as _);
                let _ = git2::opts::set_mwindow_mapped_limit(DEFAULT_MWINDOW_MAPPED_LIMIT as _);
            }
        });
        let path = path.as_ref();
        let repo = Repository::discover(path).map_err(|e| {
            if e.code() == git2::ErrorCode::NotFound {
                Error::NotARepo(path.to_path_buf())
            } else {
                map_err(e)
            }
        })?;
        let backend = Git2Backend {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            alive: Arc::new(()),
            git_dir: repo.path().to_path_buf(),
            walks: Mutex::new(WalkCache::default()),
        };
        // Seed the opening thread's cache so the first call doesn't reopen.
        REPOS.with(|r| {
            r.borrow_mut()
                .insert(backend.id, (Arc::downgrade(&backend.alive), repo))
        });
        Ok(backend)
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Runs `f` with this thread's `Repository`, opening it on first use.
    fn with_repo<T>(&self, f: impl FnOnce(&Repository) -> Result<T>) -> Result<T> {
        REPOS.with(|cell| {
            let mut map = cell.borrow_mut();
            if !map.contains_key(&self.id) {
                map.retain(|_, (alive, _)| alive.strong_count() > 0);
                let repo = Repository::open(&self.git_dir).map_err(map_err)?;
                map.insert(self.id, (Arc::downgrade(&self.alive), repo));
            }
            let (_, repo) = map.get(&self.id).expect("inserted above");
            // Release the map borrow is not possible while `repo` is borrowed;
            // `f` must not re-enter `with_repo` (it never does internally).
            f(repo)
        })
    }

    /// Like `with_repo`, with a mutable handle (libgit2's stash API needs one).
    fn with_repo_mut<T>(&self, f: impl FnOnce(&mut Repository) -> Result<T>) -> Result<T> {
        REPOS.with(|cell| {
            let mut map = cell.borrow_mut();
            if !map.contains_key(&self.id) {
                map.retain(|_, (alive, _)| alive.strong_count() > 0);
                let repo = Repository::open(&self.git_dir).map_err(map_err)?;
                map.insert(self.id, (Arc::downgrade(&self.alive), repo));
            }
            let (_, repo) = map.get_mut(&self.id).expect("inserted above");
            f(repo)
        })
    }
}

impl GitBackend for Git2Backend {
    #[tracing::instrument(level = "debug", skip_all)]
    fn repo_info(&self) -> Result<RepoInfo> {
        self.with_repo(|repo| {
            let head = match repo.head() {
                Ok(r) => r.peel_to_commit().ok().map(|c| to_core(c.id())),
                Err(e) if e.code() == git2::ErrorCode::UnbornBranch => None,
                Err(e) if e.code() == git2::ErrorCode::NotFound => None,
                Err(e) => return Err(map_err(e)),
            };
            let head_branch = repo
                .find_reference("HEAD")
                .ok()
                .and_then(|r| r.symbolic_target().ok().flatten().map(str::to_owned))
                .map(|t| t.strip_prefix("refs/heads/").unwrap_or(&t).into());
            Ok(RepoInfo {
                git_dir: repo.path().to_path_buf(),
                workdir: repo.workdir().map(Path::to_path_buf),
                is_bare: repo.is_bare(),
                head,
                head_branch,
                is_shallow: repo.is_shallow(),
                has_commit_graph: dag::has_commit_graph(repo),
            })
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn refs(&self) -> Result<Vec<RefInfo>> {
        self.with_repo(|repo| {
            let head_target = repo
                .find_reference("HEAD")
                .ok()
                .and_then(|r| r.symbolic_target().ok().flatten().map(str::to_owned));
            let mut out = Vec::new();
            for r in repo.references().map_err(map_err)? {
                let r = r.map_err(map_err)?;
                // Skip symbolic refs like refs/remotes/origin/HEAD.
                if r.kind() != Some(git2::ReferenceType::Direct) {
                    continue;
                }
                let Ok(name) = r.name() else { continue };
                // Some tags point at trees/blobs (linux.git's v2.6.11-tree);
                // they can't be drawn on the graph, so skip them.
                let Ok(commit) = r.peel_to_commit() else {
                    continue;
                };
                let kind = if name.starts_with("refs/heads/") {
                    RefKind::LocalBranch
                } else if name.starts_with("refs/remotes/") {
                    RefKind::RemoteBranch
                } else if name.starts_with("refs/tags/") {
                    RefKind::Tag
                } else if name == "refs/stash" {
                    RefKind::Stash
                } else {
                    RefKind::Other
                };
                out.push(RefInfo {
                    name: name.into(),
                    shorthand: r.shorthand().unwrap_or(name).into(),
                    kind,
                    target: to_core(commit.id()),
                    is_head: head_target.as_deref() == Some(name),
                });
            }
            Ok(out)
        })
    }

    #[tracing::instrument(level = "debug", skip_all, fields(tips = tips.len(), start = range.start, end = range.end))]
    fn walk_commits(&self, tips: &[Oid], range: Range<usize>) -> Result<Vec<CommitMeta>> {
        if range.start >= range.end {
            return Ok(Vec::new());
        }
        let state = self.walks.lock().unwrap().get(tips);
        let mut state = state.lock().unwrap();
        self.with_repo(|repo| state.page(repo, tips, range))
    }

    #[tracing::instrument(level = "debug", skip_all, fields(n = oids.len()))]
    fn commit_metas(&self, oids: &[Oid]) -> Result<Vec<CommitMeta>> {
        self.with_repo(|repo| {
            oids.iter()
                .map(|oid| {
                    repo.find_commit(to_git(*oid))
                        .map(|c| walk::commit_meta(&c))
                        .map_err(map_err)
                })
                .collect()
        })
    }

    #[tracing::instrument(level = "debug", skip_all, fields(tips = tips.len()))]
    fn commit_dag(&self, tips: &[Oid]) -> Result<CommitDag> {
        self.with_repo(|repo| {
            let graph = dag::open_graph(repo);
            let dag = dag::build(repo, graph.as_ref(), tips)?;
            tracing::debug!(
                commits = dag.len(),
                from_graph = dag.stats.from_commit_graph,
                parsed = dag.stats.parsed,
                "commit dag"
            );
            Ok(dag)
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn write_commit_graph(&self) -> Result<()> {
        self.with_repo(dag::write_commit_graph)
    }

    #[tracing::instrument(level = "debug", skip_all, fields(paths = opts.pathspec.len()))]
    fn status(&self, opts: &StatusOpts) -> Result<StatusSnapshot> {
        self.with_repo(|repo| {
            if repo.is_bare() {
                return Ok(StatusSnapshot::default());
            }
            let mut so = StatusOptions::new();
            so.include_untracked(opts.include_untracked)
                .recurse_untracked_dirs(opts.recurse_untracked_dirs)
                .include_ignored(opts.include_ignored)
                .include_unmodified(false)
                .renames_head_to_index(opts.detect_renames)
                .renames_index_to_workdir(opts.detect_renames)
                .exclude_submodules(false);
            for p in &opts.pathspec {
                so.pathspec(path_to_bytes(p));
            }
            if !opts.pathspec.is_empty() {
                // Pathspecs from the watcher are literal paths, not globs.
                so.disable_pathspec_match(true);
            }
            let statuses = repo.statuses(Some(&mut so)).map_err(map_err)?;
            let mut entries = Vec::with_capacity(statuses.len());
            for e in statuses.iter() {
                let s = e.status();
                let (index, worktree) = split_status(s);
                if index == Change::Unmodified && worktree == Change::Unmodified {
                    continue;
                }
                // libgit2's entry path is the *old* side for renames; report
                // the current path and keep the original separately.
                let hti = e.head_to_index();
                let itw = e.index_to_workdir();
                let path = itw
                    .as_ref()
                    .and_then(|d| d.new_file().path_bytes())
                    .or_else(|| hti.as_ref().and_then(|d| d.new_file().path_bytes()))
                    .unwrap_or_else(|| e.path_bytes());
                let orig_path = if s.is_index_renamed() {
                    hti.as_ref().and_then(|d| d.old_file().path_bytes())
                } else if s.is_wt_renamed() {
                    itw.as_ref().and_then(|d| d.old_file().path_bytes())
                } else {
                    None
                }
                .map(bytes_to_path);
                entries.push(StatusEntry {
                    path: bytes_to_path(path),
                    orig_path,
                    index,
                    worktree,
                });
            }
            entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
            Ok(StatusSnapshot { entries })
        })
    }

    #[tracing::instrument(level = "debug", skip_all, fields(target = ?spec.target))]
    fn diff(&self, spec: &DiffSpec) -> Result<Diff> {
        self.with_repo(|repo| {
            let mut opts = DiffOptions::new();
            opts.context_lines(spec.context_lines);
            for p in &spec.paths {
                opts.pathspec(path_to_bytes(p));
            }
            if !spec.paths.is_empty() {
                opts.disable_pathspec_match(true);
            }
            let mut diff = match &spec.target {
                DiffTarget::IndexToWorkdir { include_untracked } => {
                    opts.include_untracked(*include_untracked)
                        .recurse_untracked_dirs(*include_untracked)
                        .show_untracked_content(*include_untracked);
                    repo.diff_index_to_workdir(None, Some(&mut opts))
                }
                DiffTarget::HeadToIndex => {
                    let head_tree = match repo.head() {
                        Ok(h) => Some(h.peel_to_tree().map_err(map_err)?),
                        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => None,
                        Err(e) => return Err(map_err(e)),
                    };
                    repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))
                }
                DiffTarget::Commit(oid) => {
                    let commit = repo.find_commit(to_git(*oid)).map_err(map_err)?;
                    let new_tree = commit.tree().map_err(map_err)?;
                    let old_tree = match commit.parent_count() {
                        0 => None,
                        _ => Some(commit.parent(0).and_then(|p| p.tree()).map_err(map_err)?),
                    };
                    repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut opts))
                }
                DiffTarget::Commits { old, new } => {
                    let old_tree = repo.find_commit(to_git(*old)).and_then(|c| c.tree());
                    let new_tree = repo.find_commit(to_git(*new)).and_then(|c| c.tree());
                    let (old_tree, new_tree) =
                        (old_tree.map_err(map_err)?, new_tree.map_err(map_err)?);
                    repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), Some(&mut opts))
                }
            }
            .map_err(map_err)?;

            let is_workdir = matches!(spec.target, DiffTarget::IndexToWorkdir { .. });
            if spec.detect_renames && !is_workdir {
                let mut find = git2::DiffFindOptions::new();
                find.renames(true);
                diff.find_similar(Some(&mut find)).map_err(map_err)?;
            }
            flatten_diff(&diff)
        })
    }

    #[tracing::instrument(level = "debug", skip_all, fields(paths = paths.len()))]
    fn stage(&self, paths: &[PathBuf]) -> Result<()> {
        self.with_repo(|r| write::stage(r, paths))
    }

    #[tracing::instrument(level = "debug", skip_all, fields(paths = paths.len()))]
    fn unstage(&self, paths: &[PathBuf]) -> Result<()> {
        self.with_repo(|r| write::unstage(r, paths))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn stage_hunk(&self, path: &Path, hunk: fgc_core::HunkSel) -> Result<()> {
        self.with_repo(|r| write::stage_hunk(r, path, hunk))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn unstage_hunk(&self, path: &Path, hunk: fgc_core::HunkSel) -> Result<()> {
        self.with_repo(|r| write::unstage_hunk(r, path, hunk))
    }

    #[tracing::instrument(level = "debug", skip_all, fields(amend))]
    fn commit(&self, message: &str, amend: bool) -> Result<Oid> {
        self.with_repo_mut(|r| write::commit(r, message, amend))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn create_branch(&self, name: &str, at: Oid, checkout: bool) -> Result<()> {
        self.with_repo(|r| write::create_branch(r, name, at, checkout))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn checkout_branch(&self, name: &str) -> Result<()> {
        self.with_repo(|r| write::checkout_branch(r, name))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        self.with_repo(|r| write::delete_branch(r, name, force))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn stash_push(&self, message: Option<&str>) -> Result<()> {
        self.with_repo_mut(|r| write::stash_push(r, message))
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn stash_pop(&self) -> Result<()> {
        self.with_repo_mut(write::stash_pop)
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn refresh_index(&self) -> Result<()> {
        self.with_repo(|r| match r.workdir() {
            // Through git itself: it takes the index lock and gives up
            // harmlessly if someone else is writing the index.
            Some(wd) => cli::run_quiet(r.path(), Some(wd), &["update-index", "-q", "--refresh"]),
            None => Ok(()),
        })
    }

    #[tracing::instrument(level = "debug", skip_all, fields(op = ?op))]
    fn network(&self, op: fgc_core::NetworkOp, progress: &dyn Fn(&str)) -> Result<String> {
        self.with_repo(|r| write::network(r, op, progress))
    }
}

fn split_status(s: git2::Status) -> (Change, Change) {
    use git2::Status as S;
    if s.contains(S::CONFLICTED) {
        return (Change::Conflicted, Change::Conflicted);
    }
    let index = if s.contains(S::INDEX_NEW) {
        Change::Added
    } else if s.contains(S::INDEX_MODIFIED) {
        Change::Modified
    } else if s.contains(S::INDEX_DELETED) {
        Change::Deleted
    } else if s.contains(S::INDEX_RENAMED) {
        Change::Renamed
    } else if s.contains(S::INDEX_TYPECHANGE) {
        Change::TypeChange
    } else {
        Change::Unmodified
    };
    let worktree = if s.contains(S::WT_NEW) {
        Change::Untracked
    } else if s.contains(S::WT_MODIFIED) {
        Change::Modified
    } else if s.contains(S::WT_DELETED) {
        Change::Deleted
    } else if s.contains(S::WT_RENAMED) {
        Change::Renamed
    } else if s.contains(S::WT_TYPECHANGE) {
        Change::TypeChange
    } else if s.contains(S::IGNORED) {
        Change::Ignored
    } else {
        Change::Unmodified
    };
    (index, worktree)
}

fn file_change(d: git2::Delta) -> FileChange {
    use git2::Delta as D;
    match d {
        D::Added => FileChange::Added,
        D::Deleted => FileChange::Deleted,
        D::Renamed => FileChange::Renamed,
        D::Copied => FileChange::Copied,
        D::Typechange => FileChange::TypeChange,
        D::Untracked => FileChange::Untracked,
        D::Unreadable => FileChange::Unreadable,
        D::Conflicted => FileChange::Conflicted,
        D::Modified | D::Unmodified | D::Ignored => FileChange::Modified,
    }
}

fn u32_len(n: usize) -> Result<u32> {
    u32::try_from(n).map_err(|_| Error::Unsupported("diff larger than 4 GiB".into()))
}

/// Flattens a libgit2 diff into the core's contiguous representation.
fn flatten_diff(diff: &git2::Diff<'_>) -> Result<Diff> {
    let mut out = Diff::default();
    let n = diff.deltas().len();
    out.files.reserve(n);
    for idx in 0..n {
        let delta = diff.get_delta(idx).expect("index in range");
        let patch = git2::Patch::from_diff(diff, idx).map_err(map_err)?;
        let hunk_start = u32_len(out.hunks.len())?;
        let mut file = FileDiff {
            old_path: delta.old_file().path_bytes().map(bytes_to_path),
            new_path: delta.new_file().path_bytes().map(bytes_to_path),
            change: file_change(delta.status()),
            binary: delta.flags().is_binary(),
            additions: 0,
            deletions: 0,
            hunks: hunk_start..hunk_start,
        };
        if matches!(delta.status(), git2::Delta::Added | git2::Delta::Untracked) {
            file.old_path = None;
        } else if delta.status() == git2::Delta::Deleted {
            file.new_path = None;
        }
        if let Some(patch) = patch {
            file.binary |= patch.delta().flags().is_binary();
            for h in 0..patch.num_hunks() {
                let (hunk, n_lines) = patch.hunk(h).map_err(map_err)?;
                let line_start = u32_len(out.lines.len())?;
                for l in 0..n_lines {
                    let line = patch.line_in_hunk(h, l).map_err(map_err)?;
                    let kind = match line.origin() {
                        '+' => {
                            file.additions += 1;
                            LineKind::Addition
                        }
                        '-' => {
                            file.deletions += 1;
                            LineKind::Deletion
                        }
                        ' ' => LineKind::Context,
                        '=' | '>' | '<' => LineKind::NoNewline,
                        _ => continue,
                    };
                    let mut content = line.content();
                    if let Some(stripped) = content.strip_suffix(b"\n") {
                        content = stripped;
                    }
                    let start = u32_len(out.text.len())?;
                    if kind == LineKind::NoNewline {
                        out.text.push_str("No newline at end of file");
                    } else {
                        match std::str::from_utf8(content) {
                            Ok(s) => out.text.push_str(s),
                            Err(_) => out.text.push_str(&String::from_utf8_lossy(content)),
                        }
                    }
                    let end = u32_len(out.text.len())?;
                    out.lines.push(DiffLine {
                        kind,
                        old_lineno: line.old_lineno(),
                        new_lineno: line.new_lineno(),
                        text: start..end,
                    });
                }
                let header = String::from_utf8_lossy(hunk.header());
                out.hunks.push(Hunk {
                    header: header.trim_end().into(),
                    old_start: hunk.old_start(),
                    old_lines: hunk.old_lines(),
                    new_start: hunk.new_start(),
                    new_lines: hunk.new_lines(),
                    lines: line_start..u32_len(out.lines.len())?,
                });
            }
        }
        file.hunks.end = u32_len(out.hunks.len())?;
        out.files.push(file);
    }
    Ok(out)
}
