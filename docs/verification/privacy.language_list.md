# privacy.language_list

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | setting |
| Observable effect | The language-list switch in Settings › Privacy & security › Recommendations & offers (General on earlier versions, where it reads 'Let websites show me locally relevant content by accessing my language list') reads Off. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the switch Off at once; kept after the sign-out the batch used (no Windows restart in this batch); the value gone again after Undo, as at the baseline; Undo exact (snapshot) |
