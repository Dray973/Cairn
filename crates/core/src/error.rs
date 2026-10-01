//! Unified error type for the core engine.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("this operation requires an elevated (Administrator) process")]
    NotElevated,

    #[error("Win32 error: {0}")]
    Win32(#[from] windows::core::Error),

    #[error("System Restore error {code}: {message}")]
    RestorePoint { code: u32, message: String },

    #[error("service not found: {0}")]
    ServiceNotFound(String),

    #[error("invalid registry path: {0}")]
    InvalidPath(String),

    #[error("state journal error: {0}")]
    Journal(#[from] rusqlite::Error),

    /// The journal was written by a newer schema than this build reads; it was not changed.
    #[error(
        "This change history was written by a newer version of Cairn (journal schema {found}; \
         this version reads up to {supported}). Update Cairn to use it; nothing was changed."
    )]
    JournalTooNew { found: i64, supported: i64 },

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("PowerShell interop failed (exit {code}): {stderr}")]
    PowerShell { code: i32, stderr: String },

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Extracts the underlying Win32 error code when this error wraps one
    /// (an `HRESULT` of the form `0x8007xxxx`).
    pub fn win32_code(&self) -> Option<u32> {
        match self {
            Error::Win32(e) => {
                let hr = e.code().0 as u32;
                if hr & 0xFFFF_0000 == 0x8007_0000 {
                    Some(hr & 0xFFFF)
                } else {
                    None
                }
            }
            Error::RestorePoint { code, .. } => Some(*code),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
