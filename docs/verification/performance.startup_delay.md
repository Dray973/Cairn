# performance.startup_delay

| Field | Value |
|---|---|
| Source | https://support.microsoft.com/en-us/windows/change-which-apps-run-automatically-at-startup-in-windows-9115d841-735e-488d-e749-9ba301d441e6 |
| Mechanism | setting |
| Observable effect | Looked for and not found: startup programs did not start measurably sooner after sign-in (scripts/verification/startup_timing.ps1 compared cold boots with and without the value). |
| Restart | sign out |
| Range | every version and edition |
| Retired | Cairn measured no earlier start of startup programs on Windows 11, so it no longer offers this. |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | no-effect | none | yes | yes | cold boot with the value: the machine-wide Run entries started 9.4 to 17.1 s after Explorer and the median over every startup entry was 18.2 s; cold boot without it: 12.0 to 13.6 s and a median of 15.1 s; boot-to-boot noise dominated and nothing started sooner; the value was kept across the boot and undone exactly (snapshot) |
| 2026-10-09 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | the scan of the final catalog shows the tweak as Retired with 'Cairn measured no earlier start of startup programs on Windows 11, so it no longer offers this.' |
