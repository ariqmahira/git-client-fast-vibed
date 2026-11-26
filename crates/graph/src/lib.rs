//! Commit graph engine (milestone M2).
//!
//! Pipeline, all off the UI thread:
//!
//! 1. The backend returns the full [`fgc_core::CommitDag`] (commit-graph
//!    file → milliseconds even for linux.git).
//! 2. [`Topology`] sorts it into display rows (date or topo order, children
//!    always above parents).
//! 3. [`LayoutBuilder`] assigns lanes incrementally in chunks. Only the lane
//!    of each row plus a lane-state checkpoint per chunk is stored (~2 bytes
//!    per commit); [`Layout::rows`] rebuilds edge geometry for the visible
//!    window from the nearest checkpoint in microseconds.
//! 4. [`GraphEngine`] drives this on a background thread and publishes
//!    immutable [`GraphSnapshot`]s: first screen as soon as its rows are laid
//!    out, then progressively, with cancellation via generation ids. When
//!    new commits appear, the previous layout's chunks are reused as soon as
//!    the lane state converges, so only the new prefix is recomputed.

mod engine;
mod layout;
mod topology;

pub use engine::{EngineOptions, GraphEngine, GraphEvent, GraphSnapshot, LoadStats, Phase};
pub use layout::{CHUNK_ROWS, Layout, LayoutBuilder, RowGeometry};
pub use topology::{GraphOrder, Topology};
