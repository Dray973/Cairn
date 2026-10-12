# interface.search_icon

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-search#configuresearchontaskbarmode |
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-windows-11 |
| Mechanism | setting |
| Observable effect | The taskbar shows a search icon instead of the search box, and Settings › Personalization › Taskbar › Search reads 'Search icon only'. Microsoft documents the four taskbar search styles and a policy that pins one of them; the tweak writes the per-user value behind the Settings choice, which is not documented. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the search icon only after the File Explorer restart; kept after the File Explorer restart the batch used (no Windows restart in this batch); the search box back after Undo; Undo exact (snapshot) |
