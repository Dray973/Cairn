# privacy.explorer_cloud_files

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-fileexplorer#disablegraphrecentitems |
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-common |
| Mechanism | setting |
| Observable effect | 'Include account-based insights, recent, favorite, and recommended files' in File Explorer's Folder Options (General) is cleared, and Home lists files from this PC only. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the checkbox cleared at once; the Home page effect could not be shown because the Recommended section was off on the test PC, so that part of the description is Microsoft's statement (documented only); kept after the Windows restart; the value back at its baseline after Undo; Undo exact (snapshot) |
