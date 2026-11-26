//! `RepoActor`: one per open repository (plan §5).
//!
//! The UI sends [`Command`]s and receives [`Event`]s; it never calls Git.
//! The actor owns the backend and the graph engine and runs jobs on a small
//! worker pool, highest [`Priority`] first. Commands whose results supersede
//! each other (e.g. "show the diff of commit X" while the user arrows
//! through commits) carry a generation id: stale jobs are skipped if they
//! haven't started and their results dropped if they have.
//!
//! Write operations run in order on a single writer thread (index writes
//! must never race), network operations on their own thread (they can take
//! minutes and must not hold up reads), and a file-system watcher turns
//! working-tree edits into targeted status updates and ref changes into
//! graph reloads.
//!
//! UI-agnostic on purpose: the GPUI shell uses it today, and a fallback
//! shell (plan §4) would use the same crate.

mod ops;
mod watch;

pub use ops::{WriteOp, network_label};
pub use watch::{FsChanges, RepoWatcher, classify};

use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use fgc_core::{
    CommitMeta, Diff, DiffSpec, DiffTarget, GitBackend, NetworkOp, Oid, RefInfo, RefKind, RepoInfo,
    Result, StatusOpts, StatusSnapshot,
};
use fgc_git_backend::Git2Backend;
pub use fgc_graph::{EngineOptions, GraphSnapshot};
use fgc_graph::{GraphEngine, GraphEvent};

/// Plan §5: Interactive > Visible > Background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Background = 0,
    Visible = 1,
    Interactive = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Reload refs and the graph (on open, and when refs change).
    Refresh,
    /// Metadata for commits about to be drawn.
    Metas(Vec<Oid>),
    /// Diff of a commit against its first parent. Supersedes earlier ones.
    CommitDiff(Oid),
    /// Working-tree status.
    Status,
    /// Staged + unstaged diff for the working tree, optionally one path.
    /// Supersedes earlier ones.
    WorkdirDiff(Option<PathBuf>),
    /// Status for specific paths, merged into the last full snapshot
    /// (what the watcher sends after a save).
    StatusPaths(Vec<PathBuf>),
    /// A write operation, run in submission order on the writer thread.
    Write(WriteOp),
    /// A network operation, run on the network thread.
    Network(NetworkOp),
}

#[derive(Debug, Clone)]
pub enum Event {
    RepoInfo(Arc<RepoInfo>),
    Refs(Arc<Vec<RefInfo>>),
    Graph(Arc<GraphSnapshot>),
    Metas(Vec<CommitMeta>),
    CommitDiff {
        oid: Oid,
        diff: Arc<Diff>,
    },
    Status(Arc<StatusSnapshot>),
    WorkdirDiff {
        path: Option<PathBuf>,
        staged: Arc<Diff>,
        unstaged: Arc<Diff>,
    },
    /// Every job reports its duration (for the debug overlay).
    Timing {
        op: &'static str,
        duration: Duration,
        detail: String,
    },
    Error {
        op: &'static str,
        message: String,
    },
    /// A write or network operation started (`id` is what `send` returned).
    OpStarted {
        id: u64,
        label: String,
        command: String,
    },
    /// A progress line from a network operation (git's own wording).
    OpProgress {
        id: u64,
        line: String,
    },
    /// A write or network operation finished. `Ok` carries the command that
    /// ran; `Err` the reason it failed.
    OpFinished {
        id: u64,
        label: String,
        command: String,
        result: std::result::Result<(), String>,
    },
}

#[derive(Debug, Clone)]
pub struct ActorOptions {
    pub workers: usize,
    pub graph: EngineOptions,
    /// Watch the working tree and `.git` for changes.
    pub watch: bool,
}

impl Default for ActorOptions {
    fn default() -> Self {
        ActorOptions {
            workers: 3,
            graph: EngineOptions::default(),
            watch: true,
        }
    }
}

type Sink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Slot {
    CommitDiff,
    WorkdirDiff,
}

struct Job {
    priority: Priority,
    seq: u64,
    cmd: Command,
    /// For superseding commands: the slot generation at submit time.
    generation: Option<(Slot, u64)>,
}

