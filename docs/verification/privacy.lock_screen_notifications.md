# privacy.lock_screen_notifications

| Field | Value |
|---|---|
| Source | https://support.microsoft.com/en-us/windows/change-notification-settings-in-windows-8942c744-6198-fe56-4639-34320cf9444e |
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-admx-wpn#nolockscreentoastnotification |
| Mechanism | setting |
| Observable effect | A notification raised while the PC is locked does not show on the lock screen and waits in the notification centre; 'Show notifications on the lock screen' in Settings › System › Notifications reads Off. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | a test notification sent while locked (scripts/verification/toast_test.ps1) showed on the lock screen before the apply, did not show after it and waited in the notification centre, and showed again after Undo; the switch read Off; 'Show reminders and incoming VoIP calls on the lock screen' stayed ticked; kept after the Windows restart; Undo exact (snapshot) |
