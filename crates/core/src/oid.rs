use std::fmt;
use std::str::FromStr;

use crate::Error;

/// A SHA-1 object id.
///
/// Kept as plain bytes (`Copy`, 20 bytes) so snapshots can hold millions of
/// them cheaply. SHA-256 repositories are out of scope until libgit2 support
/// stabilises; when it does this becomes an enum or a 32-byte array.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub const ZERO: Oid = Oid([0; 20]);

    pub fn from_bytes(bytes: &[u8]) -> Option<Oid> {
        bytes.try_into().ok().map(Oid)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 20]
    }

    /// Abbreviated hex form (first `len` hex digits, max 40).
    pub fn short(&self, len: usize) -> String {
        let mut s = self.to_string();
        s.truncate(len.min(40));
        s
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.short(12))
    }
}

impl FromStr for Oid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || Error::InvalidInput(format!("not a 40-char hex object id: {s:?}"));
        if s.len() != 40 {
            return Err(bad());
        }
        let mut out = [0u8; 20];
        for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
            let hi = hex_val(chunk[0]).ok_or_else(bad)?;
            let lo = hex_val(chunk[1]).ok_or_else(bad)?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Oid(out))
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let s = "0123456789abcdef0123456789abcdef01234567";
        let oid: Oid = s.parse().unwrap();
        assert_eq!(oid.to_string(), s);
        assert_eq!(oid.short(7), "0123456");
        assert!("xyz".parse::<Oid>().is_err());
        assert!(
            "g123456789abcdef0123456789abcdef01234567"
                .parse::<Oid>()
                .is_err()
        );
    }
}
