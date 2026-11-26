use std::path::PathBuf;

/// Engine-neutral error. Backends map their native errors into this so that
/// engine types never leak into the UI layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("operation not supported by this repository: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Engine(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
