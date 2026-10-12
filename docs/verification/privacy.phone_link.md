# privacy.phone_link

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-connectivity#allowphonepclinking |
| Mechanism | policy |
| Observable effect | Settings › Bluetooth & devices › Mobile devices and Phone Link report that linking a phone is blocked; after Undo 'Add device' is offered again. |
| Restart | Windows restart |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | after the Windows restart Mobile devices and Phone Link said linking is blocked; no phone was linked to the test PC, so the undo limit about relinking a phone stays as Microsoft documents it; after Undo 'Add device' was offered again; Undo exact (snapshot) |
