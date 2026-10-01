//! The verdict of `sfc /verifyonly`, independent of the display language.
//!
//! System File Checker prints its verdict only as localized text. Its message table
//! (RT_MESSAGETABLE in `sfc.exe` and its MUI files) holds that text under stable ids, so the
//! output is matched against the messages loaded in the current UI language and in English.
//! The module is loaded as a data file only; no code of it runs.

use std::path::Path;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::Diagnostics::Debug::{
    FormatMessageW, FORMAT_MESSAGE_FROM_HMODULE, FORMAT_MESSAGE_IGNORE_INSERTS,
};
use windows::Win32::System::LibraryLoader::{
    LoadLibraryExW, LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE,
};

use super::run::StepOutcome;
use crate::tools::{classify, ToolId};
use crate::win::wide;

/// "Windows Resource Protection did not find any integrity violations."
pub(crate) const NO_VIOLATIONS: u32 = 0x4000_100A;
/// "Windows Resource Protection found integrity violations."
pub(crate) const VIOLATIONS: u32 = 0x4000_100B;
/// No integrity violations, but the component store's metadata is damaged.
pub(crate) const METADATA_CORRUPT: u32 = 0x4000_1015;
/// A repair is waiting for a restart.
pub(crate) const REPAIR_PENDING: u32 = 0x4000_100D;
/// Another servicing operation is running.
pub(crate) const BUSY: u32 = 0x4000_1013;
/// "Windows Resource Protection could not perform the requested operation."
pub(crate) const COULD_NOT_PERFORM: u32 = 0x4000_1007;
/// "Windows Resource Protection could not start the repair service."
pub(crate) const SERVICE_FAILED: u32 = 0x4000_100E;
/// "You must be an administrator …"
pub(crate) const NOT_ADMIN: u32 = 0x4000_1005;

const IDS: [u32; 8] = [
    NO_VIOLATIONS,
    VIOLATIONS,
    METADATA_CORRUPT,
    REPAIR_PENDING,
    BUSY,
    COULD_NOT_PERFORM,
    SERVICE_FAILED,
    NOT_ADMIN,
];

/// U.S. English.
const LANG_EN_US: u32 = 0x0409;

/// The first line of each verdict message, normalized, in every language loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SfcMessages {
    pub(crate) first_lines: Vec<(u32, String)>,
}

impl SfcMessages {
    /// The verdict messages of `<system_dir>\sfc.exe` in the current UI language and in
    /// English. Empty when the module or its messages cannot be read.
    pub(crate) fn load(system_dir: &Path) -> SfcMessages {
        let path = wide(&system_dir.join("sfc.exe").to_string_lossy());
        // SAFETY: `path` is NUL-terminated; the module is mapped as a data file and image
        // resource only, so none of its code runs; it is freed below.
        let module = match unsafe {
            LoadLibraryExW(
                PCWSTR(path.as_ptr()),
                None,
                LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
            )
        } {
            Ok(module) => module,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read System File Checker's messages");
                return SfcMessages::default();
            }
        };
        let mut messages = SfcMessages::default();
        for lang in [0, LANG_EN_US] {
            for id in IDS {
                if let Some(text) = format_message(module, id, lang) {
                    messages.push(id, &text);
                }
            }
        }
        // SAFETY: `module` was loaded above and is freed exactly once.
        unsafe {
            let _ = FreeLibrary(module);
        }
        messages
    }

    /// Messages given as text, for tests and fixed fallbacks.
    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(u32, &str)]) -> SfcMessages {
        let mut messages = SfcMessages::default();
        for (id, text) in pairs {
            messages.push(*id, text);
        }
        messages
    }

    fn push(&mut self, id: u32, text: &str) {
        let line = first_line(text);
        if !line.is_empty() && !self.first_lines.iter().any(|(i, l)| *i == id && *l == line) {
            self.first_lines.push((id, line));
        }
    }

    fn id_of(&self, line: &str) -> Option<u32> {
        self.first_lines
            .iter()
            .find(|(_, l)| l == line)
            .map(|(id, _)| *id)
    }
}

