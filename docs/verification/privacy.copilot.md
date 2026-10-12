# privacy.copilot

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-windowsai#turnoffwindowscopilot |
| Mechanism | policy |
| Observable effect | On Windows 11 22H2 and 23H2 the Copilot sidebar and its taskbar button are gone; Windows 11 24H2 and later ignore the policy, and Cairn does not offer it there. |
| Restart | sign out |
| Range | Windows 11 22H2 (build 22621.2361) or later through 23H2; every edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-04 | 26H2 | Home | 26300.9457 | no-effect | functional | n/a | yes | the earlier version's policy values applied on 26H2, which ignores them: Win+C still opened the Copilot app after a sign-out; Undo exact (snapshot); this is why 24H2 and later are outside the tweak's range |
| 2026-10-06 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | the scan showed Not on this version of Windows with 'Windows 11 24H2 and later no longer use this setting; this PC runs Windows 11 Home 26H2 (build 26300.9457).' |
| 2026-10-09 | 26H2 | Home | 26300.9457 | documented | none | n/a | n/a | no PC with Windows 11 23H2 or earlier was available for a check; Microsoft documents the policy for 22H2 and 23H2; the scan of the final catalog shows the same Not on this version of Windows state |
