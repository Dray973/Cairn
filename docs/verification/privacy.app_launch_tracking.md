# privacy.app_launch_tracking

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | setting |
| Observable effect | 'Improve Start and search results' in Settings › Privacy & security › Recommendations & offers (the app-launch tracking switch of General on earlier versions) reads Off. |
| Restart | File Explorer |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the switch Off after the Windows restart (looked at after the restart only); the value back at its baseline after Undo; Undo exact (snapshot) |