/// Message `id` of `module` in language `lang` (0: the default language search).
fn format_message(module: HMODULE, id: u32, lang: u32) -> Option<String> {
    let mut buf = vec![0u16; 4096];
    // SAFETY: `module` is a loaded module handle; `buf` is writable for its full length,
    // which is passed as the size; inserts are ignored, so no arguments are read.
    let len = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_HMODULE | FORMAT_MESSAGE_IGNORE_INSERTS,
            Some(module.0 as *const core::ffi::c_void),
            id,
            lang,
            PWSTR(buf.as_mut_ptr()),
            buf.len() as u32,
            None,
        )
    };
    if len == 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(String::from_utf16_lossy(&buf))
}

/// Trimmed text with every run of whitespace collapsed to one space.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The normalized first line of a message: the text before its first line break (`\r`, `\n`
/// or the `%n` escape) once the leading line breaks and spaces are skipped (most of sfc's
/// messages start with one); a trailing `%0` is dropped.
fn first_line(text: &str) -> String {
    let text = text.trim_start();
    let end = [text.find(['\r', '\n']), text.find("%n")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(text.len());
    let line = &text[..end];
    normalize(line.strip_suffix("%0").unwrap_or(line))
}

/// What System File Checker reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SfcVerdict {
    NoViolations,
    Violations,
    MetadataCorrupt,
    RepairPending,
    Busy,
    CouldNotRun,
    Unknown,
}

/// The verdict of an output: the output lines are read from the end, and the first one that
/// equals (after normalizing) a verdict message decides. Returns the matched line verbatim.
pub(crate) fn verdict(lines: &[String], messages: &SfcMessages) -> (SfcVerdict, Option<String>) {
    for line in lines.iter().rev() {
        let normalized = normalize(line);
        if normalized.is_empty() {
            continue;
        }
        let Some(id) = messages.id_of(&normalized) else {
            continue;
        };
        let verdict = match id {
            NO_VIOLATIONS => SfcVerdict::NoViolations,
            VIOLATIONS => SfcVerdict::Violations,
            METADATA_CORRUPT => SfcVerdict::MetadataCorrupt,
            REPAIR_PENDING => SfcVerdict::RepairPending,
            BUSY => SfcVerdict::Busy,
            _ => SfcVerdict::CouldNotRun,
        };
        return (verdict, Some(line.trim().to_string()));
    }
    (SfcVerdict::Unknown, None)
}

/// How a finished `sfc /verifyonly` run is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SfcJudgement {
    pub(crate) outcome: StepOutcome,
    pub(crate) text: String,
    pub(crate) hint: Option<String>,
    /// The verdict line as System File Checker printed it.
    pub(crate) windows_message: Option<String>,
}

