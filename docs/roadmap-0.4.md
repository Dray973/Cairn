# Cairn 0.4 roadmap

Cairn 0.3 ships every tweak with a public verification record (`docs/verification`). This page
lists what comes next, the candidates that were designed for 0.3 but could not be checked on the
test PC, and the ideas that were looked at and turned down, with the reason for each. Nothing here
is a promise: a candidate ships only after its effect is observed on a real PC, kept after a
restart and undone exactly.

The test PC runs Windows 11 Home 26H2. Several candidates need hardware or an edition it does not
have, so their batch names what a check needs.

## Candidates removed from 0.3 after checking

Each of these was designed, built and then taken out of 0.3 because its check could not be made
on the test PC. They return in 0.4 once a suitable PC is available.

| Candidate | What it does | Targets | Why it left 0.3 | Needs |
|---|---|---|---|---|
| `privacy.search_highlights` | Turns off the illustrations, trending searches and other online content Windows adds to the search box and the search home (recommended; Windows 11 22H2 or later). | policy `HKLM\SOFTWARE\Policies\Microsoft\Windows\Windows Search\EnableDynamicContentInWSB` = 0 | On Windows 11 26H2 the test PC shows no search highlights and Settings › Privacy & security › Search has no 'Show search highlights' switch, so there was nothing to observe. | A PC that still shows the feature (24H2 or 25H2). |
| `security.password_reveal` | Hides the button that reveals a typed password in Windows' own password boxes. | policy `HKLM\SOFTWARE\Policies\Microsoft\Windows\CredUI\DisablePasswordReveal` = 1 | Neither Credential Manager nor the Windows Security credential prompt shows a reveal button on 26H2, so there was nothing to observe. | A PC that still shows the feature. |
| `privacy.paint_ai` | Turns off Paint's Image Creator, Cocreator and Generative fill for everyone on the PC (requires Paint; Windows 11 22H2 build 22621.4870 or 24H2 build 26100.3360 or later). | `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\Paint\DisableImageCreator`, `DisableCocreator`, `DisableGenerativeFill` = 1 | Paint is not installed on the test PC. | A PC with Paint; Cocreator and Generative fill also need a Copilot+ PC. |
| `privacy.cloud_clipboard` | Turns off clipboard sync to other devices. | policy `HKLM\SOFTWARE\Policies\Microsoft\Windows\System\AllowCrossDeviceClipboard` = 0 | The Settings switch wrote no value on the test PC, and the effect can only be seen with a second device. | A second device signed in with the same account (batch B8). |
| `privacy.settings_sync` | Stops Windows backing up and syncing preferences through the Microsoft account. | `HKLM\SOFTWARE\Policies\Microsoft\Windows\SettingSync\DisableSettingSync` = 2, `DisableSettingSyncUserOverride` = 1 | Checking it means changing sync on a real account; it needs a throwaway account or a second PC. | A throwaway Microsoft account or a second PC (batch B8). |
| `privacy.account_notifications` | Turns off account-related notifications in Start. | `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced\Start_AccountNotifications` = 0 | Windows 11 26H2 has no account-notifications switch under Settings › Personalization › Start, so the value has no switch to compare with. | A build that shows the switch again (batch B3). |
| `privacy.recent_files` | Turns off 'Show recommended files in Start, recent files in File Explorer, and items in Jump Lists'. | the value that switch writes under `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced` | Turning the switch off clears the signed-in user's recent-files list, which Undo cannot bring back, so the check needs an account whose lists do not matter. | A temporary local account (batch B4). |
| `privacy.mobile_device_suggestions` | Turns off suggestions for using a phone with this PC. | `HKCU\Software\Microsoft\Windows\CurrentVersion\Mobility\OptedIn` = 0 (undocumented) | The value is undocumented and the only check is circular: a real suggestion must be shown first. | A PC that shows such a suggestion (batch B8). |
| `interface.outlook_classic` | Hides the new Outlook switch in classic Outlook. | Outlook policies under `HKCU\Software\Policies\Microsoft\office\16.0\outlook` | Classic Outlook is not on the test PC. | A PC with classic Outlook (batch B2). |

## Retired in 0.3

A retired tweak stays in the catalog so that changes made with an earlier version still undo, but
it is no longer offered. These could come back only in the form noted.

