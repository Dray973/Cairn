# security.device_metadata

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-deviceinstallation#preventdevicemetadatafromnetwork |
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | policy |
| Observable effect | System Properties › Hardware › Device Installation Settings shows 'No' selected and the choice greyed out. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | 'No' selected and greyed at once (Home rule: the policy is honoured on Home as far as the dialog shows); kept after the sign-out the batch used (no Windows restart in this batch); after Undo 'Yes' was selected and selectable again; Undo exact (the created policy key removed) |