impl PartialEq for Job {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}
impl Eq for Job {}
impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Job {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Highest priority first, then FIFO.
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

struct Shared {
    backend: Arc<dyn GitBackend>,
    sink: Sink,
    queue: Mutex<BinaryHeap<Job>>,
    wake: Condvar,
    seq: AtomicU64,
    commit_diff_gen: AtomicU64,
    workdir_diff_gen: AtomicU64,
    shutdown: AtomicBool,
    engine: GraphEngine,
    /// Last full status, so targeted updates can be merged into it.
    status: Mutex<Option<StatusSnapshot>>,
    writer: Mutex<Option<mpsc::Sender<(u64, WriteOp)>>>,
    network: Mutex<Option<mpsc::Sender<(u64, NetworkOp)>>>,
}

impl Shared {
    /// Routes a command; returns its id.
    fn submit(&self, cmd: Command) -> u64 {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let (priority, slot) = match cmd {
            Command::Write(op) => {
                if let Some(tx) = &*self.writer.lock().unwrap() {
                    let _ = tx.send((id, op));
                }
                return id;
            }
            Command::Network(op) => {
                if let Some(tx) = &*self.network.lock().unwrap() {
                    let _ = tx.send((id, op));
                }
                return id;
            }
            Command::Refresh => (Priority::Interactive, None),
            Command::CommitDiff(_) => (Priority::Interactive, Some(Slot::CommitDiff)),
            Command::WorkdirDiff(_) => (Priority::Interactive, Some(Slot::WorkdirDiff)),
            Command::StatusPaths(_) => (Priority::Interactive, None),
            Command::Metas(_) => (Priority::Visible, None),
            Command::Status => (Priority::Background, None),
        };
        let generation = slot.map(|s| (s, self.slot_gen(s).fetch_add(1, Ordering::SeqCst) + 1));
        self.queue.lock().unwrap().push(Job {
            priority,
            seq: id,
            cmd,
            generation,
        });
        self.wake.notify_one();
        id
    }

    fn slot_gen(&self, slot: Slot) -> &AtomicU64 {
        match slot {
            Slot::CommitDiff => &self.commit_diff_gen,
            Slot::WorkdirDiff => &self.workdir_diff_gen,
        }
    }

    fn is_current(&self, generation: Option<(Slot, u64)>) -> bool {
        generation.is_none_or(|(slot, g)| self.slot_gen(slot).load(Ordering::SeqCst) == g)
    }

    fn timed<T>(
        &self,
        op: &'static str,
        f: impl FnOnce() -> Result<T>,
        detail: impl FnOnce(&T) -> String,
    ) -> Option<T> {
        let start = Instant::now();
        match f() {
            Ok(v) => {
                (self.sink)(Event::Timing {
                    op,
                    duration: start.elapsed(),
                    detail: detail(&v),
                });
                Some(v)
            }
            Err(e) => {
                (self.sink)(Event::Error {
                    op,
                    message: e.to_string(),
                });
                None
            }
        }
    }
}

/// Handle to a running actor. Dropping it stops the workers (after their
/// current job) and cancels any graph load.
pub struct RepoActor {
    shared: Arc<Shared>,
    workers: Vec<std::thread::JoinHandle<()>>,
    _watcher: Option<RepoWatcher>,
}

impl RepoActor {
    /// Opens the repository at `path` and starts the workers. Sends
    /// [`Command::Refresh`] and [`Command::Status`] immediately.
    pub fn open(
        path: impl AsRef<Path>,
        opts: ActorOptions,
        sink: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<RepoActor> {
        let backend: Arc<dyn GitBackend> = Arc::new(Git2Backend::open(path)?);
        Ok(Self::with_backend(backend, opts, sink))
    }

    pub fn with_backend(
        backend: Arc<dyn GitBackend>,
        opts: ActorOptions,
        sink: impl Fn(Event) + Send + Sync + 'static,
    ) -> RepoActor {
        let shared = Arc::new(Shared {
            engine: GraphEngine::new(backend.clone(), opts.graph.clone()),
            backend,
            sink: Arc::new(sink),
            queue: Mutex::new(BinaryHeap::new()),
            wake: Condvar::new(),
            seq: AtomicU64::new(0),
            commit_diff_gen: AtomicU64::new(0),
            workdir_diff_gen: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            status: Mutex::new(None),
            writer: Mutex::new(None),
            network: Mutex::new(None),
        });
        let mut workers: Vec<_> = (0..opts.workers.max(1))
            .map(|i| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("fgc-worker-{i}"))
                    .spawn(move || worker(shared))
                    .expect("spawn worker")
            })
            .collect();

        let (wtx, wrx) = mpsc::channel::<(u64, WriteOp)>();
        *shared.writer.lock().unwrap() = Some(wtx);
        let s = shared.clone();
        workers.push(
            std::thread::Builder::new()
                .name("fgc-writer".into())
                .spawn(move || writer(s, wrx))
                .expect("spawn writer"),
        );
        let (ntx, nrx) = mpsc::channel::<(u64, NetworkOp)>();
        *shared.network.lock().unwrap() = Some(ntx);
        let s = shared.clone();
        workers.push(
            std::thread::Builder::new()
                .name("fgc-network".into())
                .spawn(move || network(s, nrx))
                .expect("spawn network"),
        );

        let watcher = if opts.watch {
            start_watcher(&shared)
        } else {
            None
        };
        let actor = RepoActor {
            shared,
            workers,
            _watcher: watcher,
        };
        actor.send(Command::Refresh);
        actor.send(Command::Status);
        actor
    }

    /// Queues a command and returns its id (matched by `OpStarted` /
    /// `OpFinished` for write and network operations). Never blocks on Git;
    /// safe to call from the UI thread.
    pub fn send(&self, cmd: Command) -> u64 {
        self.shared.submit(cmd)
    }

    pub fn backend(&self) -> &Arc<dyn GitBackend> {
        &self.shared.backend
    }
}

