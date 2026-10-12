# The app catalog

Updates › Install apps offers a list of apps that Cairn installs with winget, one after another and as silently as each installer allows, from each app's publisher. This page describes that list: how apps are chosen, every app of the default list with its publisher, installer and notes, the apps held back until a supervised install test passes (with the outcome of the tests made so far), and the apps left out and why.

The default list is `DEFAULT_APPS` in `crates/core/src/updates/apps.rs`, and the held apps are `HELD_APPS` in the same file. A test checks that the tables of [The default list](#the-default-list) and [Held apps](#held-apps) list exactly those ids.

## How the list works

- **121 apps in nine categories.** Each Windows account can change its own copy of the list with Edit list: add any winget package id, remove apps, or restore the default list. An account's own list is kept when Cairn is updated; Install apps then says how many apps of the default list it doesn't have, and Restore the default list brings them in and removes the account's own additions.
- **Sections and the Find box.** Each category is a section that opens when its heading is selected. A list of more than 40 apps starts with every section closed; a section opened or closed stays so while Cairn is open. The Find box shows the apps with a word of their name or id that starts with each word typed, at most 40 at a time, and works for names written together: "zip" finds 7-Zip and NanaZip.
- **How Cairn installs.** One app at a time, from Cairn running as administrator for the signed-in account, always with `winget install --id <id> --exact --source winget --silent --no-upgrade --accept-package-agreements --accept-source-agreements --disable-interactivity`. Cairn never passes `--scope`, `--location`, `--override`, `--custom`, `--force` or `--version`, so winget picks the installer: it prefers one for the signed-in account and otherwise takes one for the whole PC, and each publisher's own silent defaults apply. Apps already installed are skipped.
- **Installs can't be undone by Cairn.** Remove an app in Settings › Apps. Each install is recorded in History › Activity.
- **Installed means Windows lists it.** An app shows ✓ Installed when Cairn's check finds it in Windows' list of installed apps (`winget export`), or when winget finds it already installed. A check follows every batch of installs; until it ends, an app winget reports as installed shows "winget reported success. The next check shows whether Windows lists it." If the check doesn't find the app, it shows "winget reported success, but Windows doesn't list it yet. Choose Check again in a minute." until a check finds it.
- **Apps Cairn removed.** Cairn's Debloat section can remove Microsoft Teams, which is also on this list. Installing it from Install apps doesn't change Debloat's record of the removal, and the install confirmation says when Cairn removed the app.
- **Install folders.** An installer that needs an install folder fails with "This app's installer needs an install folder, which Cairn doesn't choose. Install it from its publisher's site.", because Cairn never chooses one.

## How apps are chosen

An app is in the default list when all of these hold:

1. A typical Windows 11 user might install it on a new PC, or it is a mainstream tool in its field, such as software development or design.
2. It is in the `winget` source under its publisher's own id, and `winget show --id <id> --exact` finds it.
3. Its installer supports a silent install, and the installer winget picks on x64 neither refuses to run from an administrator app nor needs an install folder.
4. It is maintained. Launchers that update themselves while their winget package stays old, such as Steam and Amazon Games, are the exception.
5. It is free to use, has a free plan, or is the client of a paid service whose description here says it is paid. Trials and apps that keep asking for a purchase are out.
6. It brings no bundled offers, no promotional background component and no advertising that only a paid plan removes. The ads of a streaming service's free tier are part of its content.
7. At most the two or three leading apps of a kind are in, so the list stays short enough to read. Browsers and password managers are the exceptions: the list has more of them because they differ in how they handle privacy and where they keep your data.

An app is out when any of these hold:

- **It conflicts with Cairn:** another optimizer, cleaner or leftover remover; a driver updater; a second app installer or updater; a tool that writes the policies Cairn records; a tool that takes over the DNS servers Cairn manages; or a replacement Start menu or context menu.
- **It is a remote-access tool.** Remote-access tools are abused in support scams (CISA advisory AA23-025A), so the list offers none; users can add one.
- **Its driver does more than the app's own job.** A vendor's signed driver is fine when it reads the hardware the app shows (the sensor tools), is the app's network adapter (the VPNs) or runs the app's own virtual machines and connects them to the network and to USB devices (VirtualBox). A driver that changes how Windows runs other software (sandboxing, process tampering, overclocking) or that replaces the boot loader is out.
- **Other system-level risk:** firewall rules that can cut the connection, partitioning, codec packs that change every app's decoders, or binaries nobody can explain.
- **Legal or terms-of-service risk:** emulators, Discord client mods, widely pirated trialware.
- **It is a portable package or a command-line tool.** winget installs portable and zip-portable packages without a Start menu entry, and Windows doesn't list them as installed. Command-line tools are out too, except the language toolchains (Python, Node.js, Go, .NET SDK, Eclipse Temurin JDK) and GitHub CLI, which installs with an MSI package that Windows lists and goes with Git.
- **It is a server or a background service** rather than a desktop app.
- **It duplicates an app already chosen**, is another version of one, or does what Windows or Cairn already do: Cairn's space analyzer and disk speed test, Windows' Night light, clipboard history and built-in `sudo`.
- **It is out of support or about to be** (.NET 6, 8 and 9), or it is a **pre-release** build.
- **Windows doesn't list it as installed,** so Cairn could never show it as installed.

## Categories

| Key | Heading | Apps |
|---|---|---|
| `browsers` | Browsers | 6 |
| `chat` | Chat and calls | 9 |
| `gaming` | Gaming | 9 |
| `media` | Media | 13 |
| `creative` | Creative tools | 15 |
| `productivity` | Productivity | 21 |
| `utilities` | Utilities | 18 |
| `security` | Security and privacy | 8 |
| `developer` | Developer tools | 22 |
| | **Total** | **121** |

An entry of an account's own list with a category this version doesn't know, such as one a later version saved, is left out and counted in the list's warning; the rest of the list is still shown.

## The default list

**Installs** is the scope of the installer winget picks on an x64 PC with winget's default settings: Per-user, Per-machine, Per-user (MSIX) for a package registered for the account, or Not declared when the manifest doesn't say. **Installer** is the installer type and, where the manifest declares one, its elevation requirement: `elevatesSelf` asks for administrator rights itself and `elevationRequired` must start elevated; both are fine, because Cairn installs as administrator. The installers were checked with `winget show` and the manifests in microsoft/winget-pkgs on 2026-10-01. "(In 0.2.)" marks the apps that were in Cairn 0.2's list. "(Install checked.)" marks the apps whose install from Cairn was checked on a real PC (Windows 11 Home 26H2) on 2026-10-08: each installed with exit 0, showed ✓ Installed once a check found it, and was listed by Settings › Apps.

### Browsers (6)

| winget id | Name | Publisher | Installs | Installer | Description | Notes |
|---|---|---|---|---|---|---|
| `Mozilla.Firefox` | Firefox | Mozilla | Per-machine | nullsoft | Mozilla's open-source web browser. | Firefox adds Mozilla's Default Browser Agent, a scheduled task that checks which browser is the default; whether it still shows notifications asking to make Firefox the default was not checked. (In 0.2.) |
| `Google.Chrome` | Google Chrome | Google | Per-machine | wix, elevatesSelf | Google's web browser. | (In 0.2.) |
| `Brave.Brave` | Brave | Brave Software | Per-user | exe | Chromium-based browser that blocks ads and trackers. | (In 0.2.) |
| `Vivaldi.Vivaldi` | Vivaldi | Vivaldi Technologies | Per-user | exe | Highly customizable browser with built-in mail and calendar. | (In 0.2.) |
| `LibreWolf.LibreWolf` | LibreWolf | LibreWolf | Per-machine | nullsoft | Firefox-based browser with telemetry removed and stricter privacy defaults. | No automatic updates unless LibreWolf WinUpdater is added; App updates keeps it current. |
| `DuckDuckGo.DesktopBrowser` | DuckDuckGo | DuckDuckGo | Per-user (MSIX) | msix | Privacy-focused browser with tracker blocking and one-click data clearing. |  |

### Chat and calls (9)

| winget id | Name | Publisher | Installs | Installer | Description | Notes |
|---|---|---|---|---|---|---|
| `Discord.Discord` | Discord | Discord | Per-user | exe | Voice, video and text chat for communities and gaming. | Updates itself; winget's update-all skips it. Squirrel installer (see the Squirrel note). Its manifest has no silent switch, so Discord's setup splash shows during the install. (In 0.2.) |
| `SlackTechnologies.Slack` | Slack | Slack Technologies | Per-user | exe | Team chat for work. | Updates itself; winget's update-all skips it. Squirrel installer (see the Squirrel note). (In 0.2.) |
| `Zoom.Zoom` | Zoom | Zoom | Per-machine | msi | Video meetings, chat and phone. | (In 0.2.) |
| `Microsoft.Teams` | Microsoft Teams | Microsoft | Per-user (MSIX) | msix | Chat and video meetings for work, school and personal accounts. | (In 0.2.) |
| `Telegram.TelegramDesktop` | Telegram | Telegram | Per-user | inno | Cloud-based messaging app. | Its silent install does not open Telegram. (In 0.2. Install checked.) |
| `OpenWhisperSystems.Signal` | Signal | Signal Messenger | Per-user | nullsoft | End-to-end encrypted messaging; links to Signal on your phone. | (In 0.2.) |
| `Cisco.Webex` | Webex | Cisco | Per-user | wix | Video meetings and team messaging. | Updates by removing the old version first. |
| `TeamSpeakSystems.TeamSpeakClient` | TeamSpeak 3 | TeamSpeak Systems | Per-user | nullsoft | Low-latency voice chat, popular with gaming groups. | 3.6.2 is TeamSpeak's current stable client; TeamSpeak 6 is still a beta. |
| `Element.Element` | Element | Element | Per-user | exe | Encrypted chat on the open Matrix network. | Squirrel installer (see the Squirrel note): the install finishes a few seconds after winget reports success, and the tile says so until the next check finds it; Element does not open by itself. (Install checked.) |

### Gaming (9)

| winget id | Name | Publisher | Installs | Installer | Description | Notes |
|---|---|---|---|---|---|---|
| `Valve.Steam` | Steam | Valve | Per-machine | nullsoft, elevatesSelf | Game store, launcher and community. | (In 0.2.) |
| `EpicGames.EpicGamesLauncher` | Epic Games Launcher | Epic Games | Per-machine | wix, elevatesSelf | Epic Games Store, Fortnite and Unreal Engine. | (In 0.2.) |
| `GOG.Galaxy` | GOG Galaxy | GOG | Per-machine | inno | GOG's store for DRM-free games, and a launcher for other libraries. | (In 0.2.) |
| `ElectronicArts.EADesktop` | EA app | Electronic Arts | Per-machine | burn | Electronic Arts' game store and launcher. | Updates by removing the old version first. (In 0.2.) |
| `Ubisoft.Connect` | Ubisoft Connect | Ubisoft | Per-machine | nullsoft, elevatesSelf | Ubisoft's game store and launcher. | (In 0.2.) |
| `Amazon.Games` | Amazon Games | Amazon | Per-user | exe | Launcher for games from Prime Gaming and Amazon. | winget adds the 32-bit Visual C++ runtime it needs. |
| `RockstarGames.Launcher` | Rockstar Games Launcher | Rockstar Games | Not declared | exe, elevationRequired | Launcher for Grand Theft Auto V, Red Dead Redemption 2 and other Rockstar games. | winget adds the Visual C++ runtimes it needs. |
| `Nvidia.GeForceNow` | GeForce NOW | NVIDIA | Per-user | exe | Cloud gaming: plays games you own on NVIDIA's servers. |  |
| `Playnite.Playnite` | Playnite | Josef Nemec | Per-user | inno | Open-source library that gathers your games from every launcher. |  |

### Media (13)

| winget id | Name | Publisher | Installs | Installer | Description | Notes |
|---|---|---|---|---|---|---|
| `VideoLAN.VLC` | VLC | VideoLAN | Per-machine | wix, elevatesSelf | Plays almost any video or audio file. | (In 0.2.) |
| `clsid2.mpc-hc` | MPC-HC | MPC-HC team (clsid2) | Per-machine | inno, elevatesSelf | Lightweight classic video player. |  |
| `HandBrake.HandBrake` | HandBrake | The HandBrake Team | Per-machine | nullsoft | Converts videos to other formats and sizes. | winget adds .NET Desktop Runtime 10, which it needs, inside HandBrake's own install; both appear in Settings › Apps. (In 0.2. Install checked.) |
| `PeterPawlowski.foobar2000` | foobar2000 | Peter Pawlowski | Per-machine | nullsoft, elevationRequired | Lightweight, customizable music player. |  |
| `XBMCFoundation.Kodi` | Kodi | XBMC Foundation | Per-user | nullsoft | Media center for your own videos, music and photos. |  |
| `Plex.Plex` | Plex | Plex | Per-machine | nullsoft, elevatesSelf | Plays media from a Plex server and Plex's free streaming TV. |  |
| `Deezer.Deezer` | Deezer | Deezer | Per-user | nullsoft | Music streaming app. |  |
| `calibre.calibre` | calibre | Kovid Goyal | Per-machine | wix | Organizes and converts e-books and syncs e-readers. |  |
| `Amazon.Kindle` | Kindle | Amazon | Per-user | nullsoft | Reads Kindle books on your PC. |  |
| `FlorianHeidenreich.Mp3tag` | Mp3tag | Florian Heidenreich | Per-machine | nullsoft, elevationRequired | Edits tags and cover art of music files. | (Install checked.) |
| `DuongDieuPhap.ImageGlass` | ImageGlass | Duong Dieu Phap | Per-user (MSIX) | msix | Fast photo viewer for many image formats. |  |
| `IrfanSkiljan.IrfanView` | IrfanView | Irfan Skiljan | Per-machine | exe | Classic fast image viewer with batch conversion. | Free for non-commercial use. |
| `TIDALMusicAS.TIDAL` | TIDAL | TIDAL Music | Per-user | exe | Hi-fi music streaming (subscription). | Squirrel installer (see the Squirrel note). |

### Creative tools (15)

| winget id | Name | Publisher | Installs | Installer | Description | Notes |
|---|---|---|---|---|---|---|
| `OBSProject.OBSStudio` | OBS Studio | OBS Project | Per-machine | nullsoft | Records the screen and streams live. | Close it before updating; its installer stops while OBS runs. (In 0.2.) |
| `Audacity.Audacity` | Audacity | Audacity Team (Muse Group) | Not declared | wix, elevatesSelf | Records and edits audio. | winget installs the GitHub release MSI, not the MuseHub download. (In 0.2.) |
| `GIMP.GIMP.3` | GIMP | The GIMP Team | Per-user | inno | Open-source photo and image editor. | Package id carries the major version (3). |
| `dotPDN.PaintDotNet` | Paint.NET | dotPDN | Per-machine | zip, wix | Easy photo and image editor. |  |
| `KDE.Krita` | Krita | Krita Foundation | Per-machine | nullsoft | Digital painting and illustration. |  |
| `Inkscape.Inkscape` | Inkscape | Inkscape | Per-machine | wix | Vector graphics editor. | Updates by removing the old version first. |
| `BlenderFoundation.Blender` | Blender | Blender Foundation | Per-machine | wix, elevatesSelf | 3D modelling, animation and rendering. | Large download. Updates by removing the old version first. |
| `darktable.darktable` | darktable | darktable team | Per-user | inno | Edits and organizes camera RAW photos. |  |
| `Meltytech.Shotcut` | Shotcut | Meltytech | Per-user | inno, elevatesSelf | Open-source video editor. |  |
| `KDE.Kdenlive` | Kdenlive | KDE | Per-user | nullsoft | Open-source video editor. |  |
| `Canva.Canva` | Canva | Canva | Per-user | nullsoft | Designs social posts, presentations and documents. | Needs a Canva account. |
| `Canva.Affinity` | Affinity | Canva | Per-user (MSIX) | msix | Photo, vector and page layout editing in one app. | Free with a Canva account; some AI features need Canva Pro. |
| `Musescore.Musescore` | MuseScore Studio | MuseScore | Per-user | wix | Writes, plays back and prints sheet music. | winget installs the GitHub release MSI, not Muse Hub. |
| `NickeManarin.ScreenToGif` | ScreenToGif | Nicke Manarin | Per-user (MSIX) | msix | Records part of the screen as an animated GIF or video. |  |
| `Figma.Figma` | Figma | Figma | Per-user | exe | Interface and graphic design with online collaboration. | Squirrel installer (see the Squirrel note). |

### Productivity (21)

| winget id | Name | Publisher | Installs | Installer | Description | Notes |
|---|---|---|---|---|---|---|
| `TheDocumentFoundation.LibreOffice` | LibreOffice | The Document Foundation | Per-machine | msi | Free office suite: documents, spreadsheets and presentations. | winget adds the Visual C++ runtime it needs. (In 0.2.) |
| `ONLYOFFICE.DesktopEditors` | ONLYOFFICE | Ascensio System | Per-machine | inno, elevatesSelf | Office suite focused on Microsoft Office file compatibility. |  |
| `Adobe.Acrobat.Reader.64-bit` | Adobe Acrobat Reader | Adobe | Per-machine | exe | Views, prints, signs and comments on PDFs. | Promotes paid Acrobat features inside the app. (In 0.2.) |
| `SumatraPDF.SumatraPDF` | SumatraPDF | Krzysztof Kowalczyk | Per-user | exe | Small, fast reader for PDF, e-book and comic files. | (In 0.2.) |
| `geeksoftwareGmbH.PDF24Creator` | PDF24 Creator | geek software GmbH | Per-machine | wix | PDF toolbox: merge, split, compress and convert; adds a PDF printer. |  |
| `Notepad++.Notepad++` | Notepad++ | Notepad++ Team | Per-machine | wix, elevatesSelf | Text and code editor with tabs and syntax highlighting. | winget installs Notepad++'s MSI package. (In 0.2.) |
| `Mozilla.Thunderbird` | Thunderbird | Mozilla (MZLA Technologies) | Per-machine | nullsoft | Free email, calendar and contacts app. |  |
| `Obsidian.Obsidian` | Obsidian | Obsidian | Per-user | nullsoft | Notes kept as Markdown files on your PC. |  |
| `Notion.Notion` | Notion | Notion Labs | Per-user | nullsoft | Notes, documents and project planning in one online workspace. | Close it before updating; its installer stops while Notion runs. |
| `Joplin.Joplin` | Joplin | Laurent Cozic | Per-user | nullsoft | Open-source notes with optional end-to-end encrypted sync. |  |
| `Doist.Todoist` | Todoist | Doist | Per-user | nullsoft | To-do lists and reminders that sync across devices. | Close it before updating; its installer stops while Todoist runs. |
| `Anki.Anki` | Anki | Ankitects | Per-user | wix | Flashcards with spaced repetition for studying. |  |
| `DigitalScholar.Zotero` | Zotero | Corporation for Digital Scholarship | Per-machine | nullsoft | Collects, organizes and cites research sources. |  |
| `Google.GoogleDrive` | Google Drive | Google | Per-machine | exe | Shows your Google Drive files in File Explorer and syncs them. |  |
| `Dropbox.Dropbox` | Dropbox | Dropbox | Not declared | msi | Syncs your Dropbox files with the PC. | winget picks Dropbox's Enterprise MSI installer. |
| `Proton.ProtonDrive` | Proton Drive | Proton AG | Per-user | burn | End-to-end encrypted cloud storage. |  |
| `Mega.MEGASync` | MEGA | Mega Limited | Per-machine | nullsoft, elevatesSelf | Syncs files with MEGA's end-to-end encrypted cloud storage. |  |
| `Cyanfish.NAPS2` | NAPS2 | NAPS2 Software | Per-user (MSIX) | msix | Scans documents to PDF and other formats. |  |
| `Google.EarthPro` | Google Earth Pro | Google | Per-machine | exe | Explores satellite imagery, 3D terrain and historical maps. |  |
| `Perplexity.Perplexity` | Perplexity | Perplexity | Per-user | nullsoft | AI answer engine that cites its sources. |  |
| `Anthropic.Claude` | Claude | Anthropic | Per-user | exe | Anthropic's AI assistant. | Needs an Anthropic account. Squirrel installer (see the Squirrel note): winget installs the classic build, and some features need the Microsoft Store (MSIX) version. |

### Utilities (18)

| winget id | Name | Publisher | Installs | Installer | Description | Notes |
|---|---|---|---|---|---|---|
| `7zip.7zip` | 7-Zip | Igor Pavlov | Per-machine | wix, elevatesSelf | Opens and creates ZIP, 7z, RAR and other archives. | No automatic updates; App updates keeps it current. (In 0.2.) |
| `M2Team.NanaZip` | NanaZip | M2-Team | Per-user (MSIX) | msix | 7-Zip-based archiver that adds to the Windows 11 context menu. | (Install checked.) |
| `voidtools.Everything` | Everything | voidtools | Per-machine | wix | Finds files and folders by name instantly. | (In 0.2.) |
| `Microsoft.PowerToys` | PowerToys | Microsoft | Per-user | burn | Microsoft's power-user tools: window layouts, launcher, color picker and more. | (In 0.2.) |
| `ShareX.ShareX` | ShareX | ShareX Team | Per-user | inno | Screenshots, screen recording and uploads. | (In 0.2.) |
| `Greenshot.Greenshot` | Greenshot | Greenshot | Per-user | inno | Simple screenshot tool with annotations. |  |
| `AutoHotkey.AutoHotkey` | AutoHotkey | AutoHotkey Foundation | Per-user | exe | Automates keystrokes and mouse clicks and creates hotkeys. |  |
| `LocalSend.LocalSend` | LocalSend | LocalSend | Per-user | inno | Sends files to nearby phones and PCs on your network. | Installs for the signed-in account although Cairn runs as administrator; winget run without administrator rights lists it afterwards. (Install checked.) |
| `Google.QuickShare` | Quick Share | Google | Per-machine | wix, elevatesSelf | Shares files with nearby Android phones and PCs. |  |
| `qBittorrent.qBittorrent` | qBittorrent | The qBittorrent project | Per-machine | nullsoft, elevatesSelf | Open-source BitTorrent client without ads. |  |
| `CPUID.CPU-Z` | CPU-Z | CPUID | Per-machine | inno, elevatesSelf | Shows processor, memory and motherboard details. |  |
| `CPUID.HWMonitor` | HWMonitor | CPUID | Per-machine | inno, elevatesSelf | Shows temperatures, voltages and fan speeds. | Its sensor driver loads with memory integrity on, and the install adds nothing to Windows' driver store. (Install checked.) |
| `TechPowerUp.GPU-Z` | GPU-Z | TechPowerUp | Per-machine | exe | Shows graphics card details and sensors. |  |
| `REALiX.HWiNFO` | HWiNFO | REALiX | Per-machine | inno, elevatesSelf | Detailed hardware information and sensor logging. | Free for non-commercial use. |
| `CrystalDewWorld.CrystalDiskInfo` | CrystalDiskInfo | Crystal Dew World | Per-machine | inno | Shows drive health (S.M.A.R.T.) and temperature. |  |
| `Microsoft.VCRedist.2015+.x64` | Visual C++ Redistributable (x64) | Microsoft | Per-machine | burn, elevatesSelf | Runtime that many apps and games need (VCRUNTIME140.dll and others). |  |
| `Microsoft.VCRedist.2015+.x86` | Visual C++ Redistributable (x86) | Microsoft | Per-machine | burn, elevatesSelf | The same runtime for 32-bit apps and games. |  |
| `Microsoft.DotNet.DesktopRuntime.10` | .NET Desktop Runtime 10 | Microsoft | Per-machine | burn | Runtime for Windows apps built with .NET 10. | Package id carries the major version. |

### Security and privacy (8)

| winget id | Name | Publisher | Installs | Installer | Description | Notes |
|---|---|---|---|---|---|---|
| `Bitwarden.Bitwarden` | Bitwarden | Bitwarden | Per-user | nullsoft | Open-source password manager. | (In 0.2.) |
| `KeePassXCTeam.KeePassXC` | KeePassXC | KeePassXC Team | Per-machine | wix | Offline password manager that keeps your vault in a local file. | (In 0.2.) |
| `AgileBits.1Password` | 1Password | AgileBits | Per-user (MSIX) | msix | Password manager (subscription). |  |
| `Proton.ProtonPass` | Proton Pass | Proton AG | Per-user (MSIX) | msix | End-to-end encrypted password manager. |  |
| `Proton.ProtonVPN` | Proton VPN | Proton AG | Per-machine | inno, elevatesSelf | VPN with a free plan. | While connected, the VPN's DNS servers are used instead of the ones set in Cairn's Network section (not checked with Cairn). |
| `MullvadVPN.MullvadVPN` | Mullvad VPN | Mullvad VPN | Per-machine | nullsoft | Paid VPN that asks for no email or personal details. | While connected, the VPN's DNS servers are used instead of the ones set in Cairn's Network section (not checked with Cairn). |
| `WireGuard.WireGuard` | WireGuard | WireGuard LLC | Per-machine | wix | Connects to WireGuard VPN servers from a configuration file. |  |
| `Cryptomator.Cryptomator` | Cryptomator | Skymatic | Per-machine | wix | Encrypts files before they reach your cloud storage. | winget installs the MSI, which doesn't add WinFsp; the manifest says Cryptomator works best with it. Without WinFsp, Cryptomator shows vaults another way (not checked). |

### Developer tools (22)

| winget id | Name | Publisher | Installs | Installer | Description | Notes |
|---|---|---|---|---|---|---|
| `Microsoft.VisualStudioCode` | Visual Studio Code | Microsoft | Per-user | inno | Microsoft's code editor. | (In 0.2.) |
| `Git.Git` | Git | The Git for Windows project | Per-user | inno, elevatesSelf | Version control, with Git Bash. | (In 0.2.) |
| `GitHub.cli` | GitHub CLI | GitHub | Per-machine | wix | GitHub on the command line (gh). | `gh` works in a new console after the install. (Install checked.) |
| `Python.Python.3.14` | Python 3.14 | Python Software Foundation | Per-user | burn | Python programming language. | Package id carries the version. Python 3.15 is new (winget didn't offer it yet on 2026-10-05); the list keeps 3.14, which common packages already support. (In 0.2.) |
| `OpenJS.NodeJS.LTS` | Node.js LTS | OpenJS Foundation | Per-machine | wix, elevatesSelf | JavaScript runtime, long-term support line. | (In 0.2.) |
| `Microsoft.PowerShell` | PowerShell 7 | Microsoft | Per-user (MSIX) | msix | The current cross-platform PowerShell. | winget installs the MSIX package since 7.6: per-user, no remoting into it. |
| `Microsoft.VisualStudio.Community` | Visual Studio Community 2026 | Microsoft | Per-machine | exe | Microsoft's IDE; choose workloads in Visual Studio Installer afterwards. | Free for individuals, open source and small teams. Close Visual Studio before updating. |
| `JetBrains.Toolbox` | JetBrains Toolbox | JetBrains | Per-user | exe | Installs and updates JetBrains IDEs. | (In 0.2.) |
| `GoLang.Go` | Go | Google (Go team) | Per-machine | wix | Go programming language. |  |
| `EclipseAdoptium.Temurin.25.JDK` | Eclipse Temurin JDK 25 | Eclipse Adoptium | Per-machine | wix | Open-source Java development kit, long-term support. | Package id carries the version. |
| `Microsoft.DotNet.SDK.10` | .NET SDK 10 | Microsoft | Per-machine | burn | Builds .NET apps (long-term support release). | Package id carries the version. |
| `Anysphere.Cursor` | Cursor | Anysphere | Per-user | inno | AI code editor based on VS Code. |  |
| `ZedIndustries.Zed` | Zed | Zed Industries | Per-machine | inno | Fast code editor with built-in collaboration. |  |
| `Unity.UnityHub` | Unity Hub | Unity Technologies | Per-user (MSIX) | msix | Installs and manages Unity editor versions. |  |
| `Google.AndroidStudio` | Android Studio | Google | Per-machine | nullsoft, elevatesSelf | Google's IDE for Android apps. | Updates itself; winget doesn't update it. Its manifest counts exit code 1223 (the install was cancelled) as success, so winget reports a cancelled install as successful. |
| `WinSCP.WinSCP` | WinSCP | Martin Prikryl | Per-user | inno, elevatesSelf | SFTP, FTP and SCP file transfer. |  |
| `PuTTY.PuTTY` | PuTTY | Simon Tatham | Per-machine | wix, elevatesSelf | SSH and serial terminal. | Updates by removing the old version first. |
| `Oracle.VirtualBox` | VirtualBox | Oracle | Per-machine | exe | Runs other operating systems in virtual machines. | Install it last in a batch: its network driver briefly resets network connections, so the next app's download can fail. Installs network and USB drivers. With memory integrity or another Windows feature that uses the hypervisor on, VirtualBox runs on Hyper-V, which Oracle documents as experimental and slower on some PCs. Its manifest counts exit code 3010 (restart needed) as success, so Cairn never shows "Restart Windows to finish." for it. winget adds the Visual C++ runtime it needs. |
| `WinMerge.WinMerge` | WinMerge | WinMerge | Per-user | inno | Compares and merges files and folders. |  |
| `GitHub.GitHubDesktop` | GitHub Desktop | GitHub | Per-user | exe | Git client for GitHub repositories. | Squirrel installer (see the Squirrel note). |
| `Postman.Postman` | Postman | Postman | Per-user | exe | Builds and tests HTTP APIs. | Squirrel installer (see the Squirrel note). |
| `Docker.DockerDesktop` | Docker Desktop | Docker | Per-machine | exe, elevatesSelf | Builds and runs containers; uses WSL 2. | On Windows 11 Home it runs Linux containers; Windows containers need Pro, Enterprise or Education. Its installer turns on the Windows features Windows Subsystem for Linux and Virtual Machine Platform, which need a restart to finish and which removing Docker Desktop leaves on. That restart is one Windows reports but winget's result does not carry, so Cairn checks Windows' own pending-restart flags after a batch and says when a restart is needed. Needs hardware virtualization. Docker's documentation says membership of the docker-users group is equivalent to administrator rights. Free for personal use, education, non-commercial open source and businesses under 250 staff and $10M revenue. (Install checked: installed, a Linux container ran after the restart.) |

## Notes that apply to many apps

- **Squirrel installers.** Discord, Slack, Element, TIDAL, Figma, Claude, GitHub Desktop and Postman use the Squirrel installer. Started from an administrator app, Squirrel starts a copy of itself without administrator rights and exits at once, so winget reports success before anything is installed: in the supervised Element install, the app's folder and its entry in Settings › Apps appeared about eight seconds after winget's report, and Element did not open by itself. That is why Cairn marks an install as installed only once a check finds the app; until then the tile says that winget reported success. The installer winget downloaded (about 200 MB for Element) stays in the WinGet folder of your temporary files afterwards; Cairn's Cleanup removes temporary files older than a day.
- **Apps that update themselves.** Most browsers, chat apps, game launchers and cloud-storage apps update themselves, so App updates may find nothing to do for them. Discord, Slack and Android Studio are marked in their manifests so that winget updates them only when they are named; App updates leaves such apps unselected. 7-Zip and LibreWolf have no updater of their own; App updates keeps them current.
- **Close the app first.** The manifests of OBS Studio, Notion and Todoist say their installers stop while the app is open; Cairn then says "The app is open. Close it and try again." Webex, the EA app, Inkscape, Blender and PuTTY update by removing the old version first.
- **Dependencies winget adds.** winget installs the Visual C++ runtime for LibreOffice, KeePassXC, Plex, VirtualBox, the Rockstar Games Launcher and Amazon Games, and .NET Desktop Runtime 10 for HandBrake, inside the app's own install, so they have no rows of their own in History › Activity. Removing the app in Settings › Apps leaves them installed.
- **Drivers.** VirtualBox, WireGuard, Mullvad VPN and Proton VPN install network drivers. Removing such an app in Settings › Apps runs its own uninstaller, which decides what of them stays.
- **VPNs and Cairn's DNS setting.** While Proton VPN or Mullvad VPN is connected, Windows uses the VPN's DNS servers instead of the ones set in Cairn's Network section; whether either app also changes the DNS servers Cairn set on the network adapter was not checked. Cloudflare WARP is left out because taking over DNS is its main job.
- **Licences worth a word.** IrfanView and HWiNFO are free for non-commercial use. Visual Studio Community is free for individuals, open source and small teams.
- **Ids that carry a version** need a look each year: `Python.Python.3.14`, `GIMP.GIMP.3`, `EclipseAdoptium.Temurin.25.JDK`, `Microsoft.DotNet.SDK.10` and `Microsoft.DotNet.DesktopRuntime.10`. `Microsoft.VisualStudio.Community` currently installs the 2026 edition.

## Held apps

These are complete entries of `HELD_APPS`, but Install apps doesn't offer them. Each one joins the default list only when a supervised install on a real PC shows the result in the last column; the table records the outcome of the installs made so far. Users can add any of them to their own list with Edit list.

| winget id | Name | Category | Notes | Supervised install so far | Joins the defaults when |
|---|---|---|---|---|---|
| `Roblox.Roblox` | Roblox | Gaming | Installer scope not declared; a per-user installer started from an administrator app. | Failed on 2026-10-08 (Windows 11 Home 26H2). Started from Cairn, which runs as administrator, the installer installed Roblox machine-wide instead of for the signed-in account, opened the Roblox player with administrator rights, and registered an uninstall entry whose uninstaller removed nothing afterwards, from Settings › Apps or from an administrator prompt; the test install had to be removed by hand. | The install lands in the signed-in account and the player starts without administrator rights, which needs Cairn to run such installers as the signed-in user instead of as administrator; until then it stays out. |
| `Rustlang.Rustup` | Rust (rustup) | Developer tools | Without Visual Studio's C++ tools, rustup-init offers to install them; silent behaviour untested. | Not checked: the test needs a PC without Visual Studio's C++ build tools, and the test PC has them. | On a PC without Visual Studio's C++ build tools, the silent install must not download and run the Visual Studio installer unasked; otherwise it stays out and developers are pointed to Visual Studio Community and rustup. |

Element, TIDAL, Figma, Claude, GitHub Desktop, Postman and Docker Desktop were held until the supervised installs of 2026-10-08 and are in [the default list](#the-default-list) now; their rows there say what was checked. Element stood for the Squirrel apps: its install finished after winget's report, the tile said so until the next check found it, and the app did not start by itself.

## Apps left out

Users can add any of these to their own list with Edit list, using the winget id.

### Left out of the default list

| winget id | Name | Why it is not in the defaults |
|---|---|---|
| `Blizzard.BattleNet` | Battle.net | Its manifest requires an install folder, which Cairn never passes, and has no silent switch: winget stops with 0x8A15005F (an install folder must be provided) before downloading anything. |
| `Apple.iTunes` | iTunes | iTunes for Windows has historically installed a second updater (Apple Software Update) and other Apple components that stay after iTunes is removed (not re-checked for 12.13). Apple's newer Apple Music and Apple Devices apps come from the Microsoft Store, planned for the Store phase. |
| `Amazon.Music` | Amazon Music | Its installer has no silent switch, so it opens its own window and waits for someone at the PC. |
| `IDRIX.VeraCrypt` | VeraCrypt | Its kernel driver is its main function, and encrypting the Windows drive replaces the boot loader with VeraCrypt's own, whose files are signed only with Microsoft's 2011 UEFI certificate; some newer PCs with Secure Boot on don't start it. For encrypting the Windows drive, use Device encryption or BitLocker. Users who want encrypted file containers can add VeraCrypt to their own list. |

**Opera** (`Opera.Opera`, and Opera GX, `Opera.OperaGX`) was in Cairn 0.2's list and is not in 0.3's. Opera's privacy statement says that Opera on Windows includes a component called Browser Assistant that "displays notifications promoting the browser's features or the browser itself" and that may report system events such as another browser being installed. That is a bundled promotional component (rule 6). An installed Opera still appears in App updates, and an account's own list keeps it.

### Apps people ask for

| App | Why it is not in the list |
|---|---|
| Adobe Creative Cloud (`Adobe.CreativeCloud`) | A second app installer and updater: it installs and updates Adobe's apps itself. |
| DaVinci Resolve | Not in the `winget` source when the list was checked; Blackmagic Design offers it from its own site after a registration form. |
| Riot Client and VALORANT (`RiotGames.Valorant.*`) | VALORANT needs Riot Vanguard, an anti-cheat driver that runs in the Windows kernel and starts with Windows. winget has one package per game server region. |
| CapCut (`ByteDance.CapCut`) | The list's video editors are Shotcut and Kdenlive (rule 7). winget installs CapCut silently for the signed-in account, so users can add it. |

### Other apps checked

| App (winget id) | Why it is not in the list |
|---|---|
| Spotify (`Spotify.Spotify`) | Its winget installer refuses to run from an administrator app, so it fails from Cairn. Its Microsoft Store version is planned for the Store phase. |
| Minecraft Launcher (`Mojang.MinecraftLauncher`) | The winget package is Mojang's older launcher (x86, version 2.0.0.0, last updated 2024-10); the current launcher comes through the Microsoft Store and the Xbox app. |
| Microsoft 365 (`Microsoft.Office`) | It is "Microsoft 365 Apps for enterprise" through the Office Deployment Tool, needs a subscription and is already installed on most PCs. |
| Malwarebytes (`Malwarebytes.Malwarebytes`) | A trial of the paid edition, then a scanner with upgrade prompts. The paid edition registers as the antivirus, so Windows Security turns Microsoft Defender's real-time protection off. |
| NordVPN (`NordSecurity.NordVPN`), Windscribe (`Windscribe.Windscribe`) | NordVPN is paid only and depends on .NET Desktop Runtime 8, which ends 2026-11-10. Windscribe's manifest declares no installer scope. Proton VPN (free plan) and Mullvad VPN are in. |
| KeePass (`DominikReichl.KeePass`) | KeePassXC is in. |
| Ditto (`Ditto.Ditto`) | Windows 11 has clipboard history (Win+V). |
| Flow Launcher (`Flow-Launcher.Flow-Launcher`) | PowerToys has a launcher. |
| Auto Dark Mode (`ArminOsaj.AutoDarkMode`) | Changes Windows' theme on a schedule; niche. |
| balenaEtcher (`Balena.Etcher`) | Writes images to drives; niche, and a wrong pick erases a drive. |
| Evernote (`Evernote.Evernote`) | Its free plan is limited enough to work like a trial; Obsidian, Joplin and Notion are in. |
| Logseq, FreeCAD, Ultimaker Cura, LMMS, Upscayl, MediaInfo, MusicBrainz Picard, XnView MP, Plexamp, pCloud Drive, Beeper, Mumble, KDE Connect, Rainmeter, DBeaver, HeidiSQL, NVM for Windows | Specialized or niche. pCloud Drive declares no installer scope, and XnView MP is free for private use only. |
| Streamlabs Desktop (`Streamlabs.Streamlabs`) | Does what OBS Studio does, with a paid upsell. |
| Logitech G HUB (`Logitech.GHUB`) and other vendors' peripheral and driver suites | Useful only with that vendor's hardware; the PC maker's own tools are the better source. |
| Sandboxie-Plus (`Sandboxie.Plus`) | A kernel driver is its main part. |
| Windsurf (`Codeium.Windsurf`) | Cursor and Visual Studio Code are in. |
| Godot (`GodotEngine.GodotEngine`) | A portable zip package without a Start menu entry. |
| Windows Subsystem for Linux (`Microsoft.WSL`) | Part of Windows (`wsl --install`), and its manifest says a restart may be needed. |
| Visual Studio Build Tools 2022 | Visual Studio Community 2026 is in. |
| Eclipse Temurin 21, Microsoft OpenJDK 21 | The list has one JDK, Eclipse Temurin 25. |
| Python Install Manager (`Python.PythonInstallManager`) | The future way to install Python (PEP 773). The list's Python entry moves to it when the traditional installer is phased out, after a real-PC test. |
| Perplexity Comet (`Perplexity.Comet`) | A young browser; niche for a default list. |
| RustDesk (`RustDesk.RustDesk`) | Not found by `winget show` when checked, and a remote-access tool. |
| Messenger, Skype | Discontinued: Meta stopped Messenger's Windows desktop app in December 2025, and Microsoft retired Skype in May 2025. |

### WinUtil's application list

The catalog started from WinUtil's application list (`config/applications.json` of [ChrisTitusTech/winutil](https://github.com/ChrisTitusTech/winutil), MIT licence; see [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md)). Only package ids were taken; the names, descriptions and notes here are Cairn's own. These are the verdicts on its 236 entries:

- **In the default list (90):** .NET Desktop Runtime 10, 1Password, 7-Zip, Adobe Acrobat Reader, Audacity, AutoHotkey, Bitwarden, Blender (3D Graphics), Brave, Calibre, Chrome, Claude Desktop, CPU-Z, Crystal Disk Info, Cursor, Discord, Docker Desktop, Dropbox, EA App, Element, Epic Games Launcher, Everything, Firefox, foobar2000 (Music Player), GeForce NOW, GIMP (Image Editor), Git, GitHub CLI, GitHub Desktop, Go, GOG Galaxy, Google Drive, GPU-Z, HandBrake, HWiNFO, HWMonitor, ImageGlass (Image Viewer), IrfanView, Jetbrains Toolbox, Joplin, KeePassXC, Kodi Media Center, LibreOffice, LibreWolf, LocalSend, Media Player Classic - Home Cinema, Mullvad VPN, NanaZip, NAPS2 (Scanner), NodeJS LTS, Notepad++, OBS Studio, Obsidian, ONLYOFFICE Desktop, Oracle VirtualBox, Paint.NET, PDF24 Creator, Playnite, Plex Desktop, Postman, PowerShell, PowerToys, Proton Drive, Proton Pass, Proton VPN, PuTTY, Python3, qBittorrent, ShareX (Screenshots), Signal, Slack, Steam, Sumatra PDF, Teams, TeamSpeak 3, Telegram, Thunderbird, Ubisoft Connect, Unity Game Engine, Visual C++ 2015-2022 32-bit, Visual C++ 2015-2022 64-bit, Visual Studio 2026, Vivaldi, VLC (Video Player), VS Code, WinSCP, WireGuard, Zed, Zoom, Zotero. Docker Desktop and Element were held until their own supervised install on 2026-10-08, and Claude Desktop, GitHub Desktop and Postman until Element's, which stood for the Squirrel apps.
- **Held after a failed supervised install test (see Held apps) (1):** Roblox.
- **Left out after a closer look (see Apps left out) (2):** Battle.net, iTunes.
- **Part of Windows 11 already (3):** Edge; OneDrive; Windows Terminal.
- **Conflicts with Cairn: optimizer, cleaner, leftover remover, driver updater, second app updater, or a tool that changes the same settings or images (16):** Bulk Crap Uninstaller (removes leftovers without a journal); Cloudflare WARP (replaces the DNS servers Cairn manages); DISMTools (Windows image servicing); Display Driver Uninstaller (driver removal tool); MSEdgeRedirect (hooks Edge system-wide); Nilesoft Shell (replaces Explorer's context menus); NTLite (Windows image customization; paid editions); NVCleanstall (driver installer); OFGB (Oh Frick Go Back) (another tweak tool); Policy Plus (writes policies Cairn journals); Process Lasso (optimizer); Revo Uninstaller (removes leftovers without a journal); Snappy Driver Installer Origin (driver updater); StartAllBack (replaces the Start menu and taskbar; paid); UniGetUI (second app installer and updater); Wise Program Uninstaller (WiseCleaner) (WiseCleaner leftover remover).
- **Remote-access tool: left out of a one-click list because remote-access tools are routinely abused in support scams (CISA AA23-025A); users can add one (4):** AnyDesk; Parsec (winget cannot update it); TeamViewer; TightVNC.
- **Ads, bundled offers or promotional components (3):** CurseForge (ad-supported app); Modrinth App (ads unless Modrinth+); Viber (third-party ads under the chat list on desktop; only Viber Plus removes them).
- **winget installs it as a portable command with no Start menu entry (8):** Autoruns (also overlaps Cairn's Startup section); Claude Code (command-line tool); Codex (command-line tool); Process Explorer; Process Monitor; Rufus Imager; TCPView; uv (command-line tool).
- **Installs outside Windows' installed-apps list, so Cairn cannot show it as installed (1):** Tor Browser (Tor Project: it does not integrate with the system's uninstall utilities; default folder is the Desktop).
- **Microsoft Store app, or maintained only in the Store; planned for the Store phase (3):** ChatGPT Desktop (this id is now 'ChatGPT Classic'; the current app is 9PLM9XGG6VKS); EarTrumpet (Audio) (winget's package is the 2023 build 2.3.0.0; the project ships updates through the Microsoft Store as 9NBLGGH516XP); WhatsApp Desktop (WhatsApp).
- **Trial, nagware or a paid plan is needed to use it (5):** PDF-XChange Editor (free version watermarks the paid features); Proton Mail (desktop app needs a paid plan after a 14-day trial); Sublime Text (evaluation with purchase prompts); Total Commander (shareware with reminders); WinRAR (40-day trial with reminders).
- **System-level risk: kernel driver, overclocking, firewall rules, partitioning or unexplained binaries (6):** K-Lite Codec Standard (codec pack changes system-wide decoders; VLC and MPC-HC carry their own); MiniTool Partition Wizard (partitioning; paid upsells); MSI Afterburner (overclocking); Simplewall (firewall rules can cut connectivity); System Informer (kernel driver; power tool); Ventoy (unexplained binary blobs (Ventoy issue #2795); also portable).
- **Legal or terms-of-service risk (emulators, third-party Discord clients, a download manager that is widely pirated) (5):** Cemu (emulator); Dorion (third-party Discord client); EmulationStation Desktop Edition (emulator front end); Internet Download Manager (trialware that is widely pirated); Vesktop (Discord client mod).
- **A server or background service rather than a desktop app (4):** Jellyfin Server; Plex Media Server; Sunshine/GameStream Server (game-streaming host); Syncthing (CLI / Web UI).
- **Duplicate of an app already chosen, or another version of it (24):** AIMP (Music Player) (foobar2000 is in); Amazon Corretto 21 (LTS) (one JDK: Eclipse Temurin 25); Amazon Corretto 25 (LTS) (one JDK: Eclipse Temurin 25); Amazon Corretto 8 (LTS) (one JDK: Eclipse Temurin 25); Betterbird (Thunderbird fork; Thunderbird is in); Chromium (unofficial Chromium build without automatic updates); Crystal Disk Mark (Cairn has a disk speed test); F.lux (Windows has Night light); Foxit PDF Reader (Acrobat Reader, SumatraPDF and PDF24 cover PDFs); Git Extensions (GitHub Desktop is in); JPEG View; mpc-qt (MPC-HC is in); NodeJS (Node.js LTS is in); nomacs (ImageGlass and IrfanView are in); Okular; PDFgear; PDFsam Basic (PDF24 merges and splits); PeaZip (7-Zip and NanaZip are in); Rust (rustup is the Rust project's installer); TreeSize Free (Cairn has a space analyzer); Ungoogled Chromium (no automatic updates); Visual Studio 2022 (Visual Studio 2026 is in); VS Codium (VS Code is in); WizTree (Cairn has a space analyzer).
- **Out of support or about to be (.NET 6, 8 and 9) (3):** .NET Desktop Runtime 6 (ended 2024-11-12); .NET Desktop Runtime 8 (ends 2026-11-10); .NET Desktop Runtime 9 (ends 2026-11-10).
- **Pre-release build (1):** TeamSpeak 6 (TeamSpeak 6 is still a beta in winget).
- **Specialized or niche; keeps the default list focused (users can add it) (54):** AB Download Manager; Advanced IP Scanner (network administration); Angry IP Scanner (network administration); BlurAutoClicker; Bruno; Chatterino (Twitch chat client); Cinebench R23 (benchmark); CMake; Deskflow; Ente Auth; Fast Node Manager (command-line tool); File Converter; Files (File Explorer replacement); Firefox ESR (enterprise extended-support channel); Floorp; GlazeWM; gsudo (Windows 11 24H2 has sudo built in); Helium (young project); Heroic Games Launcher; Hugo (command-line tool); HxD Hex Editor; Itch.io; Jellyfin Media Player (needs a Jellyfin server); Lazygit (command-line tool); Lua; Moonlight/GameStream Client; mpv; Mullvad Browser (Tor Browser without Tor, meant for VPN users); Neovim; NetBird; Nextcloud Desktop (needs a Nextcloud server); Nmap (network scanner); NuGet (command-line tool); Oh My Posh (Prompt) (prompt theme); OPAutoClicker; OpenVPN Connect; pnpm (command-line tool); Prism Launcher; Proton Authenticator; QOwnNotes; QTox; RDCMan (administrators' remote desktop manager); Ruby; Simplenote; Starship (Shell Prompt) (prompt theme); SyncTrayzor; Tailscale; TranslucentTB (taskbar cosmetics); Vagrant; Waterfox; Wireshark (live capture needs Npcap, which a silent install does not add); Xournal++; Yarn (command-line tool); Zen Browser (versions still carry a 'b' (beta) suffix).
- **Only useful with specific hardware (3):** OpenRGB (direct hardware access); SignalRGB; Virtual Desktop Streamer (VR headset companion).

## Microsoft Store apps (later)

Some apps people want come only, or best, from the Microsoft Store: WhatsApp, Spotify (its winget installer refuses to run from an administrator app), ChatGPT, iCloud, Apple Music, Apple Devices, Apple TV, Netflix, Xbox and EarTrumpet. The default list has no source column yet, so they wait for a release that adds Store apps after a real-PC install test. Disney+ and Prime Video won't be offered while Cairn's Debloat section removes them as promoted apps.

## Checking the ids

`scripts/check_app_ids.py` checks the ids of both lists before a release. It only reads: `winget show` installs nothing, and the GitHub mode lists folders of microsoft/winget-pkgs.

```powershell
.venv\Scripts\python.exe scripts\check_app_ids.py               # winget show for every id
.venv\Scripts\python.exe scripts\check_app_ids.py --manifests   # also hints from each installer manifest
.venv\Scripts\python.exe scripts\check_app_ids.py --github      # without winget, through the GitHub API
```

Exit code 0 means every id was found, 1 that an id is missing and 2 that the check could not finish. The GitHub mode reads `GITHUB_TOKEN` or `GH_TOKEN` when set; without a token GitHub allows 60 requests an hour, fewer than the lists have ids. The check is not part of CI: a moved or renamed id is a data change to review, not a build failure.

## Credits

The app catalog started from WinUtil's application list (MIT, CT Tech Group LLC); see [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md).
