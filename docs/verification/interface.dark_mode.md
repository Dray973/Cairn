# interface.dark_mode

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-common |
| Mechanism | setting |
| Observable effect | Windows and apps that follow the system theme switch to dark colours at once, and Settings › Personalization › Colors shows the mode Dark. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | dark at once (live) and after the File Explorer restart the batch used (no Windows restart in this batch); light again after Undo; Undo exact (snapshot); ten apply and Undo cycles on 2026-10-06 ended with both values equal to the baseline every time, with no 'not kept' after any apply |