impl Drop for RepoActor {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.engine.cancel();
        // Closing the channels ends the writer and network threads after
        // their current operation.
        self.shared.writer.lock().unwrap().take();
        self.shared.network.lock().unwrap().take();
        self.shared.wake.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn worker(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut q = shared.queue.lock().unwrap();
            loop {
                if shared.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(job) = q.pop() {
                    break job;
                }
                q = shared.wake.wait(q).unwrap();
            }
        };
        if !shared.is_current(job.generation) {
            continue; // superseded before it started
        }
        run(&shared, job);
    }
}

fn start_watcher(shared: &Arc<Shared>) -> Option<RepoWatcher> {
    let info = shared.backend.repo_info().ok()?;
    let weak = Arc::downgrade(shared);
    let result = RepoWatcher::spawn(vec![info.git_dir.clone()], info.workdir.clone(), move |c| {
        let Some(shared) = weak.upgrade() else { return };
        if c.refs {
            shared.submit(Command::Refresh);
        }
        if c.index || c.refs || c.paths.len() > 256 {
            shared.submit(Command::Status);
        } else if !c.paths.is_empty() {
            shared.submit(Command::StatusPaths(c.paths.into_iter().collect()));
        }
    });
    match result {
        Ok(w) => Some(w),
        Err(e) => {
            // E.g. inotify watch limit on a huge tree: keep working without
            // live updates rather than failing to open.
            (shared.sink)(Event::Error {
                op: "watch",
                message: format!("live updates unavailable: {e}"),
            });
            None
        }
    }
}

fn run_write(be: &dyn GitBackend, op: &WriteOp) -> Result<()> {
    match op {
        WriteOp::Stage(p) => be.stage(p),
        WriteOp::Unstage(p) => be.unstage(p),
        WriteOp::StageHunk { path, hunk } => be.stage_hunk(path, *hunk),
        WriteOp::UnstageHunk { path, hunk } => be.unstage_hunk(path, *hunk),
        WriteOp::Commit { message, amend } => be.commit(message, *amend).map(|_| ()),
        WriteOp::CreateBranch { name, at, checkout } => be.create_branch(name, *at, *checkout),
        WriteOp::Checkout(name) => be.checkout_branch(name),
        WriteOp::DeleteBranch { name, force } => be.delete_branch(name, *force),
        WriteOp::StashPush(m) => be.stash_push(m.as_deref()),
        WriteOp::StashPop => be.stash_pop(),
    }
}

