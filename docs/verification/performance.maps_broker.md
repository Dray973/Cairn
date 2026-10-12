# performance.maps_broker

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows-server/security/windows-services/security-guidelines-for-disabling-system-services-in-windows-server |
| Mechanism | service |
| Observable effect | Downloaded Maps Manager (MapsBroker) reports Manual and Stopped; after Undo it is Automatic again. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | Manual and Stopped right after the apply and after the Windows restart; after Undo Automatic and Stopped, as at the baseline; Undo exact (snapshot) |
