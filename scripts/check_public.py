"""Checks the working tree for personal data and machine identifiers before it is published.

Walks every file of the repository that Git would publish (build output, virtual environments,
caches, logs, the local data folder and the deployed native binaries are skipped) and fails on:

- the user name, the computer name and the profile folder of the account that runs the check;
- the identifiers of this PC: the account and machine SIDs, the MachineGuid, the product
  IdentifyingNumber and UUID, the BIOS, baseboard, disk, memory and monitor serial numbers, the
  MAC addresses of the network adapters (written AA-BB-..., AA:BB:... or as 12 hex digits) and
  their IPv6 addresses; an identifier written with 16 hex digits or more also matches in part,
  when 12 of its digits in a row appear in a line whatever separates them;
- the hardware names of this PC: the model, family and SKU, the baseboard product, the BIOS
  version, the memory part numbers, the disk models and firmware revisions, the monitor names,
  the descriptions of the physical network adapters and the PCI subsystem ids of the display
  adapters (names that contain a digit, matched as whole words);
- any text given with --forbid;
- the DNS resolver the network fixtures used to name;
- a path under C:\\Users other than C:\\Users\\Test (and Windows' own Public, Default and All Users);
- an e-mail address other than a GitHub noreply address;
- a MAC address outside the documentation range 00-00-5E-00-53-00 to 00-00-5E-00-53-FF.

Every value is read at run time and kept in memory: neither this script nor its output contains
one. A hit prints the file, the line (or "binary") and the kind of rule that matched, never the
matched text. PC identifiers shorter than six characters and firmware placeholders such as
"To be filled by O.E.M." are skipped; the user and computer names match as whole words, whatever
their length. Binary files are searched only for the values of six characters or more, as ASCII
and UTF-16 text.

Run it from the repository with the development environment's Python:

    .venv\\Scripts\\python.exe scripts\\check_public.py [--forbid TEXT ...]

Exit code 0: nothing found; 1: something found; 2: the check could not read an identifier
source or a file, so it is incomplete.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import subprocess
import sys
from collections.abc import Collection, Iterator
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Folders skipped at the top of the repository, and at any depth; they mirror .gitignore.
SKIP_TOP_DIRS = {".git", ".venv", "target", "build", "data"}
SKIP_ANY_DIRS = {"__pycache__", ".pytest_cache", ".ruff_cache", ".vs", ".claude"}
SKIP_SUFFIXES = (".log", ".pyc", ".rs.bk")
NATIVE_DIR = ("ui", "optimizer", "native")
NATIVE_SUFFIXES = (".dll", ".pyd", ".pdb", ".exe")
INSTALLER_OUTPUT = ("installer", "Output")

MIN_LENGTH = 6
BINARY_SNIFF = 8192
PLACEHOLDERS = {
    "to be filled by o.e.m.",
    "default string",
    "system product name",
    "system manufacturer",
    "system version",
    "base board product name",
    "undefined",
    "system serial number",
    "chassis serial number",
    "base board serial number",
    "serial number",
    "not specified",
    "not applicable",
    "not available",
    "unknown",
    "invalid",
    "none",
    "123456789",
    "0123456789",
    "1234567890",
    "03000200-0400-0500-0006-000700080009",
}
# Only zeros, F digits and separators: blank firmware fields and unset addresses.
BLANK_VALUE = re.compile(r"(?i)^[0f\s.:_-]*$")
# Identifiers written only in hex digits and separators (serial numbers, UUIDs, GUIDs, IPv6
# addresses) with at least HEX_SOURCE digits also match by any HEX_RUN of their digits in a row;
# runs of fewer than four different digits (zero padding) are left out.
HEX_SPELLING = re.compile(r"[0-9A-Fa-f{}()._:\s-]+")
NOT_HEX = re.compile(r"[^0-9A-Fa-f]")
HEX_SOURCE = 16
HEX_RUN = 12
# Sources whose values are hardware names rather than identifiers.
HARDWARE_SOURCES = ("hardware names", "monitor names")
DIGIT = re.compile(r"\d")

# Windows' own profile folders under C:\Users, and the fixture account.
ALLOWED_PROFILES = {"test", "public", "default", "all"}
# Placeholders such as $env:USERNAME or %USERNAME% name no account.
PLACEHOLDER_START = ("$", "%", "…")
SEPARATOR = r"(?:\\\\|\\|/)+"
USER_PATH = re.compile(
    r"(?i)(?<![a-z])[a-z]:" + SEPARATOR + "users" + SEPARATOR + r"([^\\/\s\"'`<>|:*?;,()\[\]{}]+)"
)
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")
NOREPLY_DOMAIN = "users.noreply.github.com"
HEX_PAIR = "[0-9A-Fa-f]{2}"
MAC = re.compile(
    r"(?<![0-9A-Fa-f:-])" + HEX_PAIR + r"([-:])(?:" + HEX_PAIR + r"\1){4}" + HEX_PAIR + r"(?![0-9A-Fa-f:-])"
)
# RFC 7042: unicast and multicast MAC addresses for documentation.
DOCUMENTATION_MACS = ("00-00-5E-00-53-", "01-00-5E-90-10-")
OLD_RESOLVER = re.compile(r"(?<![\d.])" + re.escape(".".join(("205", "171", "2", "26"))) + r"(?![\d])")

# Read-only queries; the output is a JSON object of source name -> list of values (null when
# the source could not be read).
IDENTIFIER_QUERY = r"""
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$found = [ordered]@{}
function Read-Values([string]$Name, [scriptblock]$Read) {
    try { $found[$Name] = @(& $Read | Where-Object { $_ } | ForEach-Object { [string]$_ }) }
    catch { $found[$Name] = $null }
}
function Text-Of([object]$Codes) {
    if ($Codes) { -join ($Codes | Where-Object { $_ -ne 0 } | ForEach-Object { [char]$_ }) }
}
Read-Values 'account SIDs' {
    $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $identity.User.Value
    $identity.Groups | ForEach-Object { $_.Value } | Where-Object { $_ -match '^S-1-(5-21|11-96|12-1)-' }
}
Read-Values 'product identifying number and UUID' {
    Get-CimInstance -ClassName Win32_ComputerSystemProduct | ForEach-Object { $_.IdentifyingNumber; $_.UUID }
}
Read-Values 'BIOS serial number' {
    Get-CimInstance -ClassName Win32_BIOS | ForEach-Object { $_.SerialNumber }
}
Read-Values 'baseboard serial number' {
    Get-CimInstance -ClassName Win32_BaseBoard | ForEach-Object { $_.SerialNumber }
}
Read-Values 'disk serial numbers' {
    Get-CimInstance -ClassName Win32_DiskDrive | ForEach-Object { $_.SerialNumber }
}
Read-Values 'memory serial numbers' {
    Get-CimInstance -ClassName Win32_PhysicalMemory | ForEach-Object { $_.SerialNumber }
}
Read-Values 'monitor serial numbers' {
    Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorID |
        ForEach-Object { Text-Of $_.SerialNumberID }
}
Read-Values 'hardware names' {
    Get-CimInstance -ClassName Win32_ComputerSystem |
        ForEach-Object { $_.Model; $_.SystemFamily; $_.SystemSKUNumber }
    Get-CimInstance -ClassName Win32_ComputerSystemProduct | ForEach-Object { $_.Name; $_.Version }
    Get-CimInstance -ClassName Win32_BaseBoard | ForEach-Object { $_.Product }
    Get-CimInstance -ClassName Win32_BIOS | ForEach-Object { $_.SMBIOSBIOSVersion }
    Get-CimInstance -ClassName Win32_PhysicalMemory | ForEach-Object { $_.PartNumber }
    Get-CimInstance -ClassName Win32_DiskDrive | ForEach-Object { $_.Model; $_.FirmwareRevision }
    Get-NetAdapter -Physical | ForEach-Object { $_.InterfaceDescription }
    Get-CimInstance -ClassName Win32_VideoController |
        ForEach-Object { if ($_.PNPDeviceID -match 'SUBSYS_[0-9A-F]{8}') { $Matches[0] } }
}
Read-Values 'monitor names' {
    Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorID |
        ForEach-Object { Text-Of $_.UserFriendlyName }
}
Read-Values 'MAC addresses' {
    Get-NetAdapter -IncludeHidden | ForEach-Object { $_.MacAddress }
}
Read-Values 'IPv6 addresses' {
    Get-NetIPAddress -AddressFamily IPv6 | ForEach-Object { $_.IPAddress }
}
$found | ConvertTo-Json -Compress
"""


@dataclass(frozen=True)
class Needle:
    """A value searched for, and the rule name a hit reports."""

    rule: str
    value: str
    whole_word: bool = False

    def pattern(self) -> re.Pattern[str]:
        text = re.escape(self.value)
        if self.whole_word:
            text = rf"(?<![A-Za-z0-9]){text}(?![A-Za-z0-9])"
        return re.compile(text, re.IGNORECASE)


@dataclass(frozen=True)
class Hit:
    path: str
    line: int | None
    rule: str

    def __str__(self) -> str:
        where = "binary" if self.line is None else str(self.line)
        return f"{self.path}:{where}: {self.rule}"


def usable(value: str) -> bool:
    """True for a value worth searching for: long enough and not a firmware placeholder."""
    value = value.strip()
    return len(value) >= MIN_LENGTH and value.lower() not in PLACEHOLDERS and not BLANK_VALUE.match(value)


def environment_needles(env: dict[str, str]) -> tuple[list[Needle], list[str]]:
    """The user name, computer name and profile folder of this account; missing ones by name."""
    needles: list[Needle] = []
    missing: list[str] = []
    for variable, rule in (("USERNAME", "user name"), ("COMPUTERNAME", "computer name")):
        value = env.get(variable, "").strip()
        if value:
            # Whole words: a short name would otherwise match inside ordinary words.
            needles.append(Needle(rule, value, whole_word=True))
        else:
            missing.append(variable)
    profile = env.get("USERPROFILE", "").strip().rstrip("\\/")
    if profile:
        for spelling in {profile, profile.replace("\\", "/"), profile.replace("\\", "\\\\")}:
            needles.append(Needle("user profile folder", spelling))
    else:
        missing.append("USERPROFILE")
    return needles, missing


def sid_needles(sids: list[str]) -> list[str]:
    """Account SIDs as searched: the machine or domain part of S-1-5-21 SIDs, others whole."""
    values = []
    for sid in sids:
        parts = sid.strip().split("-")
        if parts[:4] == ["S", "1", "5", "21"] and len(parts) >= 8:
            values.append("-".join(parts[:7]))
        else:
            values.append(sid.strip())
    return values


def mac_spellings(mac: str) -> list[str]:
    digits = re.sub(r"[^0-9A-Fa-f]", "", mac)
    if len(digits) != 12:
        return [mac]
    pairs = [digits[i : i + 2] for i in range(0, 12, 2)]
    return ["-".join(pairs), ":".join(pairs), digits]


def machine_guid() -> str | None:
    import winreg

    try:
        with winreg.OpenKey(
            winreg.HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Cryptography",
            0,
            winreg.KEY_READ | winreg.KEY_WOW64_64KEY,
        ) as key:
            value, _ = winreg.QueryValueEx(key, "MachineGuid")
    except OSError:
        return None
    return str(value)


def query_identifiers() -> dict[str, list[str] | None]:
    """Runs IDENTIFIER_QUERY in Windows PowerShell; an empty dict when it cannot run."""
    system_root = os.environ.get("SystemRoot", r"C:\Windows")
    powershell = Path(system_root) / "System32" / "WindowsPowerShell" / "v1.0" / "powershell.exe"
    encoded = base64.b64encode(IDENTIFIER_QUERY.encode("utf-16-le")).decode("ascii")
    command = [str(powershell), "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"]
    try:
        done = subprocess.run(
            [*command, "-EncodedCommand", encoded], capture_output=True, timeout=180, check=False
        )
        found = json.loads(done.stdout.decode("utf-8-sig", errors="replace"))
    except (OSError, subprocess.SubprocessError, ValueError):
        return {}
    if not isinstance(found, dict):
        return {}
    result: dict[str, list[str] | None] = {}
    for name, values in found.items():
        if values is None:
            result[name] = None
        elif isinstance(values, list):
            result[name] = [str(v) for v in values]
        else:
            result[name] = [str(values)]
    return result


def hex_runs(values: list[str]) -> set[str]:
    """Every run of HEX_RUN digits (uppercase) of the values written only in hex digits and
    separators that have HEX_SOURCE digits or more."""
    runs: set[str] = set()
    for value in values:
        if not HEX_SPELLING.fullmatch(value):
            continue
        digits = NOT_HEX.sub("", value).upper()
        if len(digits) < HEX_SOURCE:
            continue
        for start in range(len(digits) - HEX_RUN + 1):
            run = digits[start : start + HEX_RUN]
            if len(set(run)) >= 4:
                runs.add(run)
    return runs


def has_hex_run(line: str, runs: Collection[str]) -> bool:
    """True when the hex digits of the line, separators removed, hold one of the runs."""
    digits = NOT_HEX.sub("", line).upper()
    return any(digits[start : start + HEX_RUN] in runs for start in range(len(digits) - HEX_RUN + 1))


def machine_needles() -> tuple[list[Needle], set[str], list[str]]:
    """This PC's identifiers and hardware names, the hex runs of its long identifiers, and the
    sources that could not be read, by name."""
    values: list[str] = []
    names: list[str] = []
    failed: list[str] = []
    guid = machine_guid()
    if guid is None:
        failed.append("MachineGuid")
    else:
        values.append(guid)
    found = query_identifiers()
    if not found:
        failed.append("Windows PowerShell identifier query")
    for name, items in found.items():
        if items is None:
            failed.append(name)
            continue
        if name == "account SIDs":
            values.extend(sid_needles(items))
        elif name == "MAC addresses":
            for mac in items:
                if usable(mac):
                    values.extend(mac_spellings(mac))
        elif name == "IPv6 addresses":
            values.extend(a.split("%", 1)[0] for a in items if a.split("%", 1)[0] not in ("::1", "::"))
        elif name in HARDWARE_SOURCES:
            names.extend(items)
        else:
            values.extend(items)
    unique = sorted({v.strip() for v in values if usable(v)}, key=str.lower)
    # Names without a digit ("Virtual Machine", "Surface Laptop") are common words, not models.
    hardware = {n.strip() for n in names if usable(n) and DIGIT.search(n)}
    needles = [Needle("machine identifier", v) for v in unique]
    needles += [
        Needle("hardware name", n, whole_word=True) for n in sorted(hardware - set(unique), key=str.lower)
    ]
    return needles, hex_runs(unique), failed


def forbidden_needles(texts: list[str]) -> list[Needle]:
    return [Needle(f"forbidden text {i}", t) for i, t in enumerate(texts, 1) if t.strip()]


def repository_files(root: Path) -> Iterator[Path]:
    for dirpath, dirnames, filenames in os.walk(root):
        here = Path(dirpath)
        rel = here.relative_to(root)
        dirnames[:] = sorted(
            d
            for d in dirnames
            if not (rel == Path(".") and d in SKIP_TOP_DIRS)
            and d not in SKIP_ANY_DIRS
            and not d.endswith(".egg-info")
            and (rel / d).parts != INSTALLER_OUTPUT
        )
        for name in sorted(filenames):
            lower = name.lower()
            if lower.endswith(SKIP_SUFFIXES):
                continue
            if rel.parts == NATIVE_DIR and lower.endswith(NATIVE_SUFFIXES):
                continue
            yield here / name


def decode_text(data: bytes) -> str | None:
    """The file's text, or None for a binary file."""
    if data.startswith((b"\xff\xfe", b"\xfe\xff")):
        return data.decode("utf-16", errors="replace")
    if b"\0" in data[:BINARY_SNIFF]:
        return None
    return data.decode("utf-8", errors="replace")