fn writer(shared: Arc<Shared>, rx: mpsc::Receiver<(u64, WriteOp)>) {
    while let Ok((id, op)) = rx.recv() {
        let label = op.label();
        let command = op.git_command();
        (shared.sink)(Event::OpStarted {
            id,
            label: label.clone(),
            command: command.clone(),
        });
        let start = Instant::now();
        let result = run_write(&*shared.backend, &op).map_err(|e| e.to_string());
        (shared.sink)(Event::Timing {
            op: "write",
            duration: start.elapsed(),
            detail: label.clone(),
        });
        // Fresh state right away (the watcher would notice too, a little later).
        shared.submit(Command::Status);
        if op.changes_refs() && result.is_ok() {
            shared.submit(Command::Refresh);
        }
        (shared.sink)(Event::OpFinished {
            id,
            label,
            command,
            result,
        });
    }
}

fn network(shared: Arc<Shared>, rx: mpsc::Receiver<(u64, NetworkOp)>) {
    while let Ok((id, op)) = rx.recv() {
        let label = network_label(op).to_owned();
        (shared.sink)(Event::OpStarted {
            id,
            label: label.clone(),
            command: String::new(),
        });
        let sink = shared.sink.clone();
        let progress = move |line: &str| {
            sink(Event::OpProgress {
                id,
                line: line.to_owned(),
            })
        };
        let result = shared.backend.network(op, &progress);
        shared.submit(Command::Refresh);
        shared.submit(Command::Status);
        let (command, result) = match result {
            Ok(cmd) => (cmd, Ok(())),
            Err(e) => (String::new(), Err(e.to_string())),
        };
        (shared.sink)(Event::OpFinished {
            id,
            label,
            command,
            result,
        });
    }
}

/// Merges a targeted status (for `paths`) into the last full snapshot.
fn merge_status(base: &StatusSnapshot, paths: &[PathBuf], fresh: StatusSnapshot) -> StatusSnapshot {
    let covered = |p: &Path| paths.iter().any(|q| p == q || p.starts_with(q));
    let mut entries: Vec<_> = base
        .entries
        .iter()
        .filter(|e| !covered(&e.path))
        .cloned()
        .collect();
    entries.extend(fresh.entries);
    entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    StatusSnapshot { entries }
}

/// A graph load allocates and frees ~2x the final topology (the DAG, sort
/// buffers, the layout's expectation table). glibc keeps freed memory in its
/// arenas, so return it to the OS once the load is done.
fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free memory held by the allocator.
    unsafe {
        libc::malloc_trim(0);
    }
}

fn graph_tips(refs: &[RefInfo], info: &RepoInfo) -> Vec<Oid> {
    let mut tips: Vec<Oid> = refs
        .iter()
        .filter(|r| matches!(r.kind, RefKind::LocalBranch | RefKind::RemoteBranch))
        .map(|r| r.target)
        .collect();
    tips.extend(info.head);
    tips.sort_unstable();
    tips.dedup();
    tips
}

