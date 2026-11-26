//! Minimal reader for Git's commit-graph file format (SHA-1).
//!
//! <https://git-scm.com/docs/gitformat-commit-graph>
//!
//! Only what the graph view needs: oid ↔ position, commit time and parents.
//! Every chunk offset and size is validated on open and every parent
//! position on access, so a corrupt file yields an error (and the caller
//! falls back to parsing commits) instead of a panic.
//!
//! Files are memory-mapped. Git replaces commit-graph files by rename, never
//! in place, so a mapping of the old file stays valid while it is in use.

use std::fs::File;
use std::path::Path;

use fgc_core::{Error, Oid, Result};
use memmap2::Mmap;

const HASH_LEN: usize = 20;
const CDAT_LEN: usize = HASH_LEN + 16;
const NO_PARENT: u32 = 0x7000_0000;
const EXTRA_EDGES: u32 = 0x8000_0000;
const LAST_EDGE: u32 = 0x8000_0000;

fn corrupt(path: &Path, what: &str) -> Error {
    Error::Engine(format!("corrupt commit-graph {}: {what}", path.display()))
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

struct GraphFile {
    map: Mmap,
    /// Global position of this file's first commit (sum of base files).
    base: u32,
    n: u32,
    fanout: usize,
    oidl: usize,
    cdat: usize,
    edges: Option<(usize, usize)>,
}

impl GraphFile {
    fn open(path: &Path, base: u32, expected_bases: usize) -> Result<GraphFile> {
        let file = File::open(path)?;
        // SAFETY: see module docs; git never modifies commit-graph files in place.
        let map = unsafe { Mmap::map(&file)? };
        let data = &map[..];
        let err = |w: &str| corrupt(path, w);
        if data.len() < 8 + 12 + HASH_LEN || &data[0..4] != b"CGPH" {
            return Err(err("bad header"));
        }
        if data[4] != 1 {
            return Err(Error::Unsupported(format!(
                "commit-graph version {}",
                data[4]
            )));
        }
        if data[5] != 1 {
            return Err(Error::Unsupported("SHA-256 commit-graph".into()));
        }
        let n_chunks = data[6] as usize;
        if data[7] as usize != expected_bases {
            return Err(err("base graph count does not match chain"));
        }
        let table_end = 8 + (n_chunks + 1) * 12;
        if data.len() < table_end + HASH_LEN {
            return Err(err("truncated chunk table"));
        }
        let content_end = data.len() - HASH_LEN;
        let mut chunks: Vec<([u8; 4], usize, usize)> = Vec::with_capacity(n_chunks);
        for i in 0..n_chunks {
            let e = 8 + i * 12;
            let id: [u8; 4] = data[e..e + 4].try_into().unwrap();
            let off = u64::from_be_bytes(data[e + 4..e + 12].try_into().unwrap()) as usize;
            let next = u64::from_be_bytes(data[e + 16..e + 24].try_into().unwrap()) as usize;
            if off < table_end || next < off || next > content_end {
                return Err(err("chunk out of bounds"));
            }
            chunks.push((id, off, next - off));
        }
        let find = |id: &[u8; 4]| chunks.iter().find(|c| &c.0 == id).map(|c| (c.1, c.2));
        let (fanout, fanout_len) = find(b"OIDF").ok_or_else(|| err("missing OIDF"))?;
        let (oidl, oidl_len) = find(b"OIDL").ok_or_else(|| err("missing OIDL"))?;
        let (cdat, cdat_len) = find(b"CDAT").ok_or_else(|| err("missing CDAT"))?;
        if fanout_len != 256 * 4 {
            return Err(err("bad OIDF size"));
        }
        let n = be32(data, fanout + 255 * 4);
        let mut prev = 0;
        for i in 0..256 {
            let f = be32(data, fanout + i * 4);
            if f < prev {
                return Err(err("fanout not monotonic"));
            }
            prev = f;
        }
        if oidl_len != n as usize * HASH_LEN || cdat_len != n as usize * CDAT_LEN {
            return Err(err("chunk size does not match commit count"));
        }
        let edges = find(b"EDGE");
        if edges.is_some_and(|(_, len)| len % 4 != 0) {
            return Err(err("bad EDGE size"));
        }
        base.checked_add(n).ok_or_else(|| err("too many commits"))?;
        Ok(GraphFile {
            map,
            base,
            n,
            fanout,
            oidl,
            cdat,
            edges,
        })
    }

    fn data(&self) -> &[u8] {
        &self.map
    }

    fn oid_bytes(&self, local: u32) -> &[u8] {
        let at = self.oidl + local as usize * HASH_LEN;
        &self.data()[at..at + HASH_LEN]
    }

    fn lookup(&self, oid: &Oid) -> Option<u32> {
        let d = self.data();
        let first = oid.0[0] as usize;
        let lo = if first == 0 {
            0
        } else {
            be32(d, self.fanout + (first - 1) * 4)
        };
        let hi = be32(d, self.fanout + first * 4);
        let (mut lo, mut hi) = (lo, hi);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.oid_bytes(mid).cmp(&oid.0[..]) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
}

/// A commit-graph: a single file or a split chain, addressed by global
/// position (base files first).
pub(crate) struct CommitGraph {
    files: Vec<GraphFile>,
    total: u32,
}

impl CommitGraph {
    /// Opens `objects/info/commit-graph` or the split chain under
    /// `objects/info/commit-graphs/`. `Ok(None)` if neither exists.
    pub(crate) fn open(info_dir: &Path) -> Result<Option<CommitGraph>> {
        let chain = info_dir.join("commit-graphs").join("commit-graph-chain");
        let mut files = Vec::new();
        let mut total = 0u32;
        if chain.is_file() {
            let text = std::fs::read_to_string(&chain)?;
            for (i, hash) in text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .enumerate()
            {
                if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(corrupt(&chain, "bad chain entry"));
                }
                let path = info_dir
                    .join("commit-graphs")
                    .join(format!("graph-{hash}.graph"));
                let f = GraphFile::open(&path, total, i)?;
                total += f.n;
                files.push(f);
            }
        } else {
            let single = info_dir.join("commit-graph");
            if !single.is_file() {
                return Ok(None);
            }
            let f = GraphFile::open(&single, 0, 0)?;
            total = f.n;
            files.push(f);
        }
        if files.is_empty() {
            return Ok(None);
        }
        Ok(Some(CommitGraph { files, total }))
    }

    pub(crate) fn num_commits(&self) -> u32 {
        self.total
    }

    fn file(&self, pos: u32) -> (&GraphFile, u32) {
        if self.files.len() == 1 {
            return (&self.files[0], pos);
        }
        let i = self.files.partition_point(|f| f.base <= pos) - 1;
        let f = &self.files[i];
        (f, pos - f.base)
    }

    pub(crate) fn lookup(&self, oid: &Oid) -> Option<u32> {
        // Newest layers are most likely to hold recent commits.
        self.files
            .iter()
            .rev()
            .find_map(|f| f.lookup(oid).map(|l| f.base + l))
    }

    pub(crate) fn oid(&self, pos: u32) -> Oid {
        let (f, local) = self.file(pos);
        Oid::from_bytes(f.oid_bytes(local)).expect("20 bytes")
    }

    /// Commit time and parents of `pos`; parents are appended to `out`.
    /// `pos` must be `< num_commits()`.
    pub(crate) fn commit(&self, pos: u32, out: &mut Vec<u32>) -> Result<i64> {
        let (f, local) = self.file(pos);
        let d = f.data();
        let rec = f.cdat + local as usize * CDAT_LEN + HASH_LEN;
        let p1 = be32(d, rec);
        let p2 = be32(d, rec + 4);
        let hi = be32(d, rec + 8) & 0x3;
        let lo = be32(d, rec + 12);
        let time = ((hi as i64) << 32) | lo as i64;
        let bad = || corrupt(Path::new("commit-graph"), "parent out of range");
        if p1 != NO_PARENT {
            if p1 >= self.total {
                return Err(bad());
            }
            out.push(p1);
        }
        if p2 != NO_PARENT {
            if p2 & EXTRA_EDGES == 0 {
                if p2 >= self.total {
                    return Err(bad());
                }
                out.push(p2);
            } else {
                let (off, len) = f.edges.ok_or_else(bad)?;
                let mut i = (p2 & !EXTRA_EDGES) as usize;
                loop {
                    if (i + 1) * 4 > len {
                        return Err(bad());
                    }
                    let e = be32(d, off + i * 4);
                    let p = e & !LAST_EDGE;
                    if p >= self.total {
                        return Err(bad());
                    }
                    out.push(p);
                    if e & LAST_EDGE != 0 {
                        break;
                    }
                    i += 1;
                }
            }
        }
        Ok(time)
    }
}
