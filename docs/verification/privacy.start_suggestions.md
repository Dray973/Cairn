# privacy.start_suggestions

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-windows-11 |
| Source | https://support.microsoft.com/en-us/windows/customize-the-windows-start-menu-fde6f576-0fc0-0813-6b0d-d3ec1d244c50 |
| Mechanism | setting |
| Observable effect | 'Show recommendations for tips, shortcuts, new apps, and more' in Settings › Personalization › Start reads Off, and Start's Recommended section shows no tips or app promotions. |
| Restart | File Explorer |
| Range | Windows 11 24H2 (build 26100.0) or later; every edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-06 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | the switch Off at once; Start's Recommended section showed no tip or promotion, but none had shown at the baseline either, so the switch is the evidence; kept after the Windows restart; the value back at its baseline after Undo; Undo exact (snapshot) |
