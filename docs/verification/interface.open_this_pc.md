# interface.open_this_pc

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-common |
| Mechanism | setting |
| Observable effect | Win+E opens File Explorer on This PC, and Folder Options › General › 'Open File Explorer to' reads This PC. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | Win+E opened This PC after the File Explorer restart; kept after the File Explorer restart the batch used (no Windows restart in this batch); Home again after Undo; Undo exact (snapshot) |
