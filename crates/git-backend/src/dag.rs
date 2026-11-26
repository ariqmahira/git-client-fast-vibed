//! Full-history DAG extraction.
//!
//! Commits covered by Git's commit-graph file are read straight from the
//! memory-mapped graph (parents and commit time without inflating any
//! object; see `commit_graph.rs`). Commits newer than the graph file (fetched since it
//! was written) are parsed with libgit2 until the walk reaches graph-covered
//! history. Without a usable commit-graph everything is parsed with libgit2,
//! which is ~20x slower; callers write a graph in the background.

use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::path::PathBuf;

use fgc_core::{CommitDag, DagStats, Error, Oid, Result};
use git2::Repository;

use crate::commit_graph::CommitGraph;

use crate::convert::{map_err, to_core, to_git};
use crate::walk::OidHasher;

const NONE: u32 = u32::MAX;

pub(crate) fn info_dir(repo: &Repository) -> PathBuf {
    repo.commondir().join("objects").join("info")
}

pub(crate) fn has_commit_graph(repo: &Repository) -> bool {
    let info = info_dir(repo);
    info.join("commit-graph").is_file() || info.join("commit-graphs/commit-graph-chain").is_file()
}

/// Opens the commit-graph if Git itself would use it. Git ignores the graph
/// when history is rewritten by shallow boundaries, grafts or replace refs,
/// because stored parents would then be wrong; so do we.
pub(crate) fn open_graph(repo: &Repository) -> Option<CommitGraph> {
    if !has_commit_graph(repo) || repo.is_shallow() {
        return None;
    }
    let common = repo.commondir();
    if common.join("info/grafts").is_file() || has_replace_refs(repo) {
        return None;
    }
    match CommitGraph::open(&info_dir(repo)) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("ignoring unreadable commit-graph: {e}");
            None
        }
    }
}

fn has_replace_refs(repo: &Repository) -> bool {
    repo.references_glob("refs/replace/*")
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// A commit reference during extraction: a commit-graph position, or an
/// index into the commits parsed from the object database.
#[derive(Clone, Copy)]
enum Ref {
    Graph(u32),
    Odb(u32),
}

struct Extract<'a> {
    repo: &'a Repository,
    odb: git2::Odb<'a>,
    graph: Option<&'a CommitGraph>,
    /// Graph positions already seen in pass 1 (bitset).
    reach: Vec<u64>,
    /// Graph positions reached in pass 1, in order: the seeds of pass 2.
    stack: Vec<u32>,
    odb_oids: Vec<Oid>,
    odb_index: HashMap<Oid, u32, BuildHasherDefault<OidHasher>>,
}

impl Extract<'_> {
    fn mark(&mut self, pos: u32) {
        let (w, b) = ((pos / 64) as usize, pos % 64);
        if self.reach[w] & (1 << b) == 0 {
            self.reach[w] |= 1 << b;
            self.stack.push(pos);
        }
    }

    /// Resolves `oid`, queueing it for expansion if new. `None` if the
    /// commit isn't present (shallow boundary, missing object).
    fn resolve(&mut self, oid: Oid) -> Option<Ref> {
        if let Some(pos) = self.graph.and_then(|g| g.lookup(&oid)) {
            self.mark(pos);
            return Some(Ref::Graph(pos));
        }
        if let Some(&i) = self.odb_index.get(&oid) {
            return Some(Ref::Odb(i));
        }
        if !self.odb.exists(to_git(oid)) {
            return None;
        }
        let i = self.odb_oids.len() as u32;
        self.odb_oids.push(oid);
        self.odb_index.insert(oid, i);
        Some(Ref::Odb(i))
    }
}

