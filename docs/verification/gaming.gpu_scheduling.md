# gaming.gpu_scheduling

| Field | Value |
|---|---|
| Source | https://devblogs.microsoft.com/directx/hardware-accelerated-gpu-scheduling/ |
| Mechanism | setting |
| Observable effect | Settings › System › Display › Graphics › Advanced graphics settings shows 'Hardware-accelerated GPU scheduling' On after a Windows restart. Microsoft does not document the registry value, which stores the switch's state. |
| Restart | Windows restart |
| Range | every version and edition; needs a graphics driver that supports GPU scheduling |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | documented | none | yes | yes | the switch was already On on the test PC (the graphics driver's default, with no value stored), so storing the value changed nothing visible; the value was set, kept after the Windows restart and undone exactly (deleted; the switch stayed On) |
