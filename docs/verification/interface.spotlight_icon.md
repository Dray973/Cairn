# interface.spotlight_icon

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/configuration/windows-spotlight/ |
| Source | https://winaero.com/how-to-remove-learn-more-about-this-picture-icon-in-windows-11/ |
| Mechanism | setting |
| Observable effect | The 'Learn about this picture' icon is gone from the desktop while the background is Windows spotlight, and the pictures keep changing. Microsoft documents Windows spotlight; the hidden-icon value the tweak writes is reported by the third-party page next to the check below. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the icon gone after the File Explorer restart with the background still a spotlight picture; kept after the Windows restart; the icon back after Undo and its restart; Undo exact (snapshot) |
