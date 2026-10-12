# performance.delivery_optimization

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-deliveryoptimization#dodownloadmode |
| Mechanism | policy |
| Observable effect | Settings › Windows Update › Advanced options › Delivery Optimization shows 'Allow downloads from other devices' Off and managed. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the switch Off and managed before the restart; kept after the Windows restart; the value back at its baseline after Undo; Undo exact (snapshot) |
