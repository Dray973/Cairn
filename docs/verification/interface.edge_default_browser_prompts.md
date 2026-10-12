# interface.edge_default_browser_prompts

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/defaultbrowsersettingenabled |
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/defaultbrowsersettingscampaignenabled |
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/showpdfdefaultrecommendationsenabled |
| Mechanism | policy |
| Observable effect | Edge shows no 'make Edge your default browser' bar or campaign and no PDF-default prompt; edge://policy lists the three values with status OK. |
| Restart | app restart |
| Range | Microsoft Edge; every Windows 11 version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | documented | none | yes | yes | Edge 154.0.4258.62 was already the default browser on the test PC, so no prompt existed to suppress and edge://settings/defaultBrowser showed an active 'Make default' button while the policies were set; the three edge://policy rows showed source Platform, level Mandatory and status OK; kept after the Edge restart the batch used (no Windows restart in this batch); the rows were gone after Undo; Undo exact (snapshot); the effect is Microsoft's documentation of the policies |
