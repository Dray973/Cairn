# privacy.tailored_experiences

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-experience#allowtailoredexperienceswithdiagnosticdata |
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | policy |
| Observable effect | The 'Personalized offers' switch in Settings › Privacy & security › Recommendations & offers (Tailored experiences on earlier versions) reads Off and is greyed out. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the switch Off and greyed after the Windows restart (looked at after the restart only); clickable again after Undo; Undo exact (snapshot) |
