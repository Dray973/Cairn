# privacy.suggested_content

| Field | Value |
|---|---|
| Source | https://support.microsoft.com/en-us/windows/change-notification-settings-in-windows-8942c744-6198-fe56-4639-34320cf9444e |
| Source | https://learn.microsoft.com/en-us/windows/configuration/windows-spotlight/ |
| Mechanism | setting |
| Observable effect | 'Get tips and suggestions when using Windows' in Settings › System › Notifications › Additional settings is cleared (SubscribedContent-338389Enabled). |
| Restart | sign out |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the checkbox cleared at once; 'Show me suggested content in the Settings app' is no longer on the General page of 26H2 and was not located elsewhere; SilentInstalledAppsEnabled (silent installs), SystemPaneSuggestionsEnabled and SubscribedContent-338388Enabled (Start suggestions) and SubscribedContent-338393Enabled, -353694Enabled and -353696Enabled (suggested content in Settings) had nothing to observe on the test PC; kept after the Windows restart; Undo exact (snapshot) |
