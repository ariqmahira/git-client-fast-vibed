//! Incremental lane layout.
//!
//! Lane rules (deterministic; a function of the lane state and the row's
//! parents only, which is what makes checkpoint replay and chunk reuse work):
//!
//! - Each lane is either free or *expects* one row (the next commit drawn in
//!   it). A row is expected by at most one lane.
//! - A commit sits in the lane expecting it, or else the leftmost free lane
//!   (a branch tip).
//! - Its first parent continues in the commit's lane. Any parent that is
//!   already expected by another lane is joined into that lane instead of
//!   opening a new one; other parents open the leftmost free lane at or right
//!   of the commit's lane.
//! - Lanes never move sideways; freed lanes are reused, trailing ones trimmed.
//!
//! Storage is chunked: per row only its lane (`u16`), plus the lane state at
//! each chunk start encoded *relative to the chunk start*. Relative encoding
//! makes chunks position-independent, so when new commits are prepended the
//! old chunks can be reused verbatim once the lane state converges.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::Topology;

/// Target rows per chunk (= max rows replayed to render any window).
pub const CHUNK_ROWS: usize = 1024;

const FREE: u32 = u32::MAX;
const NO_LANE: u16 = u16::MAX;

#[derive(Debug)]
struct Chunk {
    /// Lane of each row in the chunk.
    lanes: Vec<u16>,
    /// Lane state before the first row: expected row minus chunk start, or FREE.
    start_state: Box<[u32]>,
    /// Max lane count reached inside the chunk.
    width: u16,
}

/// Everything needed to draw one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowGeometry {
    pub row: u32,
    /// Lane of the commit dot.
    pub lane: u16,
    /// A line enters the dot from the top of the row (in `lane`).
    pub has_incoming: bool,
    /// Lanes passing straight through the row without touching the dot.
    pub passthrough: Vec<u16>,
    /// Lanes the dot connects to at the bottom of the row (one per parent;
    /// equal to `lane` for a first parent continuing straight down).
    pub outgoing: Vec<u16>,
}

impl RowGeometry {
    /// Lanes needed to draw this row.
    pub fn width(&self) -> u16 {
        let m = self.passthrough.iter().chain(&self.outgoing).copied().max();
        m.map_or(self.lane, |m| m.max(self.lane)) + 1
    }
}

trait ExpectMap {
    fn take(&mut self, row: u32) -> Option<u16>;
    fn get(&self, row: u32) -> Option<u16>;
    fn set(&mut self, row: u32, lane: u16);
}

struct VecExpect(Vec<u16>);

impl ExpectMap for VecExpect {
    fn take(&mut self, row: u32) -> Option<u16> {
        let l = std::mem::replace(&mut self.0[row as usize], NO_LANE);
        (l != NO_LANE).then_some(l)
    }
    fn get(&self, row: u32) -> Option<u16> {
        let l = self.0[row as usize];
        (l != NO_LANE).then_some(l)
    }
    fn set(&mut self, row: u32, lane: u16) {
        self.0[row as usize] = lane;
    }
}

impl ExpectMap for HashMap<u32, u16> {
    fn take(&mut self, row: u32) -> Option<u16> {
        self.remove(&row)
    }
    fn get(&self, row: u32) -> Option<u16> {
        HashMap::get(self, &row).copied()
    }
    fn set(&mut self, row: u32, lane: u16) {
        self.insert(row, lane);
    }
}

fn alloc(lanes: &mut Vec<u32>, from: usize) -> u16 {
    let idx = match lanes[from.min(lanes.len())..]
        .iter()
        .position(|&l| l == FREE)
    {
        Some(i) => from + i,
        None => {
            lanes.push(FREE);
            lanes.len() - 1
        }
    };
    assert!(idx < NO_LANE as usize, "graph wider than {} lanes", NO_LANE);
    idx as u16
}

/// Lays out one row. Returns (lane, has_incoming); fills `outgoing` if given.
fn step_row(
    lanes: &mut Vec<u32>,
    expect: &mut impl ExpectMap,
    row: u32,
    parents: &[u32],
    mut outgoing: Option<&mut Vec<u16>>,
) -> (u16, bool) {
    let incoming = expect.take(row);
    let lane = match incoming {
        Some(l) => l,
        None => alloc(lanes, 0),
    };
    lanes[lane as usize] = FREE;
    for (k, &p) in parents.iter().enumerate() {
        let target = if let Some(m) = expect.get(p) {
            m
        } else {
            let t = if k == 0 {
                lane
            } else {
                alloc(lanes, lane as usize)
            };
            lanes[t as usize] = p;
            expect.set(p, t);
            t
        };
        if let Some(out) = outgoing.as_deref_mut() {
            out.push(target);
        }
    }
    while lanes.last() == Some(&FREE) {
        lanes.pop();
    }
    (lane, incoming.is_some())
}

