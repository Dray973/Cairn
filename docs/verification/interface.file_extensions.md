# interface.file_extensions

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-common |
| Mechanism | setting |
| Observable effect | File Explorer shows file name extensions, and Folder Options › View shows 'Hide extensions for known file types' cleared. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | ui-state | n/a | yes | ten apply and Undo cycles ended with the value equal to the baseline every time, with no 'not kept' after any apply; applied once more, 'File name extensions' was ticked in File Explorer, and History listed the change after it was unticked there; no restart in that session |
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | extensions shown after the File Explorer restart; kept after the File Explorer restart the batch used (no Windows restart in this batch); hidden again after Undo; Undo exact (snapshot) |
