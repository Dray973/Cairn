# privacy.diagtrack_service

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows-server/security/windows-services/security-guidelines-for-disabling-system-services-in-windows-server |
| Source | https://learn.microsoft.com/en-us/windows/privacy/configure-windows-diagnostic-data-in-your-organization |
| Mechanism | service |
| Observable effect | The Connected User Experiences and Telemetry service (DiagTrack) reports Disabled and Stopped and does not start after a Windows restart. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | works | functional | yes | no | Disabled and Stopped right after the apply and still after the Windows restart; Undo restored Automatic and started the service; the snapshot after Undo differed from the one before: the restore also wrote DelayedAutostart = 0 under the service key, where no value existed before (an absent value means the same: not delayed); the restore of this version writes it only for an original that started delayed, and the check of 2026-10-11 shows its exact undo |
| 2026-10-11 | 26H2 | Home | 26300.9457 | works | functional | n/a | yes | the fixed version applied and undid it without a restart: Disabled and Stopped after the apply; Undo restored Automatic and started the service, and the snapshot after Undo equaled the one before, DelayedAutostart absent before and after; Undo exact (snapshot) |
