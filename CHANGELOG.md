# Changelog

What changed in each release of Cairn, newest first. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## 0.3.0 — 2026-10-12

Cairn 0.3.0 checks its own work: it reads back every value a tweak sets or an undo restores,
lists the changes that Windows or another program has changed since, and offers a tweak only
with a public record of its check on a real PC. It also adds new tweaks and a Security group,
search in Optimize, a Secure Boot certificates check, a bigger Install apps list and release
automation.

### Added

- **Verification records.** Every tweak Cairn offers was checked on a real PC (Windows 11 Home
  26H2) and has a record in [docs/verification](docs/verification/README.md): the documentation
  it rests on, the effect to look for and each check, including whether the effect was still
  there after the restart the check used (a Windows restart, or for tweaks that need less, a
  sign-out or a restart of File Explorer or Edge) and whether Undo restored the registry exactly.
  A new tweak whose effect was looked for and not found was left out, and tweaks of earlier
  versions in which no effect could be found are retired (see Removed); where the test PC could
  not show a tweak's effect at all, because the feature was missing there or already in the
  tweak's state or the check did not make the step that shows it, the record says so and, apart
  from Turn on Game Mode, the tweak is not recommended (see Known limitations). The test
  `crates/core/tests/verification_records.rs` checks every record, and with `CAIRN_RELEASE=1`,
  as release builds set it, it fails while any tweak lacks one.
- **Read-back after tweaks and undo.** After applying tweaks or undoing changes, Cairn reads
  every value it wrote or restored half a second later and reports the ones that Windows or
  another program had already changed. A value an apply could not keep stays recorded, so Undo
  still restores its original; a value an undo could not restore is reported, and Cairn does not
  write it again. Five seconds after applying tweaks it checks once more and says whether every
  value it set is still set.
- **Changes that no longer hold.** Optimize's scan marks the tweaks whose recorded values were
  changed after Cairn applied them, and History has a **Changed** view of every recorded change
  that no longer holds (tweaks, startup apps, Windows Update settings, DNS servers and the power
  plan) with **Undo** and **Keep as is**, and **Apply again** for tweaks. Keep as is stops tracking
  a change without touching the PC; its confirmation names the values that Undo could then no
  longer restore, and says so when Cairn keeps tracking the rest of the change. Cairn never sets a
  changed value again on its own, and it says so when changes made by an earlier version of Cairn
  cannot be checked, because that version did not record what it set. `optctl drift` and
  `optctl verify <ids…>` show the same from the command line.
- **Windows versions and editions.** A tweak can be limited to Windows builds and editions:
  Optimize shows *Not on this version of Windows* and *Not on this edition of Windows* next to
  *Not on this PC*, and *Retired* for a tweak Cairn no longer offers, which it lists only while a
  change of it can still be undone. Tweaks for Recall, Notepad's AI features and Windows' text
  and image generation are offered only where those exist, and the Office policies only with an
  Office edition that honours them (see Changed). Profiles skip such rows with the same reason
  and leave retired tweaks out of exports.
- **Undo limits.** A tweak whose effect Undo cannot fully reverse says what stays changed in a
  warning before it is applied (for example, Windows deletes the snapshots Recall saved), is never
  recommended and starts unchecked in a profile.
- **Search in Optimize.** Ctrl+F searches the tweaks by title, description and topic, and the
  **Show** menu lists the tweaks available here (the default), the ones changed since Cairn applied
  them, the ones not available here or all of them. When the scan could not read a tweak Cairn
  applied, *Changed since applied* says so instead of reporting that nothing changed. Category
  headings collapse.
- `optctl catalog` shows each tweak's Windows range, retirement and undo limit; `optctl apply`
  repeats the undo limit of a tweak it applied, and `apply`, `revert` and `scan` print the values
  that were not kept, not restored or changed since applied.