/// An immutable (possibly partial) layout. Cheap to clone: chunks are shared.
#[derive(Debug, Clone)]
pub struct Layout {
    topology: Arc<Topology>,
    /// (start row, chunk), sorted by start.
    chunks: Arc<[(u32, Arc<Chunk>)]>,
    rows_laid: u32,
    max_lanes: u16,
    reused_from: Option<u32>,
}

impl Layout {
    pub fn topology(&self) -> &Arc<Topology> {
        &self.topology
    }

    /// Rows laid out so far (all of them once complete).
    pub fn rows_laid(&self) -> u32 {
        self.rows_laid
    }

    pub fn is_complete(&self) -> bool {
        self.rows_laid as usize == self.topology.len()
    }

    /// Widest lane count in the laid-out rows.
    pub fn max_lanes(&self) -> u16 {
        self.max_lanes
    }

    /// First row whose layout was reused from the previous layout, if any.
    pub fn reused_from(&self) -> Option<u32> {
        self.reused_from
    }

    fn chunk_index(&self, row: u32) -> usize {
        match self.chunks.binary_search_by_key(&row, |c| c.0) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }

    /// Lane of the commit dot in `row`, if laid out.
    pub fn lane(&self, row: u32) -> Option<u16> {
        if row >= self.rows_laid {
            return None;
        }
        let (start, chunk) = &self.chunks[self.chunk_index(row)];
        chunk.lanes.get((row - start) as usize).copied()
    }

    /// Geometry for `rows` (clamped to the laid-out rows). Replays from the
    /// nearest chunk checkpoint: at most `CHUNK_ROWS` extra rows of work.
    pub fn rows(&self, rows: Range<u32>) -> Vec<RowGeometry> {
        let end = rows.end.min(self.rows_laid);
        if rows.start >= end {
            return Vec::new();
        }
        let ci = self.chunk_index(rows.start);
        let (start, chunk) = &self.chunks[ci];
        let mut lanes: Vec<u32> = chunk
            .start_state
            .iter()
            .map(|&rel| if rel == FREE { FREE } else { rel + start })
            .collect();
        let mut expect: HashMap<u32, u16> = lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| **l != FREE)
            .map(|(i, &l)| (l, i as u16))
            .collect();
        let mut out = Vec::with_capacity((end - rows.start) as usize);
        let topo = &self.topology;
        for row in *start..end {
            let parents = topo.parents(row);
            if row < rows.start {
                step_row(&mut lanes, &mut expect, row, parents, None);
                continue;
            }
            let before = lanes.clone();
            let mut outgoing = Vec::with_capacity(parents.len());
            let (lane, has_incoming) =
                step_row(&mut lanes, &mut expect, row, parents, Some(&mut outgoing));
            debug_assert_eq!(Some(lane), self.lane(row), "replay diverged at row {row}");
            let passthrough = before
                .iter()
                .enumerate()
                .filter(|&(i, &l)| l != FREE && !(has_incoming && i == lane as usize))
                .map(|(i, _)| i as u16)
                .collect();
            out.push(RowGeometry {
                row,
                lane,
                has_incoming,
                passthrough,
                outgoing,
            });
        }
        out
    }

    /// Approximate heap size in bytes (layout only, not the topology).
    pub fn heap_bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|(_, c)| c.lanes.capacity() * 2 + c.start_state.len() * 4 + 48)
            .sum()
    }
}

struct Reuse {
    prev: Layout,
    /// New row = old row + delta.
    delta: u32,
    next_chunk: usize,
}

/// Builds a [`Layout`] incrementally: call [`LayoutBuilder::step`] with a row
/// budget until it returns `true`, publishing [`LayoutBuilder::snapshot`]s in
/// between.
pub struct LayoutBuilder {
    topo: Arc<Topology>,
    lanes: Vec<u32>,
    expect: VecExpect,
    row: u32,
    chunks: Vec<(u32, Arc<Chunk>)>,
    cur_start: u32,
    cur_state: Box<[u32]>,
    cur_lanes: Vec<u16>,
    cur_width: u16,
    max_lanes: u16,
    reuse: Option<Reuse>,
    reused_from: Option<u32>,
}

