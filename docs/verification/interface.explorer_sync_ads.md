# interface.explorer_sync_ads

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-common |
| Mechanism | setting |
| Observable effect | 'Show sync provider notifications' in File Explorer's Folder Options (View) is cleared, and File Explorer shows no sync app notices at the top of its windows. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the checkbox cleared at once; File Explorer's sync notices were not looked at; kept after the Windows restart; the value back at its baseline after Undo; Undo exact (snapshot) |