def line_rules(line: str) -> Iterator[str]:
    """The generic rules a line breaks."""
    if OLD_RESOLVER.search(line):
        yield "old fixture DNS resolver"
    for match in USER_PATH.finditer(line):
        name = match.group(1).rstrip(".…").lower()
        if name and name not in ALLOWED_PROFILES and not name.startswith(PLACEHOLDER_START):
            yield "path under C:\\Users other than Test"
    for match in EMAIL.finditer(line):
        if match.group(0).lower().rsplit("@", 1)[1] != NOREPLY_DOMAIN:
            yield "e-mail address"
    for match in MAC.finditer(line):
        mac = match.group(0).upper().replace(":", "-")
        if not mac.startswith(DOCUMENTATION_MACS) and not BLANK_VALUE.match(mac):
            yield "MAC address"


def check_file(
    path: Path,
    rel: str,
    needles: list[Needle],
    patterns: list[re.Pattern[str]],
    runs: Collection[str] = frozenset(),
) -> list[Hit]:
    data = path.read_bytes()
    text = decode_text(data)
    hits: list[Hit] = []
    if text is None:
        lowered = data.lower()
        for needle in needles:
            if len(needle.value) < MIN_LENGTH:
                continue
            value = needle.value.lower()
            if value.encode("utf-8") in lowered or value.encode("utf-16-le") in lowered:
                hits.append(Hit(rel, None, needle.rule))
        return hits
    for number, line in enumerate(text.splitlines(), 1):
        rules = [
            needle.rule for needle, pattern in zip(needles, patterns, strict=True) if pattern.search(line)
        ]
        if runs and has_hex_run(line, runs):
            rules.append("part of a machine identifier")
        rules.extend(line_rules(line))
        hits.extend(Hit(rel, number, rule) for rule in dict.fromkeys(rules))
    return hits


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--forbid",
        action="append",
        default=[],
        metavar="TEXT",
        help="also fail on this text (case ignored); may be given several times",
    )
    parser.add_argument("--root", type=Path, default=ROOT, help="folder to check (default: the repository)")
    args = parser.parse_args(argv)
    if sys.platform != "win32":
        print("check_public.py reads this PC's identifiers from Windows; run it on Windows.", file=sys.stderr)
        return 2

    env_needles, missing = environment_needles(dict(os.environ))
    pc_needles, runs, failed = machine_needles()
    needles = env_needles + pc_needles + forbidden_needles(args.forbid)
    patterns = [needle.pattern() for needle in needles]
    problems = [f"environment variable {name} is not set" for name in missing]
    problems += [f"could not read {source}" for source in failed]

    root = args.root.resolve()
    hits: list[Hit] = []
    count = 0
    for path in repository_files(root):
        rel = path.relative_to(root).as_posix()
        count += 1
        try:
            hits.extend(check_file(path, rel, needles, patterns, runs))
        except OSError as error:
            problems.append(f"could not read {rel}: {error.strerror or 'error'}")

    for hit in hits:
        print(hit)
    for problem in problems:
        print(f"warning: {problem}", file=sys.stderr)
    print(
        f"{count} files checked against {len(pc_needles)} machine identifiers and hardware names, "
        f"{len(env_needles)} account values and {len(args.forbid)} forbidden texts: "
        f"{len(hits)} hits in {len({h.path for h in hits})} files."
    )
    if hits:
        return 1
    return 2 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