impl LayoutBuilder {
    pub fn new(topo: Arc<Topology>) -> Self {
        let n = topo.len();
        LayoutBuilder {
            topo,
            lanes: Vec::new(),
            expect: VecExpect(vec![NO_LANE; n]),
            row: 0,
            chunks: Vec::new(),
            cur_start: 0,
            cur_state: Box::new([]),
            cur_lanes: Vec::with_capacity(CHUNK_ROWS),
            cur_width: 0,
            max_lanes: 0,
            reuse: None,
            reused_from: None,
        }
    }

    /// Like [`LayoutBuilder::new`], but reuses `prev`'s chunks if `topo` is
    /// `prev`'s topology with commits prepended (the common case after a
    /// commit or fetch): only rows until the lane state converges with
    /// `prev` are recomputed. Falls back to a full layout otherwise.
    pub fn with_reuse(topo: Arc<Topology>, prev: &Layout) -> Self {
        let mut b = Self::new(topo);
        let old = prev.topology.oids();
        let new = b.topo.oids();
        if prev.is_complete()
            && prev.topology.order() == b.topo.order()
            && new.len() >= old.len()
            && !old.is_empty()
            && new[new.len() - old.len()..] == *old
        {
            b.reuse = Some(Reuse {
                prev: prev.clone(),
                delta: (new.len() - old.len()) as u32,
                next_chunk: 0,
            });
        }
        b
    }

    pub fn rows_laid(&self) -> u32 {
        self.row
    }

    pub fn is_done(&self) -> bool {
        self.row as usize == self.topo.len()
    }

    fn state_rel(&self, start: u32) -> Box<[u32]> {
        self.lanes
            .iter()
            .map(|&l| if l == FREE { FREE } else { l - start })
            .collect()
    }

    fn close_chunk(&mut self) {
        if self.cur_lanes.is_empty() {
            return;
        }
        let lanes = std::mem::replace(&mut self.cur_lanes, Vec::with_capacity(CHUNK_ROWS));
        let state = std::mem::take(&mut self.cur_state);
        self.chunks.push((
            self.cur_start,
            Arc::new(Chunk {
                lanes,
                start_state: state,
                width: self.cur_width,
            }),
        ));
        self.cur_width = 0;
    }

    /// Checks whether the lane state before `self.row` equals the previous
    /// layout's state at the same commit; if so splices in its chunks.
    fn try_converge(&mut self) -> bool {
        let Some(reuse) = &mut self.reuse else {
            return false;
        };
        let Some(old_row) = self.row.checked_sub(reuse.delta) else {
            return false;
        };
        let chunks = &reuse.prev.chunks;
        while reuse.next_chunk < chunks.len() && chunks[reuse.next_chunk].0 < old_row {
            reuse.next_chunk += 1;
        }
        let Some((old_start, chunk)) = chunks.get(reuse.next_chunk) else {
            self.reuse = None;
            return false;
        };
        if *old_start != old_row {
            return false;
        }
        // Relative encoding: equal relative states mean equal absolute
        // states after shifting by delta.
        let same = chunk.start_state.len() == self.lanes.len()
            && chunk
                .start_state
                .iter()
                .zip(&self.lanes)
                .all(|(&rel, &abs)| {
                    if rel == FREE {
                        abs == FREE
                    } else {
                        abs != FREE && abs - self.row == rel
                    }
                });
        if !same {
            reuse.next_chunk += 1;
            return false;
        }
        let reuse = self.reuse.take().unwrap();
        self.close_chunk();
        for (start, chunk) in &reuse.prev.chunks[reuse.next_chunk..] {
            self.max_lanes = self.max_lanes.max(chunk.width);
            self.chunks.push((start + reuse.delta, chunk.clone()));
        }
        self.reused_from = Some(self.row);
        self.row = self.topo.len() as u32;
        true
    }

    /// Lays out up to `max_rows` more rows. Returns `true` when complete.
    pub fn step(&mut self, max_rows: usize) -> bool {
        let n = self.topo.len() as u32;
        let end = self.row.saturating_add(max_rows as u32).min(n);
        while self.row < end {
            if self.reuse.is_some() && self.try_converge() {
                return true;
            }
            if self.cur_lanes.len() >= CHUNK_ROWS || (self.cur_lanes.is_empty() && self.row == 0) {
                self.close_chunk();
                self.cur_start = self.row;
                self.cur_state = self.state_rel(self.row);
            }
            let row = self.row;
            let (lane, _) = step_row(
                &mut self.lanes,
                &mut self.expect,
                row,
                self.topo.parents(row),
                None,
            );
            self.cur_lanes.push(lane);
            let width = (self.lanes.len() as u16).max(lane + 1);
            self.cur_width = self.cur_width.max(width);
            self.max_lanes = self.max_lanes.max(width);
            self.row += 1;
        }
        self.is_done()
    }

