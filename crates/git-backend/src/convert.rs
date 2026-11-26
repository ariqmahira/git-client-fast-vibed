use std::path::{Path, PathBuf};

use fgc_core::{Error, Oid};

pub(crate) fn to_core(oid: git2::Oid) -> Oid {
    Oid::from_bytes(oid.as_bytes()).expect("SHA-1 oid is 20 bytes")
}

pub(crate) fn to_git(oid: Oid) -> git2::Oid {
    git2::Oid::from_bytes(oid.as_bytes()).expect("20-byte oid")
}

pub(crate) fn map_err(e: git2::Error) -> Error {
    match e.code() {
        git2::ErrorCode::NotFound => Error::NotFound(e.message().to_owned()),
        git2::ErrorCode::InvalidSpec | git2::ErrorCode::Ambiguous => {
            Error::InvalidInput(e.message().to_owned())
        }
        _ => Error::Engine(e.message().to_owned()),
    }
}

/// Git stores paths as bytes. On Unix they map to `OsStr` losslessly.
#[cfg(unix)]
pub(crate) fn bytes_to_path(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(not(unix))]
pub(crate) fn bytes_to_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

#[cfg(unix)]
pub(crate) fn path_to_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
pub(crate) fn path_to_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().replace('\\', "/").into_bytes()
}
