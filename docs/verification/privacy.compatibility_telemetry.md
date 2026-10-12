# privacy.compatibility_telemetry

| Field | Value |
|---|---|
| Source | https://support.microsoft.com/en-us/help/4525208 |
| Source | https://learn.microsoft.com/en-us/windows/privacy/required-diagnostic-events-fields-windows-11-24h2 |
| Mechanism | scheduled task |
| Observable effect | Task Scheduler shows the Application Experience tasks Microsoft Compatibility Appraiser Exp and MareBackup as Disabled; tasks this version of Windows lacks are reported as skipped. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | Microsoft Compatibility Appraiser Exp and MareBackup Disabled right after the apply and after the Windows restart; Microsoft Compatibility Appraiser and ProgramDataUpdater are not on 26H2 and were reported as skipped by name; enabled again after Undo; Undo exact (snapshot) |