| Tweak | Why it was retired | What a return would need |
|---|---|---|
| `interface.widgets` | Windows 11 26H2 refuses the policy write (`HKLM\SOFTWARE\Policies\Microsoft\Dsh\AllowNewsAndInterests`) even for administrators. | A new candidate on the per-user value Settings › Personalization › Taskbar › Widgets writes (`HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced\TaskbarDa` = 0), worded as hiding the button for the signed-in account (batch B4). |
| `performance.startup_delay` | Two cold boots with and without the value showed no earlier start of startup programs; boot-to-boot noise dominates. | A measurable effect. |
| `privacy.ceip`, `gaming.multimedia_scheduling`, `gaming.network_throttling` | No way to observe an effect on Windows 11 is known. | An observable effect. |
| `privacy.activity_history`, `privacy.web_search`, `privacy.cortana`, `interface.edge_sidebar` | Retired by the 0.3 checks: no observable effect (Activity History, web search), the Cortana app is gone, the Edge sidebar app list is gone. | Nothing planned. |

## The 0.4 batches

Every batch follows the 0.3 procedure: a design per candidate, an apply and undo on a real PC with
before and after snapshots, and a public record. Candidates marked [proof] rest on a registry value
that Microsoft does not document and must first be shown to work.

| Batch | Candidates | Needs |
|---|---|---|
| **B1 Edge** | `privacy.edge_ai_features` (AI search and suggestions in Edge), `privacy.edge_on_device_ai`, `privacy.edge_typing_services`, `privacy.edge_error_services`, `privacy.edge_preload`, `privacy.edge_do_not_track`, `privacy.edge_autofill`, `privacy.edge_password_saving`, `interface.edge_sign_in_prompts`, `privacy.edge_browser_sign_in` (High risk: the account can no longer sign in), `interface.edge_search_bar`, `privacy.edge_local_suggestions`, `interface.edge_copilot_button` [proof] | Edge Dev for the on-device AI candidate; a spare PC or a second Windows account for the browser sign-in candidate. Every Edge policy page and the Edge Stable release notes are re-read before the release, because Edge retires policies every few weeks. |
| **B2 Office and Outlook** | `interface.outlook_classic` (with a classic-Outlook requirement), `privacy.office_linkedin` (only if Word still has the option), `privacy.office_all_connected_experiences` (High risk) | Classic Outlook; Microsoft 365 Apps for business or enterprise, since consumer Microsoft 365 ignores Office policies. |
| **B3 Start, lock screen and notifications** | `interface.lock_screen_spotlight` [proof], `interface.no_lock_screen`, `interface.logon_blur`, `interface.settings_home` [proof], `interface.verbose_status`, `privacy.account_notifications` if a build shows its switch again | A Home check; a Pro PC where Home ignores a policy. |
| **B4 Interface and Explorer preferences** | `performance.visual_effects`, `interface.battery_percent`, `interface.numlock_sign_in`, `interface.scrollbars`, `interface.snapping`, `interface.storage_sense`, the Widgets button replacement, `privacy.recent_files` | New engine action kinds (below); a laptop for the battery candidate; a temporary local account for recent files. |
| **B5 System, power and network** | `performance.hibernation`, `performance.prefer_ipv4`, `performance.reserved_storage`, `performance.registry_backup` (needs a design and a check of the RegBack folder on 26H2) | New engine action kinds; the reserved-storage change runs DISM only inside an elevated apply or undo. |
| **B6 AI** | `privacy.click_to_do`, `privacy.remove_recall` (High risk; its undo limit names Windows Features), `privacy.presence_sensing`, `privacy.voice_activation`, plus `privacy.paint_ai` for its Copilot+ features | A Copilot+ PC; a presence sensor; the Copilot+ and presence-sensor probes shown to work first. |
| **B7 Telemetry and small privacy** | `privacy.find_my_device`, `privacy.diagnostic_limits`, `privacy.steps_recorder`, `privacy.media_player_usage` [proof], `privacy.malware_report` (with a Malicious Software Removal Tool requirement), `privacy.offline_maps`, `privacy.search_location` | An observable effect for each; a candidate without one is not offered. |
| **B8 Clipboard, sync and phone** | `privacy.clipboard_history` (warns that turning history off clears Win+V), `privacy.cloud_clipboard`, `privacy.sync_passwords`, `privacy.sync_language`, `privacy.sync_accessibility`, `privacy.sync_other_settings`, `privacy.sync_personalization`, `privacy.settings_sync`, `privacy.mobile_devices` [proof], `privacy.mobile_device_suggestions` [proof] | A throwaway Microsoft account or a second PC for the sync candidates; a phone for the mobile candidates. |
| **B9 Security** | `security.projection_to_pc`, `security.remote_desktop` (Pro and higher), `security.wpbt` [proof], `security.driver_search` and `security.coinstallers` [proof], a **Security Mode** card, `wu.optional_updates` in Updates | A Pro PC; an OEM PC with a Windows Platform Binary Table; a device with a co-installer. Cairn never disables Defender, SmartScreen, UAC or Windows Update. |
| **B10 Store-app permission policies** | Policies for camera, microphone, account info, contacts, calendar, call history, email, messaging, phone calls, tasks, notifications, app diagnostics and location; later radios, trusted devices, motion, passkeys and the graphics-capture policies | A camera for the camera check. The descriptions say "Packaged apps, such as apps from the Microsoft Store, the new Teams and the new Outlook, can no longer use …"; whether desktop apps are affected is checked per build. |
| **B11 Other browsers** | Brave policies (one candidate; needs a design), a Browsers topic and a Brave requirement | Brave installed. |

