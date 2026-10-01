"""The live-state snapshot `scripts/live_state_snapshot.ps1`, read as text: it records every value an
undo of the app-permission records earlier builds left writes (History still undoes them), at the
paths those records name and History recognizes, and the consent store the permissions guide reads.
Nothing is run."""

from __future__ import annotations

import re
from pathlib import Path

from optimizer.widgets import history

from .fake_permissions import DEVICE_STORE, LOCATION_SENSOR, SENSOR_VALUE, FakePermissions

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "live_state_snapshot.ps1"
STORE_RS = ROOT / "crates" / "core" / "src" / "permissions" / "store.rs"
TABLE = "[ordered]@{"


def text() -> str:
    return SCRIPT.read_text(encoding="utf-8")


def rust_constant(name: str) -> str:
    """A `&str` constant of the engine's permissions layout."""
    match = re.search(rf'const {name}: &str\s*=\s*r?"([^"]+)";', STORE_RS.read_text(encoding="utf-8"))
    assert match, name
    return match.group(1)


def recorded_rows() -> list[dict[str, object]]:
    """The journal rows of a Store-apps switch and of the location switch for the whole PC, as
    earlier builds recorded them."""
    fake = FakePermissions.__new__(FakePermissions)
    fake._init_permissions({"permission_recorded": {"camera:apps": "allow", "location:device": "allow"}})
    return fake._permission_export()


def function(name: str) -> str:
    """A top-level `function <name>(...)`, from its first line to its closing brace in column 0."""
    match = re.search(rf"^function {re.escape(name)}\(.*?^\}}", text(), re.MULTILINE | re.DOTALL)
    assert match, name
    return match.group(0)


def tables(source: str) -> list[dict[str, str]]:
    """Every `[ordered]@{ ... }` literal of `source`, in order, as field -> expression."""
    found = []
    start = source.find(TABLE)
    while start >= 0:
        depth = 0
        for end in range(start + len(TABLE) - 1, len(source)):
            depth += {"{": 1, "}": -1}.get(source[end], 0)
            if depth == 0:
                break
        fields = {}
        for part in re.split(r"[;\n]", source[start + len(TABLE) : end]):
            name, equals, expression = part.partition("=")
            if equals:
                fields[name.strip()] = expression.strip()
        found.append(fields)
        start = source.find(TABLE, end)
    return found


def snapshot() -> dict[str, str]:
    """Field -> expression of the table the script writes as JSON."""
    return tables(text().split("\n$snapshot = ", 1)[1])[0]


def test_every_consent_store_records_its_own_value_and_time() -> None:
    # A record of a switch names the store key's own Value and LastSetTime (HKLM for the whole
    # PC, HKCU for Store apps), as a record of an app or of NonPackaged names its subkey's.
    store = DEVICE_STORE
    assert store.lower() == rust_constant("USER_STORE").lower(), "the store the guide reads"
    assert history._CONSENT_STORE in f"\\{store.lower()}\\", "History recognizes its records"
    consent = [r for r in recorded_rows() if str(r["key_path"]).lower().startswith(store.lower())]
    assert {(r["hive"], r["value_name"]) for r in consent} == {
        ("HKCU", "Value"),
        ("HKCU", "LastSetTime"),
        ("HKLM", "Value"),
        ("HKLM", "LastSetTime"),
    }
    value, time = "Value", "LastSetTime"
    body = function("Get-ConsentStore")
    assert f'$path = "{store}\\$Capability"' in body
    missing, present = tables(body)
    for fields in (missing, present):
        assert list(fields) == ["Capability", "Hive", "Exists", value, time, "Subkeys"]
    assert missing[value] == missing[time] == "$null"
    assert present[value] == f"Get-RegData $key '{value}'"
    assert present[time] == f"Get-RegData $key '{time}'"
    entries = tables(function("Get-ConsentEntries"))[0]
    assert (entries[value], entries[time]) == (f"Get-RegData $sub '{value}'", f"Get-RegData $sub '{time}'")
    assert "foreach ($hive in @('HKLM', 'HKCU'))" in text()
    assert "foreach ($capability in @('webcam', 'microphone', 'location'))" in text()
    assert snapshot()["ConsentStores"] == "$consentStores"


def test_the_location_sensor_override_is_recorded() -> None:
    # A record of the location switch for the whole PC also names the location sensor's override.
    [sensor] = [r for r in recorded_rows() if r["value_name"] == SENSOR_VALUE]
    assert (sensor["hive"], sensor["key_path"]) == ("HKLM", LOCATION_SENSOR)
    assert history.permission_title("HKLM", LOCATION_SENSOR) == "Location services on this PC"
    call = f"Get-RegNamedValues 'HKLM' '{LOCATION_SENSOR}' @('{SENSOR_VALUE}')"
    assignment = re.search(rf"^\$(\w+) = {re.escape(call)}$", text(), re.MULTILINE)
    assert assignment, call
    assert snapshot()["LocationSensorOverride"] == f"${assignment.group(1)}"
    # Read through the 64-bit view, with the braces of the key name taken literally.
    assert "[Microsoft.Win32.RegistryView]::Registry64" in function("Open-RegKey")
    assert "$base.OpenSubKey($Path, $false)" in function("Open-RegKey")
