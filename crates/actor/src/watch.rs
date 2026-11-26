//! File-system watcher (plan §5): working-tree edits trigger a status run on
//! just those paths; `.git` changes are filtered down to the ones that
//! matter (HEAD, refs, packed-refs, index). Events are debounced so an
//! editor's save (write temp + rename + chmod) or a fetch writing hundreds of
//! refs becomes one update.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// What changed, after filtering and debouncing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsChanges {
    /// HEAD, refs or packed-refs: branches/tags may have moved.
    pub refs: bool,
    /// The index: staged state changed (full status needed).
    pub index: bool,
    /// Working-tree paths (relative), for a targeted status.
    pub paths: BTreeSet<PathBuf>,
}

impl FsChanges {
    pub fn is_empty(&self) -> bool {
        !self.refs && !self.index && self.paths.is_empty()
    }
}

/// Quiet period that ends a burst of events.
const DEBOUNCE: Duration = Duration::from_millis(25);
/// Upper bound on how long a continuous burst is held back.
const MAX_DELAY: Duration = Duration::from_millis(120);

pub struct RepoWatcher {
    _watchers: Vec<RecommendedWatcher>,
    _thread: std::thread::JoinHandle<()>,
}

/// Classifies one changed path. Pure, so it's unit-tested.
pub fn classify(path: &Path, git_dirs: &[PathBuf], workdir: Option<&Path>, out: &mut FsChanges) {
    for gd in git_dirs {
        if let Ok(rel) = path.strip_prefix(gd) {
            let first = rel.components().next().and_then(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            });
            match first {
                Some(
                    "HEAD" | "ORIG_HEAD" | "FETCH_HEAD" | "MERGE_HEAD" | "packed-refs" | "refs",
                ) => {
                    if !rel.extension().is_some_and(|e| e == "lock") {
                        out.refs = true;
                    }
                }
                Some("index") => out.index = true,
                _ => {} // objects, logs, locks, hooks, …
            }
            return;
        }
    }
    if let Some(wd) = workdir {
        if let Ok(rel) = path.strip_prefix(wd) {
            if !rel.as_os_str().is_empty() {
                out.paths.insert(rel.to_path_buf());
            }
        }
    }
}

impl RepoWatcher {
    /// Watches `workdir` (if any) and the git directories, calling
    /// `on_change` on a background thread once per debounced burst.
    pub fn spawn(
        git_dirs: Vec<PathBuf>,
        workdir: Option<PathBuf>,
        on_change: impl Fn(FsChanges) + Send + 'static,
    ) -> notify::Result<RepoWatcher> {
        // Canonicalize so event paths (which the OS reports resolved) match.
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let git_dirs: Vec<PathBuf> = git_dirs.iter().map(|p| canon(p)).collect();
        let workdir = workdir.map(|p| canon(&p));

        let (tx, rx) = mpsc::channel::<notify::Event>();
        let mut watchers = Vec::new();
        let mut roots: Vec<PathBuf> = workdir.iter().cloned().collect();
        for gd in &git_dirs {
            if !roots.iter().any(|r| gd.starts_with(r)) {
                roots.push(gd.clone());
            }
        }
        for root in &roots {
            let tx = tx.clone();
            let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res {
                    let _ = tx.send(ev);
                }
            })?;
            w.watch(root, RecursiveMode::Recursive)?;
            watchers.push(w);
        }
        drop(tx);

        let thread = std::thread::Builder::new()
            .name("fgc-watcher".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let mut changes = FsChanges::default();
                    let started = Instant::now();
                    let mut ev = Some(first);
                    while let Some(e) = ev.take() {
                        let relevant = match e.kind {
                            EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
                            EventKind::Access(_) => false,
                            _ => true,
                        };
                        if relevant {
                            for p in &e.paths {
                                classify(p, &git_dirs, workdir.as_deref(), &mut changes);
                            }
                        }
                        let left = MAX_DELAY.saturating_sub(started.elapsed());
                        if left.is_zero() {
                            break;
                        }
                        ev = rx.recv_timeout(DEBOUNCE.min(left)).ok();
                    }
                    if !changes.is_empty() {
                        on_change(changes);
                    }
                }
            })
            .expect("spawn watcher thread");
        Ok(RepoWatcher {
            _watchers: watchers,
            _thread: thread,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_paths() {
        let gd = vec![PathBuf::from("/r/.git")];
        let wd = Some(Path::new("/r"));
        let mut c = FsChanges::default();
        classify(Path::new("/r/.git/objects/ab/cdef"), &gd, wd, &mut c);
        classify(Path::new("/r/.git/logs/HEAD"), &gd, wd, &mut c);
        classify(Path::new("/r/.git/refs/heads/main.lock"), &gd, wd, &mut c);
        assert!(c.is_empty());
        classify(Path::new("/r/.git/refs/heads/main"), &gd, wd, &mut c);
        assert!(c.refs && !c.index);
        classify(Path::new("/r/.git/index"), &gd, wd, &mut c);
        assert!(c.index);
        classify(Path::new("/r/src/lib.rs"), &gd, wd, &mut c);
        classify(Path::new("/r"), &gd, wd, &mut c);
        assert_eq!(
            c.paths.iter().collect::<Vec<_>>(),
            vec![Path::new("src/lib.rs")]
        );
    }
}
