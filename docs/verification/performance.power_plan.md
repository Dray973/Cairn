# performance.power_plan

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/powercfg-command-line-options |
| Mechanism | power scheme |
| Observable effect | powercfg /getactivescheme and Control Panel › Power Options show 'Ultimate Performance (Cairn)' as the active plan; after Undo the previous plan is active and the created plan is gone. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | 'Ultimate Performance (Cairn)' was created and active at once and still active after the Windows restart; Undo restored the previous (OEM) plan and deleted the created plan; Undo exact (snapshot) |
