# privacy.online_speech

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | setting |
| Observable effect | Settings › Privacy & security › Speech shows 'Online speech recognition' Off, and voice typing (Win+H) asks to turn it on. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | documented | none | yes | yes | online speech recognition had never been turned on on the test PC (the value was absent), so the tweak had nothing visible to change (the Speech page in Settings was not looked at); the value was set, kept after the Windows restart and undone exactly; the effect is Microsoft's documentation of the value |
