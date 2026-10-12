# interface.hidden_files

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-common |
| Mechanism | setting |
| Observable effect | File Explorer shows hidden files and folders such as AppData, and Folder Options › View shows 'Show hidden files, folders, and drives' selected. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | AppData visible in the profile folder after the File Explorer restart; kept after the File Explorer restart the batch used (no Windows restart in this batch); hidden again after Undo; Undo exact (snapshot) |
