use crate::Oid;

/// Metadata needed to draw one commit row. Built by the backend, immutable.
///
/// Only the summary line is kept; the full message is fetched on demand when
/// a commit is selected. Note for M2: the graph engine must not keep a
/// `CommitMeta` for every commit of a 1.3M-commit repo (memory budget); it
/// keeps oid/parents/time and pages metadata for visible rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    pub oid: Oid,
    pub parents: Vec<Oid>,
    pub summary: Box<str>,
    pub author_name: Box<str>,
    pub author_email: Box<str>,
    /// Seconds since the Unix epoch.
    pub author_time: i64,
    /// Author timezone offset in minutes.
    pub author_tz_minutes: i32,
    /// Seconds since the Unix epoch. Used for walk ordering.
    pub commit_time: i64,
}

impl CommitMeta {
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}
