# interface.edge_sidebar

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/hubssidebarenabled |
| Mechanism | policy |
| Observable effect | Looked for and not found: Edge 154 shows no classic sidebar to remove, so the policy changes nothing visible (the Copilot button stays). |
| Restart | none |
| Range | Microsoft Edge; every Windows 11 version and edition |
| Retired | Microsoft retired the Edge sidebar app list in 2026; this policy no longer changes anything Cairn can observe. |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-04 | 26H2 | Home | 26300.9457 | no-effect | ui-state | n/a | yes | Edge 154.0.4258.53: edge://policy listed the value with status OK, but Edge shows no classic sidebar and the Copilot button stayed; Undo exact (snapshot) |
| 2026-10-06 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | the scan of the release candidate showed the tweak as Retired with its reason |
