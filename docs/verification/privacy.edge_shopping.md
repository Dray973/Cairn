# privacy.edge_shopping

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/edgeshoppingassistantenabled |
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/showrecommendationsenabled |
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/spotlightexperiencesandrecommendationsenabled |
| Mechanism | policy |
| Observable effect | The Shopping toggle in edge://settings is off and greyed, edge://policy lists the three values with status OK, and Edge shows 'Managed by your organization' while they are set. |
| Restart | none |
| Range | Microsoft Edge; every Windows 11 version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-04 | 26H2 | Home | 26300.9457 | works | ui-state | n/a | yes | Edge 154.0.4258.53: after Reload policies edge://policy listed the three values with source Platform, level Mandatory and status OK; after Undo it said 'No policies set' and the Edge policy key was gone; no restart in that probe; Undo exact (snapshot) |
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | Edge 154.0.4258.62: the Shopping toggle off and greyed in the profile under test, which was signed out; rows OK; kept after the Edge restart the batch used (no Windows restart in this batch); clickable again after Undo; Undo exact (snapshot) |
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | edge://policy listed the three values with status OK after an Edge restart, and the values were still in place after a Windows restart; Undo exact (snapshot) |
