# privacy.web_search

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-admx-windowsexplorer#disablesearchboxsuggestions |
| Mechanism | policy |
| Observable effect | Looked for and not found: Start search showed no web results before and after, and the File Explorer search box showed no suggestions with the policy and none after Undo. |
| Restart | sign out |
| Range | every version and edition |
| Retired | Cairn found no way to observe an effect of this setting on Windows 11, so it no longer offers it. |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-04 | 26H2 | Home | 26300.9457 | no-effect | none | n/a | yes | Start search for a weather term showed no web results even before the change; the File Explorer search box showed no suggestions with the policy and none after Undo; Settings › Privacy & security › Search unchanged; Undo exact (snapshot) |
| 2026-10-06 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | the scan of the release candidate showed the tweak as Retired with its reason |