- **New tweaks**, in a new Security group and in the existing ones (recommended tweaks join their
  group's mode; the Security group has none):
  - Privacy: Turn off search history on this PC, Turn off cloud content in search, Stop linking
    phones to this PC, Turn off AI features in Notepad, Stop apps using Windows' text and image
    generation, Stop sending inking and typing data, Don't share your language list with
    websites, Hide tips and app promotions in Start (recommended), Stop the finish-setup screens
    (recommended), Hide notifications on the lock screen (recommended), Hide account-based files
    in File Explorer, Turn off Edge feedback and surveys (recommended).
  - Windows interface: Hide sync app notices in File Explorer (recommended), Remove Home and
    Gallery from File Explorer, Hide AI actions in File Explorer, Hide the 'Learn about this
    picture' icon, Allow long file paths, Stop Edge asking to be the default browser, Skip Edge's
    first-run setup, Remove news and suggested sites from Edge's new tab page (recommended), Hide
    Microsoft Rewards in Edge.
  - Security: Turn off Remote Assistance (recommended), Stop downloading device metadata apps and
    icons.
- **Secure Boot certificates**, a 25th check of the security checkup, in Device security: whether
  Windows has replaced Microsoft's 2011 Secure Boot certificates in the firmware with the 2023
  ones, read from Windows' servicing record and the System log, and, with administrator rights,
  whether the Secure Boot update task is turned on. Cairn never reads or writes the firmware. Its
  fixes open Windows Security, Windows Update, Task Scheduler or Microsoft's pages about the
  update.
- **Release automation.** A release workflow builds each version tag on GitHub Actions and drafts
  its release with `SHA256SUMS.txt` and a build-provenance attestation. Every push to main and
  every pull request runs the Rust, Rust 1.80, telemetry and UI checks; builds on main also build
  the setup and install, upgrade and uninstall it on a throwaway runner. A weekly workflow runs
  the tests that reach Windows itself on a throwaway runner.
- Cairn's programs and libraries carry version information: product Cairn, the version and a
  description.
- The installed copy ships the license notices of the Rust crates in its programs
  (`licenses\Rust-THIRD-PARTY.txt`).
- New: [SECURITY.md](SECURITY.md), [docs/privacy.md](docs/privacy.md),
  [docs/code-signing-policy.md](docs/code-signing-policy.md),
  [docs/app-catalog.md](docs/app-catalog.md), [docs/roadmap-0.4.md](docs/roadmap-0.4.md) (the
  tweaks planned for 0.4, including the candidates this release left out and why) and
  [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md); code signing through the SignPath Foundation
  is prepared and stays off until the project is accepted.

### Changed

- **Turn off Recall snapshots** (`privacy.recall`) is Medium risk and no longer recommended. It
  says that Windows deletes the snapshots Recall already saved and that Undo cannot bring them
  back, and it is offered only where Recall is installed, on Windows 11 24H2 (build 26100.3915) or
  later.
- **Hide the legacy Copilot sidebar (23H2 and earlier)** (`privacy.copilot`) is offered only on
  Windows 11 22H2 and 23H2: Windows 11 24H2 and later ignore the policy, because Copilot is an app
  there. It no longer sets the machine-wide value Cairn 0.2.0 also set, which the policy does not
  use; applying the tweak restores that value's original, and History can undo or keep it.
- **The Office policies** (`privacy.office_telemetry`, `privacy.office_connected_experiences`,
  `privacy.office_cloud_content`) are offered only with Microsoft 365 Apps for business or
  enterprise or a volume-licensed Office, the editions Microsoft documents them for. On a
  consumer edition (Microsoft 365 Family, Personal, Home & Student, Home & Business) Optimize
  shows *Not on this PC* with "Microsoft 365 Home ignores Office policies.", which is what the
  check on such a PC showed: Word's connected experiences stayed on after the policies were set
  and Office and Windows had restarted. Without Office 2016 or later it says "Microsoft Office
  (2016 or later) is not installed."
  Stop Office diagnostic data, feedback and surveys is no longer recommended, because no PC
  available could show its effect.
- **No longer recommended**, because the test PC could not show their effect (see Known
  limitations): Stop feedback prompts, Stop collecting typing and inking data, Turn off online
  speech recognition, Turn off background game recording and Turn off the Sticky, Filter and
  Toggle Keys shortcuts. They are still offered and can be applied one by one.
- **Turn off Edge shopping, tips and promotions** (`privacy.edge_shopping`) no longer mentions
  coupons, and it now says: "On a PC managed by an organization, Edge may ignore this in profiles
  signed in with a personal Microsoft account."
- **Set Downloaded Maps Manager to manual** (`performance.maps_broker`) says that Windows starts
  the service when an app or Settings needs offline maps, instead of naming the Maps app.
- **Descriptions follow what the checks showed.** Settings pages are named as Windows 11 26H2
  names them (Privacy & security › Recommendations & offers, where 'Tailored experiences' is now
  'Personalized offers'). Limit diagnostic data says that Settings then also shows 'Improve
  inking and typing' off. Turn off background game recording states its effect as Microsoft's
  documentation and says that nothing visible changes on a PC without the Xbox Game Bar app.
- **Stop Store apps running in the background** (`performance.background_apps`) takes effect at
  once and no longer asks for a sign-out.
- **Privacy Mode** also turns off Start tips and promotions, the finish-setup screens,
  lock-screen notifications and Edge feedback, and no longer applies the retired tweaks, Recall's
  policy, Stop Office diagnostic data, feedback and surveys, Stop feedback prompts, Stop
  collecting typing and inking data or Turn off online speech recognition; turning it off still
  undoes what Cairn 0.2.0 applied with them.
  **Clean Interface** also hides sync app notices in File Explorer and stops Edge's new tab page
  news, so Edge shows 'Managed by your organization' while it is on, and it no longer removes
  Widgets. **Gaming Mode** now only turns on Game Mode: it no longer prioritises games in
  multimedia scheduling (see Removed) or turns off background game recording and the Sticky,
  Filter and Toggle Keys shortcuts; turning it off still undoes what Cairn 0.2.0 applied with
  them. **Max Performance** no longer removes the startup app delay (see Removed).
- The **Privacy** starter profile also turns off Start tips and promotions, the finish-setup
  screens and lock-screen notifications, and no longer includes the retired tweaks, Recall's
  policy, Stop Office diagnostic data, feedback and surveys, Stop feedback prompts, Stop
  collecting typing and inking data or Turn off online speech recognition; the **Clean** starter
  also hides sync app notices in File Explorer and no longer removes Widgets; the **Gaming**
  starter no longer includes multimedia scheduling, network throttling or the startup app delay,
  and still turns off background game recording and the Sticky, Filter and Toggle Keys shortcuts,
  which Cairn no longer recommends.
- A change that fails while it is being made (a registry value Windows refuses to write because
  it protects the key against programs, a scheduled task, a power plan or a DNS setting that
  cannot be set) no longer leaves a change in History that was never made, and Cairn removes
  again what it created for the change: a registry key, unless that key is protected as well, or
  the 'Ultimate Performance (Cairn)' power plan. A change Cairn 0.2.0 recorded although its write
  failed (Remove Widgets, where the policy key is protected) is no longer shown as a change that
  no longer holds, and Undo ends it without writing anything.
- Undoing a service tweak restores the service's start type without adding a delayed-start value
  that the service did not have before.
- The security checkup's Remote Assistance finding can now be fixed with the recorded, undoable
  **Turn off Remote Assistance** tweak.
- **Install apps** has nine categories (new: Creative tools, Security and privacy) and 121 apps.
  Long lists show category sections that open on demand, and a Find box; an account's own list
  says how many apps of the default list it lacks. Element, TIDAL, Figma, Claude, GitHub Desktop,
  Postman and Docker Desktop joined the list after a supervised install of Element and Docker
  Desktop on a real PC; Roblox and rustup stay out of it ([docs/app-catalog.md](docs/app-catalog.md)
  says why).
- An app installed from Install apps shows ✓ Installed only once Cairn's check finds it in Windows'
  list of installed apps. Until the check that follows the batch ends it shows "winget reported
  success. The next check shows whether Windows lists it."; if that check doesn't find it,
  "winget reported success, but Windows doesn't list it yet. Choose Check again in a minute." This
  also fixes Discord and Slack, which Cairn 0.2.0 could show as installed as soon as their
  installers returned. An install that needs a restart also counts as installed only once a check
  lists it, and a check that could not read winget's list of installed apps proves nothing. An app
  removed later loses ✓ Installed at the next check, an app whose install ended is no longer ticked,
  and the install confirmation says that runtimes winget adds get no rows of their own in
  History › Activity.
- After a batch of app installs or updates, Cairn reads Windows' own pending-restart flags and
  says "Windows reports a pending restart; restart Windows to finish." on the batch result and on
  the row of every app that installed or updated, when the flags appeared during the batch, even
  when winget's result did not carry the restart (Docker Desktop's install, for example);
  `optctl updates` prints the same line.
