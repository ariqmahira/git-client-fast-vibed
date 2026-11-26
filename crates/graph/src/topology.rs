use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::OnceLock;

use fgc_core::{CommitDag, Oid};

/// How rows are ordered. Both guarantee every child is above its parents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GraphOrder {
    /// Newest commit time first among the commits whose children are all
    /// shown (`git log --date-order`). Interleaves parallel branches.
    #[default]
    Date,
    /// Keep each line of history together (`git log --topo-order`): after a
    /// merge, the merged branch is shown before continuing the mainline.
    Topo,
}

/// Commits in display order. Row `r`'s parents are rows `> r`.
#[derive(Debug)]
pub struct Topology {
    oids: Vec<Oid>,
    parent_offsets: Vec<u32>,
    parents: Vec<u32>,
    order: GraphOrder,
    /// Rows sorted by oid, for `row_of`. Built lazily (in the background by
    /// the engine) because it's only needed for selection/search.
    oid_index: OnceLock<Vec<u32>>,
}

/// Sort key prefix so equal timestamps order deterministically by oid,
/// independent of discovery order (which changes when tips change).
fn oid_key(oid: &Oid) -> u64 {
    u64::from_be_bytes(oid.0[..8].try_into().unwrap())
}

fn date_key(time: i64, oid: &Oid) -> u64 {
    let t = time.clamp(0, (1 << 34) - 1) as u64;
    (t << 30) | (oid_key(oid) >> 34)
}