Also planned for 0.4:

- The Optimize list for about 160–190 tweaks: rows built on demand, topic sub-headings that start
  collapsed, in-place updates after a scan.
- The Microsoft Store app phase of Install apps. Disney+ and Prime Video are either offered there
  or removed by the Bloatware list by default, never both, and the Bloatware list checks its
  reinstall note against the install history.
- Descriptions on app tiles (before the languages release).
- The held apps that pass a supervised install (Roblox and rustup are held after 0.3; Roblox
  installed machine-wide and started its player elevated when installed by Cairn).
- Optional Windows features and winget uninstall (new designs).
- The Secure Boot firmware read as an on-demand action, if wanted; 0.3 reads only the servicing
  registry keys, the System log and the update task.

### Engine work for 0.4

New registry data kinds (`FlagsSzSet`, `DwordFlagsSet`, `SzListItem`), writes under
`HKU\.DEFAULT`, a window-arranging live setting, a system-parameter action with its own journal
table, a hibernation action, the reserved-storage change, and protection of
`Software\Microsoft\Windows\CurrentVersion\Policies` in the write-elevation check. New journal
tables mean journal schema 6: 0.3 opens a schema-6 journal only to refuse it, so going back to 0.3
needs a journal backup (the 0.4 release notes will say so). New requirements, each a read-only
probe: battery present, no battery (fail closed), Copilot+ PC, local session, presence sensor,
WPBT table, wireless display, Steps Recorder, legacy Media Player, Malicious Software Removal
Tool, classic Outlook, Brave. A Browsers topic and a Security Mode card.

### After 0.4: languages

Translations come after 0.4.0: gettext for the app, message ids with typed arguments for the
engine, an installer language list and a pseudo-locale run in CI; then a string freeze and the
pilot languages (Simplified Chinese, Russian and German), each reviewed by a native speaker, then
Spanish, Brazilian Portuguese, Japanese and French. Right-to-left languages and Thai come later.

## Looked at and turned down

Ideas from other tune-up tools and from Microsoft's own policy lists that were checked and not
taken, so they are not dropped silently.

