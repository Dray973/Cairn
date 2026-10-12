# privacy.advertising_id

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-privacy#disableadvertisingid |
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | policy |
| Observable effect | The 'Advertising ID' switch in Settings › Privacy & security › Recommendations & offers (General on earlier versions) reads Off and is greyed out. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the switch Off and greyed after the Windows restart (the page was looked at after the restart only); clickable again after Undo; Undo exact (snapshot); ten apply and Undo cycles on 2026-10-06 ended with the value equal to the baseline every time |
