# interface.classic_context_menu

| Field | Value |
|---|---|
| Source | https://blogs.windows.com/windowsdeveloper/2021/07/19/extending-the-context-menu-and-share-dialog-in-windows-11/ |
| Mechanism | setting |
| Observable effect | Right-clicking a file shows the full (Windows 10 style) menu at once instead of the compact menu with 'Show more options'. Microsoft documents the compact menu and 'Show more options'; the per-user override of the menu handler's class registration that the tweak writes is not documented. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the full menu opened at once after the File Explorer restart; kept after the File Explorer restart the batch used (no Windows restart in this batch); the compact menu was back after Undo; Undo exact (the created class key removed) |