| Item | What it writes | Reason |
|---|---|---|
| Stop-screen details (`bsod_details`) | `CrashControl\DisplayParameters` = 1, `DisableEmoticon` = 1 | Deferred: the Windows 11 24H2 and later unexpected-restart screen already shows the stop code and the faulting driver and has no sad face; `DisplayParameters` is not documented for it, and the only check is a deliberate crash. Revisited if Microsoft documents it. |
| UTC hardware clock (`utc_clock`) | `TimeZoneInformation\RealTimeIsUniversal` = 1 | Not offered: Microsoft calls the flag unsupported (its only documentation is a Windows Embedded Standard 7 page), and Undo cannot restore the firmware clock, which Windows writes in UTC while the flag is set. The documented dual-boot fix is on the Linux side. |
| Multiplane overlay (`mpo`) | `Dwm\OverlayTestMode` = 5 | Not offered: undocumented driver work-around for flicker, reported not to work on 24H2 and later; it can hide the real fix, a driver update. |
| NFC service (`nfc_service`) | service `SEMgrSvc` disabled | Not offered: trigger-started and idle on PCs without NFC, so nothing changes there; on PCs with NFC it breaks tap-to-pair and payments, which Windows already has a switch for. |
| Device name in diagnostic data (`device_name_telemetry`) | `AllowDeviceNameInTelemetry` = 0 | No effect: the device name is not sent unless a policy turns it on, so 0 equals the default. |
| Handwriting data sharing and error reports (`handwriting_sharing`, `handwriting_errors`) | two handwriting policies | Removed feature: the handwriting personalization and error-reporting tools are not on 26H2. |
| OneSettings (`onesettings`) | the OneSettings policy | Risky with nothing to observe: Microsoft warns that Windows components and apps use OneSettings for their configuration and may stop working. |
| Experimentation (`experimentation`) and Bluetooth advertising (`bt_advertising`) | MDM-only policies written into the MDM store | Unsupported path: neither has a Group Policy mapping, and only the MDM client should write that store. |
| Headset button assistant (`headset_button`) | `AgentActivationLastUsed` | Undocumented, and the assistant it chose was Cortana, which is gone. |
| OneDrive pre-sign-in (`onedrive_presignin`) | the OneDrive sign-in policy | Undo impossible: Microsoft says the last state stays when the policy is removed, so deleting the value would not restore the previous behaviour. |
| Speech model download and update (`speech_model_download`, `speech_model_update`) | two speech policies | Obsolete: one is undocumented, the other is limited to Windows 10 1703–1803, and Windows speech recognition is deprecated. |
| Touch keyboard text prediction (`touch_kbd_prediction`) | `TabletTip\1.7\EnableTextPrediction` | A typing preference, not a data flow, in an unconfirmed registry location; the documented control is MDM-only. Could return as an Interface preference after a probe. |
| Offline maps auto-update and traffic (`maps_auto_update`, `maps_settings_traffic`) | Maps policies | Retired feature: the Maps app was removed from the Store in 2025 and is not preinstalled since 24H2; `performance.maps_broker` already covers the service. |
| Office CEIP and reliability data (`QMEnable`, `UpdateReliabilityData`) | Office 2016-era options | Nothing to observe: Microsoft 365 Apps control diagnostic data with `SendTelemetry`, which `privacy.office_telemetry` sets. |
| Office first-run privacy notice (`ShownFirstRunOptin`) | an undocumented path under `common\privacy` | The documented policy lives elsewhere and targets the Office 2016 wizard; the Microsoft 365 notice shows once per user, so there is nothing to observe. |
| Outlook inline text prediction (`InlineTextPrediction`) | a per-user preference | Not a policy, classic Outlook only, and Microsoft now turns text predictions off by default. |
| Admin-controlled migration to new Outlook (`DoNewOutlookAutoMigration`) | = 0 | No effect on a home PC: it is an administrator push that nothing sets at home. |
| Office feedback follow-up (`IncludeEmail`) | = 0 | The registry mapping is unverified and there is nothing to observe once `privacy.office_telemetry` has turned feedback and surveys off. |
| Entra-only Edge policies (`ComposeInlineEnabled`, `CopilotPageContext`) | Edge policies | No effect for home users: Microsoft limits them to work or school (Entra) profiles. The Copilot toolbar button policy (`Microsoft365CopilotChatIconEnabled`) is documented the same way and is kept as a [proof] candidate in B1 only. |
| Six values of the "Microsoft Edge - Debloat" bundle | `CreateDesktopShortcutDefault`, the Copilot extension blocklist entry, `EdgeCollectionsEnabled`, `WalletDonationEnabled`, `WebWidgetAllowed`, `EdgeAssetDeliveryServiceEnabled` | No effect once Edge is installed (an Edge Update policy); superseded by the toolbar policy and a list value Cairn would have to append to; obsolete since Edge 153 (Collections removed); feature removed (Wallet Donation); deprecated (the search bar policy replaces it); undocumented breakage ("features won't be able to download assets"). |
