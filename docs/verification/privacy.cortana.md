# privacy.cortana

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-experience#allowcortana |
| Source | https://support.microsoft.com/en-us/cortana/end-of-support-for-cortana |
| Mechanism | policy |
| Observable effect | Looked for and not found: Windows 11 no longer has the Cortana app, so the policy changes nothing visible. |
| Restart | sign out |
| Range | every version and edition |
| Retired | Cairn found no effect of this policy on Windows 11. |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-04 | 26H2 | Home | 26300.9457 | no-effect | none | n/a | yes | the Cortana app is not installed on Windows 11 26H2; nothing observable changed after the apply and a sign-out; Undo exact (snapshot) |
| 2026-10-06 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | the scan of the release candidate showed the tweak as Retired with its reason |
