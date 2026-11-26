//! Commit-graph lane layout. Milestone M2; intentionally empty until then.
//!
//! Plan: process commits in topological order keeping active-lane state,
//! emit rows in chunks, cache the layout keyed by the set of ref tips, and
//! recompute only the new prefix when commits arrive.
