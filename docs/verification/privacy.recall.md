# privacy.recall

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-windowsai#disableaidataanalysis |
| Source | https://learn.microsoft.com/en-us/windows/client-management/manage-recall |
| Mechanism | policy |
| Observable effect | On a Copilot+ PC, Settings › Privacy & security › Recall & snapshots shows saving snapshots off and managed, and Windows deletes the snapshots Recall saved. |
| Restart | sign out |
| Range | Windows 11 24H2 (build 26100.3915) or later; every edition; needs the Recall component (a Copilot+ PC) |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | the scan showed Not on this PC with 'Cairn found no sign of Recall on this PC.' (the test PC is not a Copilot+ PC and its Recall component is removed) |
| 2026-10-09 | 26H2 | Home | 26300.9457 | documented | none | n/a | n/a | no Copilot+ PC was available for a check; Microsoft documents that the policy turns off saving snapshots and deletes saved ones; the scan of the final catalog shows the same Not on this PC state |
