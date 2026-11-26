use crate::Oid;

/// Every commit reachable from a set of tips, as a compact DAG.
///
/// Commits are identified by dense `u32` ids (indices into `oids`), and
/// parents are stored CSR-style (`parent_offsets`/`parents`), so the whole
/// linux.git history (~1.5M commits) is ~45 MB and cheap to traverse.
/// Parents that aren't present (shallow/partial history) are omitted.
///
/// Ids are in discovery order and carry no ordering guarantee; the graph
/// engine sorts the DAG into display rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitDag {
    pub oids: Vec<Oid>,
    /// Committer time, seconds since the Unix epoch.
    pub commit_times: Vec<i64>,
    /// `parents[parent_offsets[i]..parent_offsets[i + 1]]` are commit `i`'s parents,
    /// first parent first. Length `oids.len() + 1`.
    pub parent_offsets: Vec<u32>,
    pub parents: Vec<u32>,
    /// Where the data came from (for diagnostics and the probe).
    pub stats: DagStats,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DagStats {
    /// Commits read from Git's commit-graph file (no object parsing).
    pub from_commit_graph: u32,
    /// Commits parsed from the object database (not in the commit-graph).
    pub parsed: u32,
}

impl CommitDag {
    pub fn len(&self) -> usize {
        self.oids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.oids.is_empty()
    }

    pub fn parents_of(&self, id: u32) -> &[u32] {
        let i = id as usize;
        &self.parents[self.parent_offsets[i] as usize..self.parent_offsets[i + 1] as usize]
    }

    /// Builds a DAG from `(oid, commit_time, parent oids)` triples. Parents not
    /// in the set are dropped. Used for provisional graphs and tests.
    pub fn from_commits<'a>(commits: impl IntoIterator<Item = (Oid, i64, &'a [Oid])>) -> CommitDag {
        let commits: Vec<_> = commits.into_iter().collect();
        let index: std::collections::HashMap<Oid, u32> = commits
            .iter()
            .enumerate()
            .map(|(i, c)| (c.0, i as u32))
            .collect();
        let mut dag = CommitDag {
            oids: Vec::with_capacity(commits.len()),
            commit_times: Vec::with_capacity(commits.len()),
            parent_offsets: Vec::with_capacity(commits.len() + 1),
            parents: Vec::new(),
            stats: DagStats::default(),
        };
        dag.parent_offsets.push(0);
        for (oid, time, parents) in commits {
            dag.oids.push(oid);
            dag.commit_times.push(time);
            dag.parents
                .extend(parents.iter().filter_map(|p| index.get(p).copied()));
            dag.parent_offsets.push(dag.parents.len() as u32);
        }
        dag
    }
}