    /// The layout so far. Cheap: shares finished chunks, copies the current
    /// partial chunk (≤ `CHUNK_ROWS` lanes).
    pub fn snapshot(&self) -> Layout {
        let mut chunks: Vec<(u32, Arc<Chunk>)> = self.chunks.clone();
        if !self.cur_lanes.is_empty() {
            chunks.push((
                self.cur_start,
                Arc::new(Chunk {
                    lanes: self.cur_lanes.clone(),
                    start_state: self.cur_state.clone(),
                    width: self.cur_width,
                }),
            ));
        }
        Layout {
            topology: self.topo.clone(),
            chunks: chunks.into(),
            rows_laid: self.row,
            max_lanes: self.max_lanes,
            reused_from: self.reused_from,
        }
    }

    pub fn finish(mut self) -> Layout {
        while !self.step(usize::MAX) {}
        self.close_chunk();
        Layout {
            topology: self.topo,
            chunks: self.chunks.into(),
            rows_laid: self.row,
            max_lanes: self.max_lanes,
            reused_from: self.reused_from,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GraphOrder;
    use fgc_core::{CommitDag, Oid};

    fn o(i: u32) -> Oid {
        let mut b = [0u8; 20];
        b[..4].copy_from_slice(&i.to_be_bytes());
        b[19] = 1;
        Oid(b)
    }

    fn topo(commits: &[(u32, i64, Vec<u32>)]) -> Arc<Topology> {
        let parents: Vec<Vec<Oid>> = commits
            .iter()
            .map(|c| c.2.iter().map(|&p| o(p)).collect())
            .collect();
        let dag = CommitDag::from_commits(
            commits
                .iter()
                .zip(&parents)
                .map(|(c, p)| (o(c.0), c.1, p.as_slice())),
        );
        Arc::new(Topology::from_dag(dag, GraphOrder::Date))
    }

    #[test]
    fn linear_history_is_one_lane() {
        let t = topo(&[(3, 30, vec![2]), (2, 20, vec![1]), (1, 10, vec![])]);
        let l = LayoutBuilder::new(t).finish();
        let rows = l.rows(0..3);
        assert_eq!(l.max_lanes(), 1);
        assert!(rows.iter().all(|r| r.lane == 0 && r.passthrough.is_empty()));
        assert!(!rows[0].has_incoming && rows[1].has_incoming);
        assert_eq!(rows[0].outgoing, vec![0]);
        assert!(rows[2].outgoing.is_empty());
    }

    #[test]
    fn merge_and_fork() {
        // m(50) merges b(40) and c(45); both fork from a(10).
        let t = topo(&[
            (5, 50, vec![4, 3]),
            (4, 40, vec![1]),
            (3, 45, vec![1]),
            (1, 10, vec![]),
        ]);
        // rows: m, c, b, a
        let l = LayoutBuilder::new(t).finish();
        let r = l.rows(0..4);
        assert_eq!((r[0].lane, r[0].outgoing.clone()), (0, vec![0, 1]));
        // c: in lane 1, passthrough lane 0 (expecting b).
        assert_eq!((r[1].lane, r[1].passthrough.clone()), (1, vec![0]));
        // c's parent a not yet expected → continues in lane 1.
        assert_eq!(r[1].outgoing, vec![1]);
        // b in lane 0; its parent a is already expected by lane 1 → joins it.
        assert_eq!(
            (r[2].lane, r[2].outgoing.clone(), r[2].passthrough.clone()),
            (0, vec![1], vec![1])
        );
        // a in lane 1, nothing else open.
        assert_eq!((r[3].lane, r[3].passthrough.clone()), (1, vec![]));
        assert_eq!(l.max_lanes(), 2);
    }

    /// Random-ish DAG with branches and merges, times decreasing with id.
    fn synthetic(n: u32, seed: u64) -> Vec<(u32, i64, Vec<u32>)> {
        let mut s = seed | 1;
        let mut rnd = move |m: u32| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s % m as u64) as u32
        };
        // Commit i's parents have larger ids (older).
        (0..n)
            .map(|i| {
                let mut parents = Vec::new();
                if i + 1 < n {
                    parents.push(i + 1 + rnd(4.min(n - i - 1)));
                    if rnd(6) == 0 && i + 2 < n {
                        let p = i + 1 + rnd((n - i - 1).min(40));
                        if !parents.contains(&p) {
                            parents.push(p);
                        }
                    }
                }
                (i, (n - i) as i64 * 10, parents)
            })
            .collect()
    }

