# privacy.edge_telemetry

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/diagnosticdata |
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/personalizationreportingenabled |
| Mechanism | policy |
| Observable effect | edge://settings/privacy shows the optional diagnostic data switch off and locked and 'Personalization & advertising' off; edge://policy lists both values with status OK. |
| Restart | app restart |
| Range | Microsoft Edge; every Windows 11 version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | Edge (its version was not noted in this session; 154.0.4258.62 the evening before): optional diagnostic data was already off on the test PC and read off and locked after an Edge restart; 'Personalization & advertising' Off; edge://policy listed both values with status OK; kept after the Windows restart; after Undo the Edge policy key was gone; Undo exact (snapshot) |
