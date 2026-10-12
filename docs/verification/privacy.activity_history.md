# privacy.activity_history

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-privacy#enableactivityfeed |
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | policy |
| Observable effect | Looked for and not found: no Windows screen on Windows 11 26H2 reflects the three policies (the Activity history page is gone from Settings, and Win+V and the clipboard settings were unchanged). |
| Restart | sign out |
| Range | every version and edition |
| Retired | Cairn found no way to observe an effect of this setting on Windows 11, so it no longer offers it. |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-04 | 26H2 | Home | 26300.9457 | no-effect | none | n/a | yes | after the apply and a sign-out Win+V still listed the test texts and the clipboard settings were neither greyed nor managed; ms-settings:privacy-activityhistory opens no page on 26H2; nothing else changed; Undo exact (snapshot) |
| 2026-10-06 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | the scan of the release candidate showed the tweak as Retired with its reason |
