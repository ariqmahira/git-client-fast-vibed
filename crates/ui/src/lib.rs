//! GPUI shell (M3): window, virtualized commit graph, file list, diff view.
//!
//! The UI thread renders immutable snapshots and sends commands to the
//! [`fgc_actor::RepoActor`]; it never calls Git.

pub mod app;
pub mod bench;
pub mod diff_doc;
pub mod graph_paint;
pub mod perf;
pub mod text_input;
pub mod theme;
pub mod util;
pub mod view;
