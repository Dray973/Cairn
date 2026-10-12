# security.remote_assistance

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-remoteassistance#solicitedremoteassistance |
| Source | https://learn.microsoft.com/en-us/windows-hardware/customize/desktop/unattend/microsoft-windows-remoteassistance-exe-fallowtogethelp |
| Mechanism | policy and setting |
| Observable effect | System Properties › Remote shows 'Allow Remote Assistance connections to this computer' unticked and greyed, Windows Remote Assistance (msra.exe) refuses to create an invitation, and Cairn's security checkup reports Remote Assistance off. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the checkbox unticked and greyed at once; msra.exe › 'Invite someone you trust to help you' answered 'This computer is not set up to send invitations' with a Repair button; the checkup's Remote Assistance check moved to Passed; kept after the sign-out the batch used (no Windows restart in this batch); after Undo the checkbox was ticked and selectable and msra offered invitations again; Undo exact (the policy key that existed before was kept) |
