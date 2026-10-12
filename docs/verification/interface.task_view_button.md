# interface.task_view_button

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-windows-11 |
| Mechanism | setting |
| Observable effect | The Task View button is gone from the taskbar, and Settings › Personalization › Taskbar shows Task view Off. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the button gone after the File Explorer restart; kept after the File Explorer restart the batch used (no Windows restart in this batch); back after Undo; Undo exact (snapshot) |
