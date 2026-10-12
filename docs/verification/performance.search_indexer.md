# performance.search_indexer

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows-server/security/windows-services/security-guidelines-for-disabling-system-services-in-windows-server |
| Mechanism | service |
| Observable effect | Windows Search (WSearch) reports Manual; it stops when the tweak is applied and starts only on demand afterwards. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | Manual and Stopped right after the apply; Manual and Running after the Windows restart, started on demand as Manual allows; after Undo Automatic and Running; Undo exact (snapshot) |
