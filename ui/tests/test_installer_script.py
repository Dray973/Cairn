"""The Inno Setup script `installer/cairn.iss`, read as text: a fixed install folder, upgrades that
delete only inside Cairn's own folder, no deletion of user data, the uninstall checks and the
identifiers the engine shares with it. Nothing is compiled or run."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "installer" / "cairn.iss"
APP_RS = ROOT / "crates" / "core" / "src" / "app.rs"


def text() -> str:
    return SCRIPT.read_text(encoding="utf-8")


def sections() -> dict[str, list[str]]:
    """Lines of each `[Section]`, comments and blank lines removed."""
    found: dict[str, list[str]] = {}
    current = ""
    for raw in text().splitlines():
        line = raw.strip()
        if not line or line.startswith(";"):
            continue
        heading = re.fullmatch(r"\[(\w+)\]", line)
        if heading:
            current = heading.group(1)
            found.setdefault(current, [])
        elif current:
            found[current].append(line)
    return found


def setup() -> dict[str, str]:
    values = {}
    for line in sections()["Setup"]:
        if "=" in line and not line.startswith("#"):
            key, value = line.split("=", 1)
            values[key.strip()] = value.strip()
    return values


def rust_constant(name: str) -> str:
    match = re.search(rf'pub const {name}: &str\s*=\s*r?"([^"]+)";', APP_RS.read_text(encoding="utf-8"))
    assert match, name
    return match.group(1)


def test_the_install_folder_is_fixed() -> None:
    values = setup()
    assert values["DefaultDirName"] == r"{autopf}\{#AppName}"
    assert values["DisableDirPage"] == "yes"
    assert values["UsePreviousAppDir"] == "no"
    assert values["PrivilegesRequired"] == "admin"
    assert values["ArchitecturesInstallIn64BitMode"] == "x64compatible"


def test_identifiers_match_the_engine() -> None:
    app_id = rust_constant("INSTALLER_APP_ID")
    assert setup()["AppId"] == "{" + app_id, "a literal brace is doubled in [Setup]"
    uninstall_key = rust_constant("UNINSTALL_KEY")
    assert f'#define UninstallKey "{uninstall_key}"' in text()
    assert uninstall_key.endswith(app_id + "_is1")


def test_setup_waits_for_the_window_and_never_forces_it() -> None:
    values = setup()
    assert values["AppMutex"] == "Cairn.Instance"
    assert values["CloseApplications"] == "yes"
    assert values["RestartApplications"] == "no"


def test_every_install_delete_is_limited_to_cairns_folder() -> None:
    entries = sections()["InstallDelete"]
    assert entries, "an upgrade removes the old runtime and app"
    for entry in entries:
        assert entry.endswith("Check: CairnIsInstalledHere"), entry
        assert '"{app}\\' in entry, entry
    assert "function CairnIsInstalledHere(): Boolean;" in text()
    body = text().split("function CairnIsInstalledHere(): Boolean;", 1)[1].split("end;", 1)[0]
    assert "FileExists(ExpandConstant('{app}\\Cairn.exe'))" in body
    assert "'InstallLocation'" in body and "HKLM64" in body


def test_no_user_data_is_ever_deleted() -> None:
    script = text()
    lowered = script.lower()
    assert "deltree" not in lowered
    assert "[uninstalldelete]" not in lowered
    assert "{localappdata}" not in lowered
    assert "{userappdata}" not in lowered
    assert "curuninstallstepchanged" not in lowered
    assert "pcoptimizer" not in lowered, "the data folder is never named"


def test_uninstall_warns_unless_nothing_is_pending() -> None:
    script = text()
    body = script.split("function InitializeUninstall(): Boolean;", 1)[1]
    assert "RunOptctl('journal pending', Code)" in body
    assert "UninstallSilent()" in body
    # Exit 3 asks about pending changes; an Exec failure or any other non-zero code says the
    # check failed; only 0 continues without a question.
    assert "Code = ExitPending" in body and "ExitPending = 3;" in script
    assert body.count("UninstallAnyway(Format(CouldNotCheckText, [Code]))") == 2
    assert "Code <> 0" in body
    assert "could not check for changes that can still be undone" in script
    assert "MB_DEFBUTTON2" in script, "No is the default"


def test_a_running_maintenance_run_is_waited_for() -> None:
    script = text()
    assert "ExitMaintenanceRunning = 4;" in script
    assert "RunOptctl('maintenance status --running', Code)" in script
    assert "Scheduled maintenance is running. Wait for it to finish, then choose Retry." in script
    assert "MB_RETRYCANCEL" in script
    uninstall = script.split("function InitializeUninstall(): Boolean;", 1)[1]
    assert uninstall.index("WaitForMaintenance()") < uninstall.index("journal pending")
    prepare = script.split("function PrepareToInstall(var NeedsRestart: Boolean): String;", 1)[1]
    prepare = prepare.split("end;", 1)[0]
    assert "FileExists(ExpandConstant('{app}\\optctl.exe'))" in prepare
    assert "WaitForMaintenance()" in prepare


def test_uninstall_removes_the_maintenance_tasks() -> None:
    entries = sections()["UninstallRun"]
    assert len(entries) == 1
    assert 'Filename: "{app}\\optctl.exe"' in entries[0]
    assert 'Parameters: "uninstall-cleanup --yes"' in entries[0]
    assert "waituntilterminated" in entries[0] and "runhidden" in entries[0]


def test_the_finish_page_opens_cairn_with_setups_rights() -> None:
    entries = sections()["Run"]
    assert len(entries) == 1
    assert 'Filename: "{app}\\Cairn.exe"' in entries[0]
    flags = entries[0].split("Flags:", 1)[1].split(";", 1)[0].split()
    assert {"nowait", "postinstall", "skipifsilent"} <= set(flags)
    # A postinstall entry otherwise starts as the user who started setup, and the launcher
    # would then ask for administrator rights a second time.
    assert "runascurrentuser" in flags
    assert "runasoriginaluser" not in flags


def test_downgrades_are_refused() -> None:
    body = text().split("function InitializeSetup(): Boolean;", 1)[1]
    assert "'DisplayVersion'" in body and "ComparePackedVersion(Have, Want) > 0" in body
    assert "A newer version of Cairn (" in body
    assert "Result := False;" in body


def test_shortcuts_carry_the_taskbar_identity() -> None:
    icons = sections()["Icons"]
    assert icons and all('AppUserModelID: "Cairn.App"' in line for line in icons)
    assert all('Filename: "{app}\\Cairn.exe"' in line for line in icons)
    desktop = 'Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked'
    assert sections()["Tasks"] == [desktop]