impl Topology {
    pub fn from_dag(dag: CommitDag, order: GraphOrder) -> Topology {
        let n = dag.len();
        let mut indegree = vec![0u32; n];
        for &p in &dag.parents {
            indegree[p as usize] += 1;
        }

        let mut rows: Vec<u32> = Vec::with_capacity(n);
        match order {
            GraphOrder::Date => {
                // One packed key per commit: time in the high 34 bits (Git
                // stores 34-bit times), oid prefix as tie-break below. One
                // random read per push, 16-byte heap entries.
                let keys: Vec<u64> = (0..n)
                    .map(|i| date_key(dag.commit_times[i], &dag.oids[i]))
                    .collect();
                let mut ready: BinaryHeap<(u64, Reverse<u32>)> = (0..n as u32)
                    .filter(|&i| indegree[i as usize] == 0)
                    .map(|i| (keys[i as usize], Reverse(i)))
                    .collect();
                while let Some((_, Reverse(id))) = ready.pop() {
                    rows.push(id);
                    for &p in dag.parents_of(id) {
                        let d = &mut indegree[p as usize];
                        *d -= 1;
                        if *d == 0 {
                            ready.push((keys[p as usize], Reverse(p)));
                        }
                    }
                }
            }
            GraphOrder::Topo => {
                // Heads oldest-first so the newest head is on top of the stack.
                let mut heads: Vec<u32> = (0..n as u32)
                    .filter(|&i| indegree[i as usize] == 0)
                    .collect();
                heads.sort_unstable_by_key(|&i| {
                    (dag.commit_times[i as usize], oid_key(&dag.oids[i as usize]))
                });
                let mut stack = heads;
                while let Some(id) = stack.pop() {
                    rows.push(id);
                    for &p in dag.parents_of(id) {
                        let d = &mut indegree[p as usize];
                        *d -= 1;
                        if *d == 0 {
                            stack.push(p);
                        }
                    }
                }
            }
        }
        debug_assert_eq!(rows.len(), n, "commit DAG must be acyclic");

        let mut node_row = vec![0u32; n];
        for (r, &id) in rows.iter().enumerate() {
            node_row[id as usize] = r as u32;
        }
        let mut oids = Vec::with_capacity(n);
        let mut parent_offsets = Vec::with_capacity(n + 1);
        let mut parents = Vec::with_capacity(dag.parents.len());
        parent_offsets.push(0);
        for &id in &rows {
            oids.push(dag.oids[id as usize]);
            parents.extend(dag.parents_of(id).iter().map(|&p| node_row[p as usize]));
            parent_offsets.push(parents.len() as u32);
        }
        Topology {
            oids,
            parent_offsets,
            parents,
            order,
            oid_index: OnceLock::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.oids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.oids.is_empty()
    }

    pub fn order(&self) -> GraphOrder {
        self.order
    }

    pub fn oid(&self, row: u32) -> Oid {
        self.oids[row as usize]
    }

    pub fn oids(&self) -> &[Oid] {
        &self.oids
    }

    /// Parent rows of `row` (all greater than `row`), first parent first.
    pub fn parents(&self, row: u32) -> &[u32] {
        let r = row as usize;
        &self.parents[self.parent_offsets[r] as usize..self.parent_offsets[r + 1] as usize]
    }

    /// Builds the oid → row index (~150 ms for 1.5M commits). Idempotent.
    pub fn build_index(&self) {
        self.oid_index.get_or_init(|| {
            let mut idx: Vec<u32> = (0..self.oids.len() as u32).collect();
            idx.sort_unstable_by(|&a, &b| self.oids[a as usize].cmp(&self.oids[b as usize]));
            idx
        });
    }

    /// Row of `oid`. Uses the index if built, otherwise a linear scan
    /// (still only a few ms for linux.git), so it never blocks on the build.
    pub fn row_of(&self, oid: &Oid) -> Option<u32> {
        match self.oid_index.get() {
            Some(idx) => idx
                .binary_search_by(|&r| self.oids[r as usize].cmp(oid))
                .ok()
                .map(|i| idx[i]),
            None => self.oids.iter().position(|o| o == oid).map(|r| r as u32),
        }
    }

    /// Approximate heap size in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.oids.capacity() * 20
            + (self.parent_offsets.capacity() + self.parents.capacity()) * 4
            + self.oid_index.get().map_or(0, |i| i.capacity() * 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(i: u8) -> Oid {
        Oid([i; 20])
    }

    /// a(5) ← b(4) ← d(1)  and a ← c(3) ← d, merge m(6) of b and c.
    /// Side-branch commit c is newer than b.
    fn diamond() -> CommitDag {
        let (m, b, c, d, a) = (o(6), o(4), o(3), o(1), o(9));
        // a is a separate tip off d, older than everything else on top.
        CommitDag::from_commits([
            (m, 60, &[b, c][..]),
            (b, 40, &[d][..]),
            (c, 50, &[d][..]),
            (d, 10, &[][..]),
            (a, 20, &[d][..]),
        ])
    }

    fn order(t: &Topology) -> Vec<u8> {
        t.oids().iter().map(|o| o.0[0]).collect()
    }

    #[test]
    fn date_order() {
        let t = Topology::from_dag(diamond(), GraphOrder::Date);
        assert_eq!(order(&t), vec![6, 3, 4, 9, 1]);
        for r in 0..t.len() as u32 {
            assert!(t.parents(r).iter().all(|&p| p > r));
        }
        assert_eq!(t.parents(0), &[2, 1]); // m → b (row 2), c (row 1)
    }

    #[test]
    fn topo_order_keeps_branches_together() {
        let t = Topology::from_dag(diamond(), GraphOrder::Topo);
        // After m: c (last parent) first, then b, then a, then d.
        assert_eq!(order(&t), vec![6, 3, 4, 9, 1]);
        let t2 = Topology::from_dag(
            CommitDag::from_commits([
                (o(1), 100, &[o(2), o(5)][..]),
                (o(2), 90, &[o(3)][..]),
                (o(3), 80, &[o(4)][..]),
                (o(5), 95, &[o(4)][..]),
                (o(4), 10, &[][..]),
            ]),
            GraphOrder::Topo,
        );
        // Date order would interleave 5 between 1 and 2; topo shows the side
        // branch (5) completely, then the mainline 2, 3.
        assert_eq!(order(&t2), vec![1, 5, 2, 3, 4]);
    }

    #[test]
    fn row_lookup() {
        let t = Topology::from_dag(diamond(), GraphOrder::Date);
        assert_eq!(t.row_of(&o(4)), Some(2));
        assert_eq!(t.row_of(&o(7)), None);
        t.build_index();
        assert_eq!(t.row_of(&o(4)), Some(2));
        assert_eq!(t.row_of(&o(1)), Some(4));
        assert_eq!(t.row_of(&o(7)), None);
    }
}
