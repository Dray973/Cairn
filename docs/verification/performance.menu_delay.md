# performance.menu_delay

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-systemparametersinfoa |
| Mechanism | setting |
| Observable effect | Menus and submenus open after 100 ms instead of 400 ms; SystemParametersInfo(SPI_GETMENUSHOWDELAY) returns 100 after the next sign-in. |
| Restart | sign out |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | SPI_GETMENUSHOWDELAY still read 400 ms before the sign-out and 100 ms after the Windows restart; after Undo the registry value was back to 400 while the live value stays 100 ms until the next sign-in; Undo exact (snapshot) |
