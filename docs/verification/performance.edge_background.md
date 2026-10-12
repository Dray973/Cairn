# performance.edge_background

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/startupboostenabled |
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/backgroundmodeenabled |
| Mechanism | policy |
| Observable effect | edge://settings/system shows Startup boost and 'Continue running background extensions and apps when Microsoft Edge is closed' off and greyed, and edge://policy lists both values with status OK. |
| Restart | none |
| Range | Microsoft Edge; every Windows 11 version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | Edge (its version was not noted in this session; 154.0.4258.62 the evening before): after an Edge restart both switches were off and greyed and edge://policy listed the values with status OK; kept after the Windows restart; after Undo the Edge policy key was gone; Undo exact (snapshot) |
