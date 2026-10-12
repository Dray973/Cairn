# privacy.error_reporting

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/win32/wer/wer-settings |
| Mechanism | setting |
| Observable effect | A program that crashes produces no new Windows Error Reporting report (scripts/verification/wer_crash_test.py exits 1 with 'no new report'); with reporting on, a report appears in the ReportQueue or ReportArchive folder. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | a crashed test child produced a report before the apply, none within 60 seconds with the tweak, and a report again after Undo; the value was still set after a cold boot made in the same session; Undo exact (snapshot) |
