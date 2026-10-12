# privacy.office_connected_experiences

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/microsoft-365-apps/privacy/manage-privacy-controls |
| Source | https://learn.microsoft.com/en-us/microsoft-365-apps/privacy/products-versions-privacy-controls |
| Mechanism | policy |
| Observable effect | In Microsoft 365 Apps for business or enterprise, File › Account › Account Privacy › Manage Settings shows the optional connected experiences off and managed, and features such as 3D Maps in Excel are unavailable. |
| Restart | app restart |
| Range | Microsoft 365 Apps for business or enterprise, or a volume-licensed Office (version 16.0 or later); every Windows 11 version and edition. Consumer Microsoft 365 and Office Home editions ignore these policies, so the tweak is not offered there. |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-08 | 26H2 | Home | 26300.9457 | no-effect | none | yes | yes | on the test PC's consumer Microsoft 365 (Home, version 16.0.20430) the value was in place after an Office restart and a Windows restart, but the privacy settings dialog has no optional connected experiences section at all there, and 'Turn on all connected experiences' stayed ticked: consumer editions ignore these policies, which Microsoft documents for business and enterprise editions; the value was kept after the Windows restart and undone exactly (snapshot) |
| 2026-10-09 | 26H2 | Home | 26300.9457 | not-offered | none | n/a | n/a | the scan of the final catalog shows Not on this PC with 'Microsoft 365 Home ignores Office policies.' on the test PC's consumer Microsoft 365 |
| 2026-10-09 | 26H2 | Home | 26300.9457 | documented | none | n/a | n/a | no PC with Microsoft 365 Apps for business or enterprise, or a volume-licensed Office, was available for a check; Microsoft documents the policy for those editions |
