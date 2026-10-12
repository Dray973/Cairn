# gaming.sticky_keys

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-stickykeys |
| Source | https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-filterkeys |
| Source | https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-togglekeys |
| Mechanism | setting |
| Observable effect | Pressing Shift five times, holding right Shift for eight seconds or holding Num Lock for five seconds opens no Sticky, Filter or Toggle Keys prompt. |
| Restart | sign out |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | documented | none | n/a | n/a | the test PC was already in the tweak's state (the hotkey bits of the three Flags values were clear), so the scan showed the tweak as applied and the apply wrote nothing; pressing Shift five times opened no prompt in that state; the apply and its undo were not exercised |
