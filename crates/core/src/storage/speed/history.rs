//! Earlier speed-test results: `<storage data dir>\speed_history.json`, newest first.
//!
//! The file is `{"version": 1, "results": [SpeedTestResult, …]}` holding at most
//! [`HISTORY_LIMIT`] results. A write goes to a `.tmp` file that then replaces the history,
//! and one process-wide lock serializes writers. A file that cannot be parsed makes reads
//! fail; the next write moves it aside as `.bad` and starts a new history.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::SpeedTestResult;
use crate::Result;

/// File name of the history in the storage data folder.
pub const HISTORY_FILE: &str = "speed_history.json";
/// Results kept.
pub const HISTORY_LIMIT: usize = 50;
const VERSION: u32 = 1;

static WRITER: Mutex<()> = Mutex::new(());

#[derive(Debug, Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    results: Vec<SpeedTestResult>,
}

fn history_path(dir: &Path) -> PathBuf {
    dir.join(HISTORY_FILE)
}

/// The earlier results in `dir`, newest first; empty when there is no history yet.
pub fn speed_history(dir: &Path) -> Result<Vec<SpeedTestResult>> {
    let path = history_path(dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let file: HistoryFile = serde_json::from_str(&text)?;
    Ok(file.results)
}

/// Adds `result` as the newest entry of the history in `dir`, keeping the newest
/// [`HISTORY_LIMIT`].
pub(crate) fn append_history(dir: &Path, result: &SpeedTestResult) -> Result<()> {
    let _writer = WRITER.lock().unwrap_or_else(|p| p.into_inner());
    fs::create_dir_all(dir)?;
    let path = history_path(dir);
    let mut results = match speed_history(dir) {
        Ok(results) => results,
        Err(e) => {
            tracing::warn!(error = %e, "the speed-test history is unreadable; starting a new one");
            let bad = dir.join(format!("{HISTORY_FILE}.bad"));
            if let Err(e) = fs::rename(&path, &bad) {
                tracing::warn!(error = %e, "cannot move the unreadable speed-test history aside");
            }
            Vec::new()
        }
    };
    results.insert(0, result.clone());
    results.truncate(HISTORY_LIMIT);
    let text = serde_json::to_string_pretty(&HistoryFile {
        version: VERSION,
        results,
    })?;
    let tmp = dir.join(format!("{HISTORY_FILE}.tmp"));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::speed::tests::sample_result;

    #[test]
    fn a_missing_history_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(speed_history(dir.path()).unwrap().is_empty());
        assert!(speed_history(&dir.path().join("missing"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn results_are_kept_newest_first_up_to_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("storage");
        for i in 0..(HISTORY_LIMIT + 5) {
            let mut result = sample_result();
            result.elapsed_ms = i as u64;
            append_history(&store, &result).unwrap();
        }
        let results = speed_history(&store).unwrap();
        assert_eq!(results.len(), HISTORY_LIMIT);
        assert_eq!(results[0].elapsed_ms, (HISTORY_LIMIT + 4) as u64);
        assert_eq!(results[HISTORY_LIMIT - 1].elapsed_ms, 5);
        assert!(!store.join(format!("{HISTORY_FILE}.tmp")).exists());
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(store.join(HISTORY_FILE)).unwrap()).unwrap();
        assert_eq!(raw["version"], 1);
    }

    #[test]
    fn a_corrupt_history_fails_to_read_and_is_replaced_on_write() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(HISTORY_FILE), b"{ not json").unwrap();
        assert!(matches!(
            speed_history(dir.path()),
            Err(crate::Error::Serde(_))
        ));
        append_history(dir.path(), &sample_result()).unwrap();
        assert_eq!(speed_history(dir.path()).unwrap().len(), 1);
        assert_eq!(
            fs::read(dir.path().join(format!("{HISTORY_FILE}.bad"))).unwrap(),
            b"{ not json"
        );
    }

    #[test]
    fn a_write_replaces_the_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        append_history(dir.path(), &sample_result()).unwrap();
        // A stale temporary file from an interrupted write is simply replaced.
        fs::write(dir.path().join(format!("{HISTORY_FILE}.tmp")), b"partial").unwrap();
        append_history(dir.path(), &sample_result()).unwrap();
        assert_eq!(speed_history(dir.path()).unwrap().len(), 2);
        assert!(!dir.path().join(format!("{HISTORY_FILE}.tmp")).exists());
    }
}
