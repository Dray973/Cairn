# interface.edge_rewards

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/showmicrosoftrewards |
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/showacrobatsubscriptionbutton |
| Source | https://learn.microsoft.com/en-us/deployedge/microsoft-edge-policies/microsoftedgeinsiderpromotionenabled |
| Mechanism | policy |
| Observable effect | Edge's PDF viewer shows no Adobe Acrobat subscription button; where Edge shows them, the Microsoft Rewards entry and the Edge Insider promotion on the About page are gone too; edge://policy lists the three values with status OK. |
| Restart | app restart |
| Range | Microsoft Edge; every Windows 11 version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | Edge 154.0.4258.62: the Acrobat button was gone from the PDF viewer after the Edge restart and back after Undo; the profile under test showed no Microsoft Rewards entry and no Insider promotion before the apply, so those two values had nothing to observe and the description states them as Microsoft's documentation; the three edge://policy rows OK; kept after the Edge restart the batch used (no Windows restart in this batch); Undo exact (snapshot) |