fn run(shared: &Arc<Shared>, job: Job) {
    let be = &shared.backend;
    let sink = &shared.sink;
    match job.cmd {
        Command::Refresh => {
            let Some(info) = shared.timed("repo_info", || be.repo_info(), |_| String::new()) else {
                return;
            };
            let Some(refs) = shared.timed("refs", || be.refs(), |r| format!("{} refs", r.len()))
            else {
                return;
            };
            let tips = graph_tips(&refs, &info);
            sink(Event::RepoInfo(Arc::new(info)));
            sink(Event::Refs(Arc::new(refs)));
            let graph_sink = sink.clone();
            let started = Instant::now();
            shared.engine.load(tips, move |ev| match ev {
                GraphEvent::Snapshot(s) => {
                    if s.phase == fgc_graph::Phase::Complete {
                        release_freed_memory();
                        graph_sink(Event::Timing {
                            op: "graph_load",
                            duration: started.elapsed(),
                            detail: format!(
                                "{} commits, {} lanes",
                                s.stats.commits, s.stats.max_lanes
                            ),
                        });
                    }
                    graph_sink(Event::Graph(s));
                }
                GraphEvent::Failed { error, .. } => graph_sink(Event::Error {
                    op: "graph_load",
                    message: error,
                }),
            });
        }
        Command::Metas(oids) => {
            if let Some(metas) = shared.timed(
                "commit_metas",
                || be.commit_metas(&oids),
                |m| format!("{}", m.len()),
            ) {
                sink(Event::Metas(metas));
            }
        }
        Command::CommitDiff(oid) => {
            let spec = DiffSpec::new(DiffTarget::Commit(oid));
            let diff = shared.timed(
                "commit_diff",
                || be.diff(&spec),
                |d| format!("{} files, {} lines", d.files.len(), d.lines.len()),
            );
            if let Some(diff) = diff {
                if shared.is_current(job.generation) {
                    sink(Event::CommitDiff {
                        oid,
                        diff: Arc::new(diff),
                    });
                }
            }
        }
        Command::Status => {
            let opts = StatusOpts::default();
            if let Some(st) = shared.timed(
                "status",
                || be.status(&opts),
                |s| format!("{} entries", s.entries.len()),
            ) {
                *shared.status.lock().unwrap() = Some(st.clone());
                sink(Event::Status(Arc::new(st)));
            }
        }
        Command::StatusPaths(paths) => {
            let base = shared.status.lock().unwrap().clone();
            let Some(base) = base else {
                shared.submit(Command::Status);
                return;
            };
            let opts = StatusOpts {
                pathspec: paths.clone(),
                ..StatusOpts::default()
            };
            if let Some(fresh) = shared.timed(
                "status_paths",
                || be.status(&opts),
                |s| format!("{} paths → {} entries", paths.len(), s.entries.len()),
            ) {
                let merged = merge_status(&base, &paths, fresh);
                *shared.status.lock().unwrap() = Some(merged.clone());
                sink(Event::Status(Arc::new(merged)));
            }
        }
        Command::Write(_) | Command::Network(_) => unreachable!("routed to their own threads"),
        Command::WorkdirDiff(path) => {
            let paths: Vec<PathBuf> = path.iter().cloned().collect();
            let staged = DiffSpec::new(DiffTarget::HeadToIndex).paths(paths.clone());
            let unstaged = DiffSpec::new(DiffTarget::IndexToWorkdir {
                include_untracked: true,
            })
            .paths(paths);
            let result = shared.timed(
                "workdir_diff",
                || Ok((be.diff(&staged)?, be.diff(&unstaged)?)),
                |(s, u)| format!("{} staged, {} unstaged files", s.files.len(), u.files.len()),
            );
            if let Some((s, u)) = result {
                if shared.is_current(job.generation) {
                    sink(Event::WorkdirDiff {
                        path,
                        staged: Arc::new(s),
                        unstaged: Arc::new(u),
                    });
                }
            }
        }
    }
}

pub use fgc_graph::{GraphOrder, Layout, Phase, RowGeometry};

#[cfg(test)]
mod tests {
    use super::*;
    use fgc_core::{Change, StatusEntry};

    fn e(p: &str, wt: Change) -> StatusEntry {
        StatusEntry {
            path: p.into(),
            orig_path: None,
            index: Change::Unmodified,
            worktree: wt,
        }
    }

    #[test]
    fn merge_replaces_only_covered_paths() {
        let base = StatusSnapshot {
            entries: vec![
                e("a.txt", Change::Modified),
                e("dir/b.txt", Change::Modified),
                e("z.txt", Change::Untracked),
            ],
        };
        // a.txt reverted, dir/ deleted entirely, new c.txt.
        let fresh = StatusSnapshot {
            entries: vec![e("c.txt", Change::Untracked)],
        };
        let merged = merge_status(
            &base,
            &["a.txt".into(), "dir".into(), "c.txt".into()],
            fresh,
        );
        let paths: Vec<_> = merged
            .entries
            .iter()
            .map(|e| e.path.to_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["c.txt", "z.txt"]);
    }
}
