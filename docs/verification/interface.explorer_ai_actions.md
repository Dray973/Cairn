# interface.explorer_ai_actions

| Field | Value |
|---|---|
| Source | https://winaero.com/windows-11-will-allow-removing-ai-actions-for-file-explorer/ |
| Source | https://www.pcworld.com/article/3003810/dont-want-ai-actions-in-file-explorer-windows-11-will-let-you-disable-them.html |
| Mechanism | policy |
| Observable effect | Right-clicking a picture in File Explorer shows no 'AI actions' entry. Microsoft has not published a page for this policy value, so the sources are third-party reports next to the check below. |
| Restart | Windows restart |
| Range | Windows 11 24H2 (build 26100.0) or later; every edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | 'AI actions' was still in a .jpg file's menu after a File Explorer restart and gone after the Windows restart, which is why the tweak asks for a Windows restart; back after Undo and its restart; Undo exact (the created policy key removed) |
