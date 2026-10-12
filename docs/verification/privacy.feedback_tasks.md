# privacy.feedback_tasks

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-experience#donotshowfeedbacknotifications |
| Mechanism | scheduled task |
| Observable effect | Task Scheduler shows the Feedback tasks DmClient and DmClientOnScenarioDownload (under Microsoft › Windows › Feedback › Siuf) as Disabled. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | both tasks Disabled right after the apply and after the Windows restart; enabled again after Undo; Undo exact (snapshot) |
