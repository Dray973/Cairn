# privacy.text_image_generation

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-privacy |
| Mechanism | policy |
| Observable effect | Settings › Privacy & security › Text and image generation shows the main switch Off and managed, and no app can be allowed while it is applied. The value is one of the AppPrivacy policies; Windows 11 26H2 ships its policy definition (AppPrivacy.admx), while the Privacy CSP page does not list it yet. |
| Restart | none |
| Range | Windows 11 24H2 (build 26100.0) or later; every edition; needs Windows' text and image generation features for apps |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the main switch Off and managed at once; no app on the test PC uses the feature, so there was no functional check (Home rule: the description says so); kept after the sign-out the batch used (no Windows restart in this batch); after Undo the value was gone again, as at the baseline; Undo exact (snapshot) |
