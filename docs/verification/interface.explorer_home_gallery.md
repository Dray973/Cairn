# interface.explorer_home_gallery

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/win32/shell/integrate-cloud-storage |
| Source | https://learn.microsoft.com/en-us/windows/win32/sysinfo/merged-view-of-hkey-classes-root |
| Mechanism | setting |
| Observable effect | Home and Gallery are gone from File Explorer's left pane, and Win+E still opens a working Home page. |
| Restart | File Explorer |
| Range | Windows 11 24H2 (build 26100.0) or later; every edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | after the File Explorer restart Home and Gallery were gone from the pane and Win+E opened a working Home; kept after the Windows restart; both entries back after Undo and its restart; Undo exact (the two created class keys removed) |
