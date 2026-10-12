# interface.edge_first_run

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/hidefirstrunexperience |
| Mechanism | policy |
| Observable effect | A new Edge profile opens without the first-run pages and splash screen; edge://policy lists the value with status OK. |
| Restart | app restart |
| Range | Microsoft Edge; every Windows 11 version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | Edge 154.0.4258.62: a throwaway profile folder showed the welcome pages at the baseline, a second one opened without them after the apply, and a third one showed them again after Undo (the folders were deleted afterwards); the edge://policy row OK; kept after the Edge restart the batch used (no Windows restart in this batch); Undo exact (snapshot) |
