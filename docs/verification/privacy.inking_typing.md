# privacy.inking_typing

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-textinput#allowlinguisticdatacollection |
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | policy and setting |
| Observable effect | 'Improve inking and typing' in Settings › Privacy & security › Diagnostics & feedback reads Off and managed. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the switch Off and managed at once (Home rule: the policy is honoured on Home as far as Settings shows); kept after the sign-out the batch used (no Windows restart in this batch); the values back at their baseline after Undo; Undo exact (snapshot) |