/// Builds the DAG reachable from `tips` in two passes:
///
/// 1. parse commits not covered by the graph (BFS via libgit2) until the
///    walk reaches graph-covered history;
/// 2. BFS over the commit-graph from there.
///
/// Parsed commits get ids `0..k`, graph commits follow in BFS order.
pub(crate) fn build(
    repo: &Repository,
    graph: Option<&CommitGraph>,
    tips: &[Oid],
) -> Result<CommitDag> {
    let n_graph = graph.map_or(0, |g| g.num_commits() as usize);
    let mut buf: Vec<u32> = Vec::with_capacity(8);
    let mut ex = Extract {
        repo,
        odb: repo.odb().map_err(map_err)?,
        graph,
        reach: vec![0u64; n_graph.div_ceil(64)],
        stack: Vec::new(),
        odb_oids: Vec::new(),
        odb_index: HashMap::default(),
    };

    for tip in tips {
        let is_commit = ex.resolve(*tip).is_some()
            && ex
                .repo
                .find_object(to_git(*tip), None)
                .map(|o| o.kind() == Some(git2::ObjectType::Commit))
                .unwrap_or(false);
        if !is_commit {
            return Err(Error::InvalidInput(format!("tip {tip} is not a commit")));
        }
    }

    // Pass 1: commits outside the commit-graph.
    let mut odb_times = Vec::new();
    let mut odb_parent_offsets = vec![0u32];
    let mut odb_parents: Vec<Ref> = Vec::new();
    let mut i = 0;
    while i < ex.odb_oids.len() {
        let commit = ex
            .repo
            .find_commit(to_git(ex.odb_oids[i]))
            .map_err(map_err)?;
        odb_times.push(commit.time().seconds());
        let parent_ids: Vec<git2::Oid> = commit.parent_ids().collect();
        drop(commit);
        for pid in parent_ids {
            if let Some(r) = ex.resolve(to_core(pid)) {
                odb_parents.push(r);
            }
        }
        odb_parent_offsets.push(odb_parents.len() as u32);
        i += 1;
    }

    // Pass 2: BFS over the commit-graph from the graph commits reached so
    // far, assigning ids in discovery order. (Measured on linux.git: BFS
    // order is the best trade-off between extraction cost and the locality
    // the topological sort gets from it; a time-ordered walk sorts faster
    // but costs 3x more to extract.)
    let k = ex.odb_oids.len() as u32;
    let mut id_of_pos = vec![NONE; n_graph];
    let mut queue: Vec<u32> = Vec::with_capacity(n_graph);
    for &pos in &ex.stack {
        id_of_pos[pos as usize] = k + queue.len() as u32;
        queue.push(pos);
    }
    let map = |r: Ref, id_of_pos: &[u32]| match r {
        Ref::Graph(p) => id_of_pos[p as usize],
        Ref::Odb(i) => i,
    };

    let mut dag = CommitDag {
        oids: Vec::with_capacity(k as usize + n_graph),
        commit_times: Vec::with_capacity(k as usize + n_graph),
        parent_offsets: Vec::with_capacity(k as usize + n_graph + 1),
        parents: Vec::with_capacity(k as usize + n_graph + n_graph / 8),
        stats: DagStats::default(),
    };
    dag.parent_offsets.push(0);
    for i in 0..k as usize {
        dag.oids.push(ex.odb_oids[i]);
        dag.commit_times.push(odb_times[i]);
        let range = odb_parent_offsets[i] as usize..odb_parent_offsets[i + 1] as usize;
        dag.parents
            .extend(odb_parents[range].iter().map(|&r| map(r, &id_of_pos)));
        dag.parent_offsets.push(dag.parents.len() as u32);
    }
    if let Some(g) = graph {
        let mut qi = 0;
        while qi < queue.len() {
            let pos = queue[qi];
            buf.clear();
            let time = g.commit(pos, &mut buf)?;
            dag.oids.push(g.oid(pos));
            dag.commit_times.push(time);
            for &p in &buf {
                let slot = &mut id_of_pos[p as usize];
                if *slot == NONE {
                    *slot = k + queue.len() as u32;
                    queue.push(p);
                }
                dag.parents.push(*slot);
            }
            dag.parent_offsets.push(dag.parents.len() as u32);
            qi += 1;
        }
    }
    dag.stats = DagStats {
        from_commit_graph: queue.len() as u32,
        parsed: k,
    };
    Ok(dag)
}

/// `git commit-graph write --reachable --split`, respecting the user's config.
pub(crate) fn write_commit_graph(repo: &Repository) -> Result<()> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo.path())
        .args([
            "commit-graph",
            "write",
            "--reachable",
            "--split",
            "--no-progress",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(Error::Engine(format!(
            "git commit-graph write failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}
