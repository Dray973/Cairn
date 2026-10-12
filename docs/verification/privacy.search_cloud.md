# privacy.search_cloud

| Field | Value |
|---|---|
| Source | https://support.microsoft.com/en-us/windows/windows-search-and-privacy-99fb8251-7260-1cd6-1bbb-15c2370eb168 |
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-search#allowcloudsearch |
| Mechanism | policy and setting |
| Observable effect | Both 'Search my accounts' switches (Microsoft account; Work or School account) in Settings › Privacy & security › Search read Off and are greyed, and the page says some settings are managed by your organization. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | both switches Off and greyed with the managed banner at once, so the policy half is honoured on Home; no cloud result was seeded, so search results themselves were not compared; kept after the sign-out the batch used (no Windows restart in this batch); after Undo both switches On and clickable; Undo exact (snapshot) |
