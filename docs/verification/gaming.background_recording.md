# gaming.background_recording

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-applicationmanagement#allowgamedvr |
| Mechanism | policy and setting |
| Observable effect | With the Xbox Game Bar app installed, Game Bar records no gameplay, in the background or with Win+Alt+R: Microsoft documents that Windows Game Recording isn't allowed while the policy is set. |
| Restart | sign out |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | documented | none | yes | yes | the Xbox Game Bar app is not installed on the test PC, so Gaming › Captures shows no recording controls with or without the policy and there was nothing to record with; the three values were set, kept after the Windows restart and undone exactly; the effect is Microsoft's documentation of the policy |
