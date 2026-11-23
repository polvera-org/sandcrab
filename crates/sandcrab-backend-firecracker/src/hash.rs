//! SHA-256 helpers.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, IoCtx, Result};

/// Lowercase hex encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Hex SHA-256 of a byte slice.
pub fn sha256_bytes(data: &[u8]) -> String {
    to_hex(&Sha256::digest(data))
}

/// Hex SHA-256 of a file's contents (streamed).
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).at(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).at(path)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(to_hex(&h.finalize()))
}

/// Verify a file against an expected hex digest (an optional `sha256:` prefix is ignored).
pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let expected = expected.strip_prefix("sha256:").unwrap_or(expected);
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(Error::DigestMismatch {
            what: path.display().to_string(),
            expected: expected.to_string(),
            actual,
        })
    }
}

/// A writer that hashes and counts everything passing through.
pub struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            written: 0,
        }
    }
    pub fn written(&self) -> u64 {
        self.written
    }
    pub fn finish(self) -> (W, String, u64) {
        (self.inner, to_hex(&self.hasher.finalize()), self.written)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_accepts_prefix_and_rejects_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        verify_sha256(
            &p,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();
        assert!(matches!(
            verify_sha256(&p, "00"),
            Err(Error::DigestMismatch { .. })
        ));
    }
}
