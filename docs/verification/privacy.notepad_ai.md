# privacy.notepad_ai

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/manage-notepad |
| Mechanism | policy |
| Observable effect | Notepad shows no Copilot button and no Write, Rewrite or Summarize entries. |
| Restart | Windows restart |
| Range | Windows 11 22H2 (build 22621.0) or later; every edition; needs Notepad 11.2503.16 or later |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | Notepad 11.2607: after a Notepad restart only Rewrite and Summarize were gone; after the Windows restart the Copilot button and Write were gone too; the Copilot button was back after Undo; Undo exact (the created policy key was removed) |
