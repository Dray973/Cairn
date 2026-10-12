# privacy.location

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | policy |
| Observable effect | Settings › Privacy & security › Location shows Location services Off and managed, and a desktop app's location request is denied (scripts/verification/location_access.ps1 prints Denied). |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-04 | 26H2 | Home | 26300.9457 | works | functional | n/a | yes | at once: the location probe answered Denied and Settings showed Location services Off, greyed and managed; Allowed again at once after Undo; no Windows restart in that session; Undo exact (snapshot) |
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the probe answered Denied and Settings showed Off and managed; kept after the Windows restart; Allowed again after Undo and the page unmanaged; Windows' own mirror value of the location consent read 0 right after Undo and was 1 again after the next restart (not a target of the tweak); every target equal to the baseline (snapshot) |
