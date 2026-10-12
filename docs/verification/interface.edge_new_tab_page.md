# interface.edge_new_tab_page

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/newtabpagecontentenabled |
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/newtabpagehidedefaulttopsites |
| Mechanism | policy |
| Observable effect | Edge's new tab page shows no content feed and no default site tiles; edge://policy lists both values with status OK. |
| Restart | none |
| Range | Microsoft Edge; every Windows 11 version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | Edge 154.0.4258.62: the content feed and the default tiles were gone after the Edge restart and back after Undo; both edge://policy rows OK; kept after the Edge restart the batch used (no Windows restart in this batch); Undo exact (snapshot) |
