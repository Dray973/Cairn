# privacy.feedback_prompts

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-experience#donotshowfeedbacknotifications |
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | policy and setting |
| Observable effect | Windows shows no feedback notifications and asks for no feedback; on versions that have it, 'Feedback frequency' in Settings › Privacy & security › Diagnostics & feedback reads Never. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | documented | none | yes | yes | Windows 11 26H2 has no 'Feedback frequency' control, and feedback notifications were not watched for; the two values were set, kept after the Windows restart and undone exactly; the effect is Microsoft's documentation of the policy and of the Siuf values |
