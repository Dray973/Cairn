# privacy.lock_screen_ads

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-common |
| Source | https://learn.microsoft.com/en-us/windows/configuration/windows-spotlight/ |
| Mechanism | setting |
| Observable effect | Settings › Personalization › Lock screen shows 'Get fun facts, tips, tricks, and more on your lock screen' unticked (with a Picture or Slideshow lock screen). |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the checkbox unticked before the restart and after the Windows restart; the values back at their baseline after Undo; Undo exact (snapshot) |
