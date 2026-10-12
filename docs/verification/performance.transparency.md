# performance.transparency

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-common |
| Mechanism | setting |
| Observable effect | Settings › Personalization › Colors shows 'Transparency effects' Off, and the taskbar and Start menu are opaque. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the taskbar and Start were opaque at once (live) and after the File Explorer restart the batch used (no Windows restart in this batch); transparent again after Undo; Undo exact (snapshot) |
