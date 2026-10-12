# gaming.game_mode

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/apps/develop/settings/settings-windows-11 |
| Mechanism | setting |
| Observable effect | Settings › Gaming › Game Mode reads On. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | ui-state | yes | yes | Game Mode read On before and after (Windows turns it on by default without storing a value; the tweak stores it); still On after the Windows restart; Undo deleted the values; Undo exact (snapshot) |
