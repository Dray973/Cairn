# privacy.diagnostic_data

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-system#allowtelemetry |
| Source | https://learn.microsoft.com/en-us/windows/privacy/configure-windows-diagnostic-data-in-your-organization |
| Mechanism | policy |
| Observable effect | Settings › Privacy & security › Diagnostics & feedback says some settings are managed by your organization, Diagnostic data reads Off with 'Your organization prevents sending diagnostic data to Microsoft', 'Send optional diagnostic data' is off and greyed, and 'Improve inking and typing' turns off with a note that the diagnostic data setting prevents sharing inking and typing data. Home and Pro treat the value as Required. |
| Restart | Windows restart |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | before the Windows restart the page still showed optional data on and nothing managed; after it the page was managed, Diagnostic data read Off and 'Improve inking and typing' was off with the note; after Undo the page was unmanaged again with optional data on; Undo exact (snapshot) |
