# Verification scripts

Helpers for the real-PC checks recorded in [`docs/verification`](../../docs/verification/README.md). None of them applies or undoes a tweak (that is done in Cairn), and none changes a setting. Their output can name apps and folders of the PC they run on, so it goes under `%LOCALAPPDATA%\PCOptimizerDev`, never into the repository (and not into `%TEMP%`, which Cairn's Cleanup empties).

Run the PowerShell scripts from a PowerShell prompt in the repository folder, for example `.\scripts\verification\regdiff.ps1 …`, or with `powershell -NoProfile -ExecutionPolicy Bypass -File …` when they take no list parameter. Run the Python scripts with `py -3.12`.

| File | Used for | What it writes |
|---|---|---|
| `targets.json` | Every registry value (with its key), service and scheduled task that a catalog tweak changes, including the values earlier versions of Cairn set. `scripts\live_state_snapshot.ps1` records all of them, so a snapshot before a check and one after Undo show any difference. | – (it is generated from the catalog; `crates/core/tests/verification_records.rs` fails when it no longer matches and prints the content it must have) |
| `regdiff.ps1` | Finding the values a Windows Settings switch writes: export the keys, change the switch, export again with `-Against` and read the list of values that changed. | `.reg` exports and the list of changes under `-Root` (default `%LOCALAPPDATA%\PCOptimizerDev\regdiff`); nothing is imported |
| `location_access.ps1` | The location tweak: asks Windows whether a desktop app may use location now (`Allowed` before, `Denied` while the tweak is applied, `Allowed` after Undo). Run it without administrator rights, like the apps the setting is about. | nothing; Windows may list Windows PowerShell among the apps that recently asked for location |
| `toast_test.ps1` | The lock-screen notifications tweak: shows a test notification after 20 seconds, so it arrives while the PC is locked. Start it from a PowerShell prompt without administrator rights (an elevated process shows no notification) and press Win+L before the delay ends. | nothing |
| `long_path_test.py` | The long file paths tweak: creates a file in a folder path of more than 300 characters, prints whether that worked, and removes the folder again. Run it in a new console, after the restart the tweak asks for. | `%LOCALAPPDATA%\PCOptimizerDev\lp-test`, removed before it exits |
| `wer_crash_test.py` | The error reporting tweak: starts a Python process that crashes itself and reports whether Windows Error Reporting recorded the crash (`--list` only counts the reports there are). | the crash report Windows Error Reporting writes, when it is on |
| `startup_timing.ps1` | The startup delay tweak: lists how many seconds after File Explorer each Run-key and Startup-folder program of this session started. Run it right after signing in, after a restart with and after one without the tweak. | the report given with `-Out` |

## Examples

```powershell
# Which values does a Settings switch write?
.\scripts\verification\regdiff.ps1 -Keys 'HKCU\Software\Microsoft','HKLM\SOFTWARE\Policies' -Label before
#   ... change the switch in Settings ...
.\scripts\verification\regdiff.ps1 -Keys 'HKCU\Software\Microsoft','HKLM\SOFTWARE\Policies' -Label after -Against before

# Snapshot of every catalog target before and after a check (targets.json is read automatically).
.\scripts\live_state_snapshot.ps1 -Out "$env:LOCALAPPDATA\PCOptimizerDev\baseline\check_before.json"

# Startup programs, right after signing in.
.\scripts\verification\startup_timing.ps1 -Out "$env:LOCALAPPDATA\PCOptimizerDev\scratch\startup-timing-before.json"
```

```text
py -3.12 scripts\verification\long_path_test.py
py -3.12 scripts\verification\wer_crash_test.py
```

A change found with `regdiff.ps1` only shows which value a switch writes. It is not evidence that Windows uses the value: the check in Cairn still has to show the effect (see the evidence levels in `docs/verification/README.md`).
