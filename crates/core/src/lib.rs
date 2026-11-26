//! Core of the fast git client.
//!
//! This crate is deliberately UI-agnostic and engine-agnostic: it defines the
//! data model (immutable snapshots) and the [`GitBackend`] trait. Concrete
//! engines (libgit2, gitoxide, git CLI) live in other crates and are routed per
//! operation behind the trait.
//!
//! Hard rule: nothing in here may be called from a UI render or event handler.
//! The UI talks to a repo actor, which talks to a backend on a worker thread.

pub mod backend;
pub mod commit;
pub mod dag;
pub mod diff;
pub mod error;
pub mod oid;
pub mod perf;
pub mod refs;
pub mod status;
pub mod write;

pub use backend::GitBackend;
pub use commit::CommitMeta;
pub use dag::{CommitDag, DagStats};
pub use diff::{Diff, DiffLine, DiffSpec, DiffTarget, FileChange, FileDiff, Hunk, LineKind};
pub use error::{Error, Result};
pub use oid::Oid;
pub use refs::{RefInfo, RefKind, RepoInfo};
pub use status::{Change, StatusEntry, StatusOpts, StatusSnapshot};
pub use write::{HunkSel, NetworkOp};
