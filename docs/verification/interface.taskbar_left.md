# interface.taskbar_left

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-windows-11 |
| Mechanism | setting |
| Observable effect | The Start button and taskbar icons sit at the left edge, and Settings › Personalization › Taskbar › Taskbar alignment reads Left. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | functional | n/a | yes | the taskbar moved to the left after the apply and back to the centre after Undo; a later apply was changed back to Center in Settings, History listed the change, and Apply again set the value again, still set 5 seconds later; no restart in that session; Undo exact (snapshot) |
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the taskbar at the left after the File Explorer restart; kept after the File Explorer restart the batch used (no Windows restart in this batch); centred again after Undo; Undo exact (snapshot) |
