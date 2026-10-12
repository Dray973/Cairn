# interface.end_task

| Field | Value |
|---|---|
| Source | https://blogs.windows.com/windows-insider/2023/04/07/announcing-windows-11-insider-preview-build-23430/ |
| Mechanism | setting |
| Observable effect | Right-clicking an app on the taskbar offers End task, and Settings › System › For developers shows the End Task switch on. Microsoft documents the switch, not the registry value behind it. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | End task present in a taskbar app's menu after the File Explorer restart; kept after the File Explorer restart the batch used (no Windows restart in this batch); gone after Undo; Undo exact (snapshot) |
