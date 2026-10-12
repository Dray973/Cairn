# privacy.ceip_tasks

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-admx-icm#ceipenable |
| Mechanism | scheduled task |
| Observable effect | Task Scheduler shows the Customer Experience Improvement Program tasks Consolidator and UsbCeip, Autochk › Proxy and DiskDiagnostic › Microsoft-Windows-DiskDiagnosticDataCollector as Disabled; tasks this version of Windows lacks are reported as skipped. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | Consolidator, UsbCeip, Autochk › Proxy and the DiskDiagnostic data collector Disabled right after the apply and after the Windows restart; KernelCeipTask is not on 26H2 and was reported as skipped by name; enabled again after Undo; Undo exact (snapshot) |
