//! Resumable, incremental commit walk.
//!
//! libgit2's `Revwalk` borrows the `Repository`, so it can't be parked between
//! page requests, and its topological sort walks the entire history before
//! yielding the first commit (seconds on linux.git). Instead we run our own
//! commit-time-ordered walk (the same priority-queue approach as libgit2's
//! `SORT_TIME`) whose state is plain data: a heap of `(time, oid)` plus a seen
//! set. That state is cached per set of tips, so paging `0..500`, `500..1000`,
//! ... costs only the page, and any worker thread can continue it.
//!
//! Ordering guarantee: newest commit time first; a commit is only ever queued
//! after one of its children was emitted. Full topological order (every child
//! before every parent, even with clock skew) is the graph engine's job (M2),
//! using commit-graph generation numbers.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use fgc_core::{CommitMeta, Error, Oid, Result};
use git2::Repository;

use crate::convert::{map_err, to_core, to_git};

/// Oids are uniformly random, so hashing the first 8 bytes is as good as
/// SipHash and much cheaper for million-entry sets.
#[derive(Default, Clone, Copy)]
pub(crate) struct OidHasher(u64);

impl Hasher for OidHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // `Oid` hashes as its 20-byte array (preceded by nothing, since it's a
        // fixed-size array). Fold whatever we get, fast path for >= 8 bytes.
        if bytes.len() >= 8 {
            self.0 ^= u64::from_le_bytes(bytes[..8].try_into().unwrap());
        } else {
            for &b in bytes {
                self.0 = self.0.rotate_left(8) ^ b as u64;
            }
        }
    }
}

type OidSet = HashSet<Oid, BuildHasherDefault<OidHasher>>;

#[derive(PartialEq, Eq)]
struct Item {
    time: i64,
    oid: Oid,
}

impl Ord for Item {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap on time; ties broken by oid for determinism.
        self.time
            .cmp(&other.time)
            .then_with(|| other.oid.cmp(&self.oid))
    }
}

impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
pub(crate) struct WalkState {
    started: bool,
    heap: BinaryHeap<Item>,
    seen: OidSet,
    /// Every commit emitted so far, in walk order.
    order: Vec<Oid>,
}

impl WalkState {
    pub(crate) fn page(
        &mut self,
        repo: &Repository,
        tips: &[Oid],
        range: Range<usize>,
    ) -> Result<Vec<CommitMeta>> {
        if !self.started {
            for &tip in tips {
                if self.seen.insert(tip) {
                    let c = repo.find_commit(to_git(tip)).map_err(|e| {
                        Error::InvalidInput(format!("tip {tip} is not a commit: {}", e.message()))
                    })?;
                    self.heap.push(Item {
                        time: c.time().seconds(),
                        oid: tip,
                    });
                }
            }
            self.started = true;
        }

        let cap = range.end.saturating_sub(range.start).min(64 * 1024);
        let mut out = Vec::with_capacity(cap);

        // Part of the page we've already walked past: just load metadata.
        let have = self.order.len();
        for &oid in &self.order[range.start.min(have)..range.end.min(have)] {
            let c = repo.find_commit(to_git(oid)).map_err(map_err)?;
            out.push(commit_meta(&c));
        }

        // Advance the walk for the rest.
        while self.order.len() < range.end {
            let Some(item) = self.heap.pop() else { break };
            let commit = repo.find_commit(to_git(item.oid)).map_err(map_err)?;
            for pid in commit.parent_ids() {
                let p = to_core(pid);
                if !self.seen.insert(p) {
                    continue;
                }
                match repo.find_commit(pid) {
                    Ok(pc) => self.heap.push(Item {
                        time: pc.time().seconds(),
                        oid: p,
                    }),
                    // Shallow / partial clones: the boundary parent isn't
                    // present. Treat it as the end of that line.
                    Err(e) if e.code() == git2::ErrorCode::NotFound => {}
                    Err(e) => return Err(map_err(e)),
                }
            }
            let idx = self.order.len();
            self.order.push(item.oid);
            if idx >= range.start {
                out.push(commit_meta(&commit));
            }
        }
        Ok(out)
    }
}

fn lossy(b: &[u8]) -> Box<str> {
    match std::str::from_utf8(b) {
        Ok(s) => s.into(),
        Err(_) => String::from_utf8_lossy(b).into(),
    }
}

pub(crate) fn commit_meta(c: &git2::Commit<'_>) -> CommitMeta {
    let author = c.author();
    CommitMeta {
        oid: to_core(c.id()),
        parents: c.parent_ids().map(to_core).collect(),
        summary: c.summary_bytes().map(lossy).unwrap_or_default(),
        author_name: lossy(author.name_bytes()),
        author_email: lossy(author.email_bytes()),
        author_time: author.when().seconds(),
        author_tz_minutes: author.when().offset_minutes(),
        commit_time: c.time().seconds(),
    }
}

/// Small LRU-ish cache of walk states keyed by the (sorted, deduped) tips.
/// History reachable from a fixed set of tips is immutable, so entries never
/// go stale; new commits mean new tips and therefore a new key.
#[derive(Default)]
pub(crate) struct WalkCache {
    map: HashMap<Vec<Oid>, Arc<Mutex<WalkState>>>,
    order: VecDeque<Vec<Oid>>,
}

const MAX_WALKS: usize = 4;

impl WalkCache {
    pub(crate) fn get(&mut self, tips: &[Oid]) -> Arc<Mutex<WalkState>> {
        let mut key = tips.to_vec();
        key.sort_unstable();
        key.dedup();
        if let Some(s) = self.map.get(&key) {
            return s.clone();
        }
        if self.map.len() >= MAX_WALKS {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        let state = Arc::new(Mutex::new(WalkState::default()));
        self.map.insert(key.clone(), state.clone());
        self.order.push_back(key);
        state
    }
}
