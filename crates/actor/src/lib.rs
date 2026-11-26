//! `RepoActor`: one per open repository (plan §5).
//!
//! The UI sends [`Command`]s and receives [`Event`]s; it never calls Git.
//! The actor owns the backend and the graph engine and runs jobs on a small
//! worker pool, highest [`Priority`] first. Commands whose results supersede
//! each other (e.g. "show the diff of commit X" while the user arrows
//! through commits) carry a generation id: stale jobs are skipped if they
//! haven't started and their results dropped if they have.
//!
//! UI-agnostic on purpose: the GPUI shell uses it today, and a fallback
//! shell (plan §4) would use the same crate.

use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fgc_core::{
    CommitMeta, Diff, DiffSpec, DiffTarget, GitBackend, Oid, RefInfo, RefKind, RepoInfo, Result,
    StatusOpts, StatusSnapshot,
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
}

#[derive(Debug, Clone)]
pub struct ActorOptions {
    pub workers: usize,
    pub graph: EngineOptions,
}

impl Default for ActorOptions {
    fn default() -> Self {
        ActorOptions {
            workers: 3,
            graph: EngineOptions::default(),
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
}

impl Shared {
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
        });
        let workers = (0..opts.workers.max(1))
            .map(|i| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(format!("fgc-worker-{i}"))
                    .spawn(move || worker(shared))
                    .expect("spawn worker")
            })
            .collect();
        let actor = RepoActor { shared, workers };
        actor.send(Command::Refresh);
        actor.send(Command::Status);
        actor
    }

    /// Queues a command. Never blocks on Git; safe to call from the UI thread.
    pub fn send(&self, cmd: Command) {
        let (priority, slot) = match &cmd {
            Command::Refresh => (Priority::Interactive, None),
            Command::CommitDiff(_) => (Priority::Interactive, Some(Slot::CommitDiff)),
            Command::WorkdirDiff(_) => (Priority::Interactive, Some(Slot::WorkdirDiff)),
            Command::Metas(_) => (Priority::Visible, None),
            Command::Status => (Priority::Background, None),
        };
        let generation = slot.map(|s| {
            (
                s,
                self.shared.slot_gen(s).fetch_add(1, Ordering::SeqCst) + 1,
            )
        });
        let job = Job {
            priority,
            seq: self.shared.seq.fetch_add(1, Ordering::Relaxed),
            cmd,
            generation,
        };
        self.shared.queue.lock().unwrap().push(job);
        self.shared.wake.notify_one();
    }

    pub fn backend(&self) -> &Arc<dyn GitBackend> {
        &self.shared.backend
    }
}

impl Drop for RepoActor {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.engine.cancel();
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
                sink(Event::Status(Arc::new(st)));
            }
        }
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