    fn all_rows(l: &Layout) -> Vec<RowGeometry> {
        l.rows(0..l.rows_laid())
    }

    #[test]
    fn invariants_on_synthetic_graph() {
        let t = topo(&synthetic(5000, 7));
        let l = LayoutBuilder::new(t.clone()).finish();
        assert!(l.is_complete());
        let rows = all_rows(&l);
        assert_eq!(rows.len(), 5000);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.row, i as u32);
            assert_eq!(Some(r.lane), l.lane(r.row));
            assert_eq!(r.outgoing.len(), t.parents(r.row).len());
            assert!(!r.passthrough.contains(&r.lane) || !r.has_incoming);
            // The lanes entering the next row are exactly passthroughs + outgoing.
            if let Some(next) = rows.get(i + 1) {
                let mut below: Vec<u16> =
                    r.passthrough.iter().chain(&r.outgoing).copied().collect();
                below.sort_unstable();
                below.dedup();
                let mut above: Vec<u16> = next.passthrough.clone();
                if next.has_incoming {
                    above.push(next.lane);
                }
                above.sort_unstable();
                assert_eq!(below, above, "row {i}");
            }
        }
        // Geometry of any window equals the full replay.
        for start in [0u32, 1, 1023, 1024, 1025, 2047, 4990] {
            let w = l.rows(start..start + 10);
            assert_eq!(
                w,
                rows[start as usize..(start as usize + 10).min(5000)].to_vec()
            );
        }
    }

    #[test]
    fn stepping_and_snapshots_match_finish() {
        let t = topo(&synthetic(3000, 11));
        let full = all_rows(&LayoutBuilder::new(t.clone()).finish());
        let mut b = LayoutBuilder::new(t);
        let mut steps = 0;
        while !b.step(337) {
            let snap = b.snapshot();
            assert_eq!(snap.rows_laid(), b.rows_laid());
            let laid = snap.rows_laid() as usize;
            assert_eq!(all_rows(&snap), full[..laid].to_vec());
            steps += 1;
        }
        assert!(steps > 5);
        assert_eq!(all_rows(&b.finish()), full);
    }

    #[test]
    fn reuse_after_new_commits_matches_full_layout() {
        let base = synthetic(6000, 3);
        // 25 new commits on top of the newest commit (id 0). Existing
        // commits keep their oids, as in a real repository.
        let k = 25u32;
        let mut grown: Vec<(u32, i64, Vec<u32>)> = (0..k)
            .map(|i| {
                let parent = if i + 1 < k { 100_000 + i + 1 } else { 0 };
                (100_000 + i, 100_000 + (k - i) as i64, vec![parent])
            })
            .collect();
        grown.extend(base.iter().cloned());

        let old = LayoutBuilder::new(topo(&base)).finish();
        let new_topo = topo(&grown);
        let reused = LayoutBuilder::with_reuse(new_topo.clone(), &old).finish();
        let fresh = LayoutBuilder::new(new_topo).finish();
        let from = reused
            .reused_from()
            .expect("layout should converge and reuse chunks");
        assert!(from < 3000, "converged late: {from}");
        assert_eq!(all_rows(&reused), all_rows(&fresh));
        assert_eq!(reused.max_lanes(), fresh.max_lanes());
    }

    #[test]
    fn reuse_falls_back_when_history_rewritten() {
        let a = topo(&synthetic(2000, 5));
        // Different commits (e.g. after a force-push): different oids.
        let rewritten: Vec<_> = synthetic(2000, 6)
            .into_iter()
            .map(|(i, t, p)| (i + 50_000, t, p.into_iter().map(|x| x + 50_000).collect()))
            .collect();
        let b = topo(&rewritten);
        let old = LayoutBuilder::new(a).finish();
        let l = LayoutBuilder::with_reuse(b.clone(), &old).finish();
        assert_eq!(l.reused_from(), None);
        assert_eq!(all_rows(&l), all_rows(&LayoutBuilder::new(b).finish()));
    }

    #[test]
    fn empty_topology() {
        let t = topo(&[]);
        let l = LayoutBuilder::new(t).finish();
        assert!(l.is_complete());
        assert!(l.rows(0..10).is_empty());
        assert_eq!(l.lane(0), None);
    }
}
