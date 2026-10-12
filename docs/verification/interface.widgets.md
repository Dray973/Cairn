# interface.widgets

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-newsandinterests#allownewsandinterests |
| Mechanism | policy |
| Observable effect | The Widgets button and board are gone from the taskbar. Windows 11 26H2 refuses the policy write to programs, so Cairn no longer offers the tweak. |
| Restart | File Explorer |
| Range | every version and edition |
| Retired | Windows 11 no longer lets programs set this policy, so Cairn no longer offers it. |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | broken | none | n/a | no | the write of the value under the Dsh policy key failed with 'Access is denied' from an elevated process, although administrators have full control of the key; Cairn had created the empty key first, and its removal was refused the same way (also from an elevated shell), so the empty key stayed behind and the Widgets button stayed; the stale record was cleared with Keep as is |
| 2026-10-09 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | the scan of the final catalog shows the tweak as Retired with 'Windows 11 no longer lets programs set this policy, so Cairn no longer offers it.' |
