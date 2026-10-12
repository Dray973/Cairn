# Privacy

Cairn runs on your PC and has no server. It has no telemetry, no analytics, no crash reports
and no check for its own updates, and it sends nothing about you or your PC anywhere.

## When Cairn uses the network

Cairn opens no network connection of its own. The Windows tools and programs it starts reach
the network at these times:

- **App updates and installs (winget).** Opening Updates checks for app updates with winget once per session, and **Check again** and the check that follows every batch of updates or installs run that check again. A check lists the installed apps and the available updates through winget, which contacts its sources to do so. Updating or installing apps, when you choose it, downloads each app's installer through winget, together with the runtimes the app's winget manifest lists; some installers download more themselves while they run. winget's sources are the winget community repository and the Microsoft Store, under their own terms.
- **Windows Update.** Cairn searches for Windows updates online only when you choose **Check online now**; the search goes to Windows Update (Microsoft). The security checkup reads Windows Update's status and the data Windows Update keeps on the PC, which can start the Windows Update service.
- **Repair the component store.** When you choose it in Tools, DISM downloads good copies of damaged Windows components from Windows Update (Microsoft).
- **Links.** A link you choose, such as a Windows help page, one of Microsoft's pages that the security checkup links to, the project page in About or App Installer in the Microsoft Store, opens in your browser or in the Store. When Cairn runs as administrator, web pages open in the browser without administrator rights.
- **Renew lease.** When you choose it, Windows asks your network's DHCP server for a new address.
- **DNS servers.** The DNS servers you choose for an adapter decide which resolver Windows asks from then on; Cairn sends nothing to them itself.

Scheduled maintenance makes no network connection. The setup downloads nothing: everything
Cairn needs comes with it.

## What stays on your PC

Cairn keeps its data for each Windows account in `%LOCALAPPDATA%\PCOptimizer`: the journal of
your changes and the activity log, the logs, and the transcripts of the winget, Tools and
scheduled maintenance runs. None of it leaves the PC unless you share it yourself. Logs and
transcripts can contain your Windows user name inside the paths they mention; look through them
before you share them. Uninstalling Cairn keeps this folder, because the journal is the only way
to undo changes that are still applied.
