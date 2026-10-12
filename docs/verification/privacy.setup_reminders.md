# privacy.setup_reminders

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-experience#allowwindowsspotlightwindowswelcomeexperience |
| Source | https://support.microsoft.com/en-us/windows/change-notification-settings-in-windows-8942c744-6198-fe56-4639-34320cf9444e |
| Mechanism | setting |
| Observable effect | 'Suggest ways to get the most out of Windows and finish setting up this device' in Settings › System › Notifications › Additional settings is cleared, and the finish-setup screens no longer appear. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the checkbox cleared at once (26H2 has no 'welcome experience' checkbox, so the tweak writes one value); the screens appear only when Windows decides to show them, so they were not observed; kept after the Windows restart; after Undo the value was back to its baseline 1 and the UserProfileEngagement key stayed, as before; Undo exact (snapshot) |