/// Judges a finished run from its output and exit code.
pub(crate) fn judge(lines: &[String], messages: &SfcMessages, exit_code: i32) -> SfcJudgement {
    let (verdict, windows_message) = verdict(lines, messages);
    let (outcome, text, hint): (StepOutcome, &str, Option<String>) = match verdict {
        SfcVerdict::NoViolations => (StepOutcome::Ok, "No integrity violations found.", None),
        SfcVerdict::Violations => (
            StepOutcome::Attention,
            "Windows found damaged system files.",
            Some("Open Tools and run Repair system files.".to_string()),
        ),
        SfcVerdict::MetadataCorrupt => (
            StepOutcome::Attention,
            "No damaged files were found, but the component store's metadata is damaged.",
            Some(
                "Open Tools and run Repair the component store, then Repair system files."
                    .to_string(),
            ),
        ),
        SfcVerdict::RepairPending => (
            StepOutcome::Attention,
            "A system repair is waiting for a restart.",
            Some("Restart Windows; the next run checks again.".to_string()),
        ),
        SfcVerdict::Busy => (
            StepOutcome::Skipped,
            "Not checked: another servicing operation was running.",
            None,
        ),
        SfcVerdict::CouldNotRun => (
            StepOutcome::Failed,
            "System File Checker couldn't run the check.",
            None,
        ),
        SfcVerdict::Unknown if exit_code == 0 => (
            StepOutcome::Unknown,
            "Finished; the result is in the log.",
            None,
        ),
        SfcVerdict::Unknown => (
            StepOutcome::Attention,
            "System File Checker ended with an error; the result is in the log.",
            classify(ToolId::SfcVerify, exit_code, false, None).hint,
        ),
    };
    SfcJudgement {
        outcome,
        text: text.to_string(),
        hint,
        windows_message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The English messages as sfc.exe's message table holds them.
    const ENGLISH: [(u32, &str); 8] = [
        (
            NO_VIOLATIONS,
            "\r\nWindows Resource Protection did not find any integrity violations.\r\n",
        ),
        (
            VIOLATIONS,
            " \r\nWindows Resource Protection found integrity violations.\r\nFor online \
             repairs, details are included in the CBS log file located at\r\n",
        ),
        (
            METADATA_CORRUPT,
            "Windows Resource Protection did not find any integrity violations but \
             cannot\r\nguarantee the integrity of the component store.\r\n",
        ),
        (
            REPAIR_PENDING,
            "\r\nThere is a system repair pending which requires reboot to complete.  Restart \
             \r\nWindows and run sfc again.\r\n",
        ),
        (
            BUSY,
            "Another servicing or repair operation is currently running.  \r\nWait for this \
             to finish and run sfc again.\r\n",
        ),
        (
            COULD_NOT_PERFORM,
            "\r\nWindows Resource Protection could not perform the requested operation.\r\n",
        ),
        (
            SERVICE_FAILED,
            "\r\nWindows Resource Protection could not start the repair service.\r\n",
        ),
        (
            NOT_ADMIN,
            "\r\nYou must be an administrator running a console session in order to \
             \r\nuse the sfc utility.\r\n",
        ),
    ];

    /// A synthetic second language.
    const GERMAN: [(u32, &str); 2] = [
        (
            NO_VIOLATIONS,
            "Der Windows-Ressourcenschutz hat keine Integritätsverletzungen gefunden.\r\n",
        ),
        (
            VIOLATIONS,
            "Der Windows-Ressourcenschutz hat Integritätsverletzungen gefunden.\r\n",
        ),
    ];

    fn messages() -> SfcMessages {
        let mut pairs: Vec<(u32, &str)> = ENGLISH.to_vec();
        pairs.extend(GERMAN);
        SfcMessages::from_pairs(&pairs)
    }

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn first_lines_are_normalized() {
        assert_eq!(first_line("  A   b \t c.\r\nsecond"), "A b c.");
        assert_eq!(first_line("\r\n \r\nA  b.\r\nsecond"), "A b.");
        assert_eq!(first_line("A.%nB"), "A.");
        assert_eq!(first_line("A.%0"), "A.");
        assert_eq!(first_line("\r\n"), "");
        let m = messages();
        assert_eq!(
            m.id_of("Windows Resource Protection found integrity violations."),
            Some(VIOLATIONS)
        );
        assert_eq!(
            m.id_of("You must be an administrator running a console session in order to"),
            Some(NOT_ADMIN)
        );
        let doubled = SfcMessages::from_pairs(&[(BUSY, "x"), (BUSY, "x")]);
        assert_eq!(doubled.first_lines.len(), 1);
    }

    #[test]
    fn every_verdict_is_recognized() {
        let m = messages();
        let cases = [
            (NO_VIOLATIONS, SfcVerdict::NoViolations, StepOutcome::Ok),
            (VIOLATIONS, SfcVerdict::Violations, StepOutcome::Attention),
            (
                METADATA_CORRUPT,
                SfcVerdict::MetadataCorrupt,
                StepOutcome::Attention,
            ),
            (
                REPAIR_PENDING,
                SfcVerdict::RepairPending,
                StepOutcome::Attention,
            ),
            (BUSY, SfcVerdict::Busy, StepOutcome::Skipped),
            (
                COULD_NOT_PERFORM,
                SfcVerdict::CouldNotRun,
                StepOutcome::Failed,
            ),
            (SERVICE_FAILED, SfcVerdict::CouldNotRun, StepOutcome::Failed),
            (NOT_ADMIN, SfcVerdict::CouldNotRun, StepOutcome::Failed),
        ];
        for (id, expected, outcome) in cases {
            let text = ENGLISH.iter().find(|(i, _)| *i == id).unwrap().1;
            let line = first_line(text);
            let output = lines(&[
                "Beginning system scan.  This process will take some time.",
                "",
                "Beginning verification phase of system scan.",
                "Verification 100% complete.",
                "",
                &format!("  {}  ", line.replace(' ', "   ")),
            ]);
            let (verdict, matched) = verdict(&output, &m);
            assert_eq!(verdict, expected, "{id:#x}");
            assert_eq!(matched.as_deref(), Some(line.replace(' ', "   ").as_str()));
            let judged = judge(&output, &m, 0);
            assert_eq!(judged.outcome, outcome, "{id:#x}");
            assert!(judged.text.ends_with('.'), "{id:#x}");
        }
    }

    #[test]
    fn other_languages_and_the_last_match_win() {
        let m = messages();
        let german = lines(&[
            "Die Überprüfung ist zu 100 % abgeschlossen.",
            "Der Windows-Ressourcenschutz hat keine Integritätsverletzungen gefunden.",
        ]);
        let (v, matched) = verdict(&german, &m);
        assert_eq!(v, SfcVerdict::NoViolations);
        assert!(matched.unwrap().starts_with("Der Windows"));
        let both = lines(&[
            "Windows Resource Protection did not find any integrity violations.",
            "Windows Resource Protection found integrity violations.",
            "For online repairs, details are included in the CBS log file located at",
        ]);
        assert_eq!(verdict(&both, &m).0, SfcVerdict::Violations);
        let judged = judge(&both, &m, 0);
        assert_eq!(
            judged.hint.as_deref(),
            Some("Open Tools and run Repair system files.")
        );
    }

    #[test]
    fn unknown_output_depends_on_the_exit_code() {
        let m = messages();
        let output = lines(&["Something else entirely."]);
        assert_eq!(verdict(&output, &m), (SfcVerdict::Unknown, None));
        let ok = judge(&output, &m, 0);
        assert_eq!(ok.outcome, StepOutcome::Unknown);
        assert_eq!(ok.text, "Finished; the result is in the log.");
        let failed = judge(&output, &m, 2);
        assert_eq!(failed.outcome, StepOutcome::Attention);
        assert!(failed.hint.unwrap().contains("0x00000002"));
        assert_eq!(
            judge(&output, &SfcMessages::default(), 0).outcome,
            StepOutcome::Unknown
        );
    }

    #[test]
    fn the_messages_of_this_pc_are_readable() {
        // Read-only: maps sfc.exe as a data file to read its message table; sfc never runs.
        let system = crate::win::paths::system_dir().unwrap();
        let m = SfcMessages::load(&system);
        assert!(
            [NO_VIOLATIONS, VIOLATIONS, BUSY]
                .iter()
                .all(|want| m.first_lines.iter().any(|(id, _)| id == want)),
            "{m:?}"
        );
        assert!(m
            .first_lines
            .iter()
            .all(|(_, l)| !l.is_empty() && l == &normalize(l)));
        assert_eq!(
            SfcMessages::load(Path::new(r"C:\NoSuchFolder")),
            SfcMessages::default()
        );
    }
}
