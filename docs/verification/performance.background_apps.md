# performance.background_apps

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-privacy#letappsruninbackground |
| Source | https://learn.microsoft.com/en-us/windows/privacy/manage-connections-from-windows-operating-system-components-to-microsoft-services |
| Mechanism | policy |
| Observable effect | A Store app's Advanced options page in Settings › Apps loses its 'Background apps permissions' section. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-04 | 26H2 | Home | 26300.9457 | works | functional | n/a | yes | Photos › Advanced options lost its 'Background app permissions' section at once, before any sign-out, and had it back after Undo; no Windows restart in that session; Undo exact (snapshot) |
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | Snipping Tool's Advanced options showed no 'Background apps permissions' section; kept after the Windows restart; the section was back after Undo; the policy is honoured on Home; Undo exact (snapshot) |
