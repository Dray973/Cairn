# gaming.mouse_acceleration

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-systemparametersinfoa |
| Source | https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-2000-server/cc978665(v=technet.10) |
| Mechanism | setting |
| Observable effect | Mouse Properties › Pointer Options shows 'Enhance pointer precision' unticked, and SystemParametersInfo(SPI_GETMOUSE) returns 0, 0, 0. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | SPI_GETMOUSE read 0, 0, 0 right after the apply (live, no restart needed) and 'Enhance pointer precision' was unticked; kept after the File Explorer restart the batch used (no Windows restart in this batch); after Undo SPI_GETMOUSE read 6, 10, 1 again; Undo exact (snapshot) |
