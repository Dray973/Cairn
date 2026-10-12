# privacy.error_report_task

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/win32/wer/wer-settings |
| Mechanism | scheduled task |
| Observable effect | Task Scheduler shows QueueReporting (under Microsoft › Windows › Windows Error Reporting) as Disabled, so queued reports stay on the PC. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the task Disabled right after the apply and after the Windows restart; enabled again after Undo; Undo exact (snapshot) |
