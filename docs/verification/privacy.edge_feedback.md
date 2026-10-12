# privacy.edge_feedback

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/userfeedbackallowed |
| Mechanism | policy |
| Observable effect | 'Send feedback' is gone from Edge's Help and feedback menu and Alt+Shift+I opens nothing; edge://policy lists the value with status OK. |
| Restart | app restart |
| Range | Microsoft Edge; every Windows 11 version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | Edge 154.0.4258.62: after the Edge restart 'Send feedback' was gone from Help and feedback and Alt+Shift+I opened nothing; the edge://policy row OK; kept after the Edge restart the batch used (no Windows restart in this batch); 'Send feedback' was back after Undo; Undo exact (snapshot) |
