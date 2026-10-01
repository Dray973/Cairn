//! SHA-256 and random bytes through Windows CNG (the BCrypt pseudo-handles, Windows 10 and
//! later).

use std::fmt;

use windows::Win32::Foundation::NTSTATUS;
use windows::Win32::Security::Cryptography::{
    BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash, BCryptGenRandom, BCryptHashData,
    BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};

use crate::{Error, Result};

/// Length of a SHA-256 digest in bytes.
pub(crate) const DIGEST_LEN: usize = 32;

/// Largest slice handed to one BCrypt call, whose length parameters are 32-bit.
const MAX_CALL: usize = 1 << 30;

fn status(code: NTSTATUS, what: &str) -> Result<()> {
    if code.is_ok() {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "{what} failed (NTSTATUS 0x{:08X})",
            code.0 as u32
        )))
    }
}

/// A running SHA-256 computation.
pub(crate) struct Sha256 {
    handle: BCRYPT_HASH_HANDLE,
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sha256").finish_non_exhaustive()
    }
}

// SAFETY: a CNG hash object may be used from any thread as long as calls on it do not
// overlap, which `&mut self` on every call guarantees.
unsafe impl Send for Sha256 {}

impl Sha256 {
    pub(crate) fn new() -> Result<Sha256> {
        let mut handle = BCRYPT_HASH_HANDLE::default();
        // SAFETY: `handle` is a valid out pointer; the pseudo-handle needs no provider handle,
        // and CNG allocates the hash object itself when no buffer is passed.
        let code =
            unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut handle, None, None, 0) };
        status(code, "BCryptCreateHash")?;
        Ok(Sha256 { handle })
    }

    pub(crate) fn update(&mut self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(MAX_CALL) {
            // SAFETY: the handle is a live hash object owned by `self`; the chunk outlives
            // the call.
            let code = unsafe { BCryptHashData(self.handle, chunk, 0) };
            status(code, "BCryptHashData")?;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<[u8; DIGEST_LEN]> {
        let mut digest = [0u8; DIGEST_LEN];
        // SAFETY: the handle is a live hash object owned by `self`; `digest` is exactly the
        // SHA-256 output length.
        let code = unsafe { BCryptFinishHash(self.handle, &mut digest, 0) };
        status(code, "BCryptFinishHash")?;
        Ok(digest)
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: the handle was created by BCryptCreateHash and is destroyed once, here.
            unsafe {
                let _ = BCryptDestroyHash(self.handle);
            }
        }
    }
}

/// SHA-256 of `data` in one call.
#[cfg(test)]
pub(crate) fn sha256(data: &[u8]) -> Result<[u8; DIGEST_LEN]> {
    use windows::Win32::Security::Cryptography::BCryptHash;
    if data.len() > MAX_CALL {
        let mut hash = Sha256::new()?;
        hash.update(data)?;
        return hash.finish();
    }
    let mut digest = [0u8; DIGEST_LEN];
    // SAFETY: both slices outlive the call; the output is exactly the SHA-256 length.
    let code = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut digest) };
    status(code, "BCryptHash")?;
    Ok(digest)
}

/// Fills `buf` with cryptographically random bytes.
pub(crate) fn random_fill(buf: &mut [u8]) -> Result<()> {
    for chunk in buf.chunks_mut(MAX_CALL) {
        // SAFETY: the chunk is writable for its whole length for the duration of the call.
        let code = unsafe { BCryptGenRandom(None, chunk, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
        status(code, "BCryptGenRandom")?;
    }
    Ok(())
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(hex(&sha256(b"").unwrap()), EMPTY);
        assert_eq!(hex(&sha256(b"abc").unwrap()), ABC);
        assert_eq!(hex(&Sha256::new().unwrap().finish().unwrap()), EMPTY);
    }

    #[test]
    fn streaming_equals_one_shot() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut hash = Sha256::new().unwrap();
        for chunk in data.chunks(7_777) {
            hash.update(chunk).unwrap();
        }
        assert_eq!(hash.finish().unwrap(), sha256(&data).unwrap());
        let mut abc = Sha256::new().unwrap();
        abc.update(b"a").unwrap();
        abc.update(b"").unwrap();
        abc.update(b"bc").unwrap();
        assert_eq!(hex(&abc.finish().unwrap()), ABC);
    }

    #[test]
    fn random_fill_fills_the_buffer() {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        random_fill(&mut a).unwrap();
        random_fill(&mut b).unwrap();
        assert_ne!(a, [0u8; 64]);
        assert_ne!(a, b);
        random_fill(&mut []).unwrap();
    }
}
