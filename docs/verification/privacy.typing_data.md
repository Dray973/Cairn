# privacy.typing_data

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | setting |
| Observable effect | On versions that have it, Settings › Privacy & security › Inking & typing personalization shows the custom dictionary off; Windows 11 26H2 has no such page. |
| Restart | sign out |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | documented | none | yes | yes | 26H2 has no Inking & typing personalization page (a Settings search for 'inking' leads only to Diagnostics & feedback), so nothing reflects the values; applied alone first: 'Improve inking and typing' stayed On, so there is no interplay with Stop sending inking and typing data; the values were set, kept after the Windows restart and undone exactly; the effect is Microsoft's documentation of the values |