- Installing Microsoft Teams after Debloat removed it names that removal, and winget error
  0x8A15005F has its own explanation.

### Removed

- Retired tweaks are no longer offered or applied, and leave the modes and the starter profiles;
  changes earlier versions made with them can still be undone:
  - **Disable Cortana** (`privacy.cortana`): Cairn found no effect of this policy on Windows 11.
  - **Turn off Activity History** (`privacy.activity_history`), **Remove web results from Start
    search** (`privacy.web_search`), **Leave the Customer Experience Improvement Program**
    (`privacy.ceip`), **Prioritise games in multimedia scheduling** (`gaming.multimedia_scheduling`)
    and **Turn off network throttling for multimedia** (`gaming.network_throttling`): Cairn found
    no way to observe an effect of these settings on Windows 11, so it no longer offers them.
  - **Remove the startup app delay** (`performance.startup_delay`): Cairn measured no earlier
    start of startup programs on Windows 11, so it no longer offers this.
  - **Remove Widgets** (`interface.widgets`): Windows 11 no longer lets programs set this policy,
    so Cairn no longer offers it (Windows 11 26H2 refuses the write with "Access is denied", even
    with administrator rights).
  - **Hide the Edge sidebar** (`interface.edge_sidebar`): Microsoft retired the Edge sidebar app
    list in 2026; the policy no longer changes anything Cairn can observe.
