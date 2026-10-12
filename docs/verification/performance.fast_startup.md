# performance.fast_startup

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/troubleshoot/windows-client/setup-upgrade-and-drivers/fast-startup-causes-system-hibernation-shutdown-fail |
| Source | https://learn.microsoft.com/en-us/windows/configuration/unified-write-filter/hibernate-once-resume-many-horm |
| Mechanism | setting |
| Observable effect | Control Panel › Power Options › 'Choose what the power buttons do' shows 'Turn on fast startup' cleared, and shutting down does a full shutdown instead of hibernating the kernel. |
| Restart | Windows restart |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | documented | none | yes | yes | the Power Options page was not looked at and no shutdown was made to observe a full shutdown (the session restarted Windows, which always does a full boot); the value read 0 after the apply and after the Windows restart and 1 again after Undo; Undo exact (snapshot); the effect is Microsoft's documentation of the value |