- **Opera** left the default list of Install apps: it installs Browser Assistant, a bundled
  promotional component. An installed Opera still appears in App updates, an account's own list
  keeps it, and Edit list can add `Opera.Opera`. VeraCrypt, iTunes, Battle.net and Amazon Music were
  considered and left out ([docs/app-catalog.md](docs/app-catalog.md) says why).

### Security

- When Cairn runs as administrator, the web pages it opens (the checkup's links to Microsoft's
  pages and the project page in About) open in the browser without administrator rights, through
  the desktop's own shell.
- Cairn starts and shows built-in images when Pillow cannot load, for example when Windows blocks
  Pillow's modules, which their authors do not sign.

### Known limitations

- Cairn 0.3.0 is not code-signed, so Microsoft Defender SmartScreen may warn about the setup and
  Smart App Control can block it.
- The checks were made on one PC with Windows 11 Home 26H2 (build 26300). Windows 11 24H2 and
  25H2 share its servicing family (26100) and its cumulative updates, so the records stand for
  them, but they were not checked separately, and neither were Pro, Enterprise and Education.
- Settings that Microsoft documents only for Pro and higher are not offered on Home when Home
  ignores them (*Not on this edition of Windows*); when Home could only show them as managed in
  Settings, they are not recommended and say so.
- Consumer editions of Microsoft 365 and Office ignore the Office policies, so the three Office
  tweaks are not offered there.
- Turn off Recall snapshots (needs a Copilot+ PC) and the legacy Copilot policy (Windows 11
  23H2 or earlier) could not run on the test PC, so their records show them not offered there
  and repeat what Microsoft documents. Other effects could not be shown there either, so those
  records repeat what Microsoft documents and the checks show only that the values were set,
  kept after a restart and undone exactly: the Office policies (its Microsoft 365 is a consumer
  edition); Stop feedback prompts and Stop collecting typing and inking data (26H2 has no
  'Feedback frequency' control and no inking personalization page); Turn off background game
  recording (the Xbox Game Bar app was not installed); Turn off online speech recognition (it was
  already off); Turn on hardware-accelerated GPU scheduling (the switch was already on); Stop Edge
  asking to be the default browser (Edge was already the default browser there); the Start,
  Settings and silent-install parts of Stop suggested apps and ads (only its tips checkbox could
  be watched); the Microsoft Rewards and Edge Insider parts of Hide Microsoft Rewards in Edge
  (neither was shown there, while the Acrobat button it also hides was); and the File Explorer
  Home part of Hide account-based files in File Explorer (the Recommended section was off). Turn
  off Fast Startup was not watched: its check restarted Windows, which always starts fully, made
  no shutdown and did not look at Power Options, so its record also repeats what Microsoft
  documents and shows only that the value was set, kept after a restart and undone exactly. Turn
  off the Sticky, Filter and Toggle Keys shortcuts was seen only in its own state (the shortcuts
  were already off there), so its apply and undo were not exercised. Turn on Game Mode was
  already on there (Game Mode is on by default), so its check could show only that Settings still
  read On after the apply; it stays recommended as Gaming Mode's only change. Apart from it,
  Cairn recommends none of the tweaks whose effect could not be shown at all or was not watched.

## 0.2.0 — 2026-10-01

A Windows 11 maintenance app with a journal-first undo model: the original value of every change
is recorded in a local SQLite journal (schema 5) before it is made, every change can be undone on
its own from History or all at once with Revert All Changes, and what cannot be undone is marked,
confirmed first and logged.

- Sixteen sections in four groups: Dashboard (a native C++ monitor of CPU, memory and processes),
  System, Security (a read-only checkup of 24 checks with a score and fixes) and Boot history;
  Optimize (57 tweaks in four groups, including Office and Edge privacy policies, and the modes
  Privacy Mode, Gaming Mode, Max Performance and Clean Interface), Apps (Store app removal),
  Startup, Permissions (a guide to Windows' own app permissions) and Network (per-adapter DNS
  presets that can be undone, Flush DNS cache, Renew lease, Reset network stack); Cleanup, Storage
  (disk speed test, space analyzer, duplicate finder), Updates (app updates and installs with
  winget from a list of 35 apps in seven categories, and five Windows Update settings that can be
  undone), Tools (System File Checker, DISM, Optimize drive, Retrim, Check disk) and Maintenance (a
  weekly scheduled task that cleans and runs read-only checks); History and Profiles (the starter
  profiles Gaming, Privacy and Clean, and profile files).
- A setup that installs for all users into `C:\Program Files\Cairn` with a private Python 3.12,
  upgrades in place, refuses downgrades and keeps the change history on uninstall, and the command
  line `optctl.exe`.
