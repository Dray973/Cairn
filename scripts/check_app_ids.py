"""Checks that the winget ids of Cairn's built-in app lists still exist. Read-only.

Reads DEFAULT_APPS and HELD_APPS from crates/core/src/updates/apps.rs and checks each id:

- by default with `winget show --id <id> --exact --source winget --accept-source-agreements
  --disable-interactivity`, which reads winget's source and installs nothing, and prints the
  version winget would install. With --manifests it also reads that version's installer
  manifest from microsoft/winget-pkgs on GitHub and warns about what matters for Cairn's
  silent installs from an administrator app: an installer that refuses elevation, a required
  install folder (Cairn never passes one), an exe installer without a silent switch, a
  portable package, and an app winget updates only when it is named. They are read from the
  manifest's text for the installer `winget show` reports (its type and download address), so
  they are hints to look at, not verdicts.
- with --github without winget: lists the package's folder
  manifests/<first letter>/<Publisher>/<Name>/ of microsoft/winget-pkgs through the GitHub API.
  Set GITHUB_TOKEN or GH_TOKEN for the API's higher rate limit; without one GitHub allows 60
  requests an hour, fewer than the lists have ids.

The check is for maintainers before a release; nothing in Cairn or its tests runs it, and the
tests use its parsing functions only.

    .venv\\Scripts\\python.exe scripts\\check_app_ids.py [--github] [--manifests] [--ids ID ...]

Exit code 0: every id was found; 1: an id was not found; 2: the check could not finish
(winget is missing, the network or GitHub failed, the rate limit was reached).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
APPS_RS = ROOT / "crates" / "core" / "src" / "updates" / "apps.rs"
LISTS = ("DEFAULT_APPS", "HELD_APPS")
API = "https://api.github.com/repos/microsoft/winget-pkgs/contents"
RAW = "https://raw.githubusercontent.com/microsoft/winget-pkgs/master"
USER_AGENT = "cairn-check-app-ids"
TIMEOUT = 30
SHOW_TIMEOUT = 180
# winget's APPINSTALLER_CLI_ERROR_NO_APPLICATIONS_FOUND.
NO_PACKAGE_FOUND = 0x8A150014
# Apps whose manifests already say winget updates them only when named; others are news.
KNOWN_EXPLICIT_UPGRADE = frozenset(
    i.lower() for i in ("Discord.Discord", "SlackTechnologies.Slack", "Google.AndroidStudio")
)

_LIST = re.compile(r"pub const (\w+): &\[\(&str, &str, AppCategory\)\] = &\[(.*?)\n\];", re.S)
_ENTRY = re.compile(r'\(\s*"([^"]+)",\s*"([^"]*)",\s*AppCategory::(\w+),?\s*\)', re.S)
_VERSION = re.compile(r"^\s*Version:\s*(\S.*?)\s*$", re.M)
_INSTALLER_TYPE = re.compile(r"^\s*Installer Type:\s*(\S+)\s*$", re.M)
_INSTALLER_URL = re.compile(r"^\s*Installer Url:\s*(\S.*?)\s*$", re.M)


@dataclass(frozen=True)
class App:
    list_name: str
    package_id: str
    name: str
    category: str


@dataclass
class Result:
    app: App
    status: str  # "found", "missing" or "error"
    detail: str = ""
    warnings: tuple[str, ...] = ()


class RateLimited(Exception):
    """GitHub refused the request for its rate limit."""


def read_lists(text: str) -> list[App]:
    """The apps of the DEFAULT_APPS and HELD_APPS constants of `text` (apps.rs), in order."""
    apps = []
    for name, body in _LIST.findall(text):
        if name in LISTS:
            apps.extend(App(name, *entry) for entry in _ENTRY.findall(body))
    return apps


def manifest_dir(package_id: str) -> str:
    """The package's folder in microsoft/winget-pkgs: every dot of the id starts a subfolder."""
    return "manifests/" + package_id[0].lower() + "/" + "/".join(package_id.split("."))


def show_command(winget: str, package_id: str) -> list[str]:
    return [
        winget,
        "show",
        "--id",
        package_id,
        "--exact",
        "--source",
        "winget",
        "--accept-source-agreements",
        "--disable-interactivity",
    ]


def show_status(returncode: int, output: str, package_id: str) -> tuple[str, str]:
    """("found", version), ("missing", "") or ("error", why) for a `winget show` that ended
    with `returncode` and printed `output`. winget prints the package as "Name [Id]"."""
    code = returncode & 0xFFFFFFFF
    if code == 0 and f"[{package_id.lower()}]" in output.lower():
        version = _VERSION.search(output)
        return "found", version.group(1) if version else ""
    if code == NO_PACKAGE_FOUND or "no package found" in output.lower():
        return "missing", ""
    return "error", f"winget ended with 0x{code:08X}"


def shown_installer_type(output: str) -> str:
    """The type of the installer winget picked, from the "Installer Type:" line `winget show`
    prints in English; "" when there is none."""
    match = _INSTALLER_TYPE.search(output)
    return match.group(1).lower() if match else ""


def shown_installer_url(output: str) -> str:
    """The download address of the installer winget picked ("Installer Url:"); "" when none."""
    match = _INSTALLER_URL.search(output)
    return match.group(1) if match else ""


def picked_installer(installer_yaml: str, url: str) -> str:
    """The manifest's fields outside its `Installers` list plus the entry of that list whose
    InstallerUrl is `url`, as text; the whole manifest when no entry has that address. Entries
    are the list items at the start of a line ("- Architecture: x64")."""
    head, found, rest = installer_yaml.partition("\nInstallers:\n")
    wanted = urllib.parse.unquote(url).strip().lower()
    if not (found and wanted):
        return installer_yaml
    for entry in re.split(r"\n(?=- )", rest):
        if wanted in urllib.parse.unquote(entry).lower():
            return f"{head}\nInstallers:\n{entry}"
    return installer_yaml


def manifest_warnings(installer_yaml: str, package_id: str, installer_type: str = "") -> list[str]:
    """Hints from an installer manifest's text about Cairn's silent, elevated installs. With
    `installer_type`, the type of the installer winget picked, the hints about the installer's
    type are about that installer; otherwise about any installer the manifest lists."""
    text = installer_yaml.lower()
    warnings = []
    if re.search(r"elevationrequirement:\s*elevationprohibited", text):
        warnings.append("an installer refuses to run from an administrator app (elevationProhibited)")
    if re.search(r"installlocationrequired:\s*true", text):
        warnings.append("the installer needs an install folder, which Cairn never passes")
    if installer_type:
        nested_portable = re.search(r"nestedinstallertype:\s*portable", text) is not None
        portable = installer_type == "portable" or (installer_type == "zip" and nested_portable)
        exe = installer_type == "exe"
    else:
        types = set(re.findall(r"(?:nested)?installertype:\s*([a-z]+)", text))
        portable, exe = "portable" in types, "exe" in types
    if portable:
        warnings.append("a portable package: no Start menu entry, and Windows doesn't list it as installed")
    if exe and not re.search(r"^\s*silent:", text, re.M):
        warnings.append("an exe installer without a silent switch can show its own window")
    explicit = re.search(r"requireexplicitupgrade:\s*true", text)
    if explicit and package_id.lower() not in KNOWN_EXPLICIT_UPGRADE:
        warnings.append("winget now updates it only when it is named (RequireExplicitUpgrade)")
    return warnings


def listing_found(entries: object) -> bool:
    """Whether a GitHub contents listing of a package folder holds a version folder."""
    return isinstance(entries, list) and any(isinstance(e, dict) and e.get("type") == "dir" for e in entries)


def _request(url: str, token: str | None, accept: str) -> bytes | None:
    """The body of `url`, or None for 404; RateLimited for GitHub's limit; OSError otherwise."""
    headers = {"User-Agent": USER_AGENT, "Accept": accept}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
            return bytes(response.read())
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        if e.code in (403, 429) and e.headers.get("x-ratelimit-remaining") == "0":
            raise RateLimited(str(e)) from e
        raise OSError(f"HTTP {e.code} for {url}") from e


def installer_manifest(package_id: str, version: str, token: str | None) -> str | None:
    """The installer manifest of `version`, or of a single-file manifest; None when neither."""
    base = f"{RAW}/{manifest_dir(package_id)}/{version}/{package_id}"
    for url in (f"{base}.installer.yaml", f"{base}.yaml"):
        body = _request(url, token, "text/plain")
        if body is not None:
            return body.decode("utf-8-sig", errors="replace")
    return None


def check_one_local(app: App, winget: str, manifests: bool, token: str | None) -> Result:
    try:
        proc = subprocess.run(
            show_command(winget, app.package_id),
            capture_output=True,
            timeout=SHOW_TIMEOUT,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        return Result(app, "error", str(e))
    output = proc.stdout.decode("utf-8", errors="replace")
    status, detail = show_status(proc.returncode, output, app.package_id)
    if not (manifests and status == "found" and detail):
        return Result(app, status, detail)
    try:
        text = installer_manifest(app.package_id, detail, token)
    except (OSError, RateLimited) as e:
        return Result(app, status, detail, (f"the manifest could not be read: {e}",))
    if text is None:
        return Result(app, status, detail, ("no manifest of this version was found on GitHub",))
    picked = picked_installer(text, shown_installer_url(output))
    hints = manifest_warnings(picked, app.package_id, shown_installer_type(output))
    return Result(app, status, detail, tuple(hints))


def check_local(apps: Sequence[App], winget: str, manifests: bool, token: str | None) -> list[Result]:
    results = []
    for app in apps:
        results.append(check_one_local(app, winget, manifests, token))
        print_result(results[-1])
    return results


def check_github(apps: Sequence[App], token: str | None) -> list[Result]:
    results = []
    for app in apps:
        url = f"{API}/{manifest_dir(app.package_id)}"
        try:
            body = _request(url, token, "application/vnd.github+json")
        except RateLimited:
            results.append(Result(app, "error", "GitHub's rate limit was reached; set GITHUB_TOKEN"))
            print_result(results[-1])
            break
        except OSError as e:
            results.append(Result(app, "error", str(e)))
            print_result(results[-1])
            continue
        if body is None:
            results.append(Result(app, "missing"))
        else:
            entries = json.loads(body.decode("utf-8", errors="replace"))
            found = listing_found(entries)
            results.append(Result(app, "found" if found else "missing", "" if found else "no version folder"))
        print_result(results[-1])
    return results


def print_result(result: Result) -> None:
    label = {"found": "ok", "missing": "MISSING", "error": "error"}[result.status]
    line = f"{label:8} {result.app.list_name:12} {result.app.package_id}"
    if result.detail:
        line += f"  ·  {result.detail}"
    print(line, flush=True)
    for warning in result.warnings:
        print(f"{'':8} warning: {warning}", flush=True)


def exit_code(results: Iterable[Result], expected: int) -> int:
    results = list(results)
    if any(r.status == "missing" for r in results):
        return 1
    if len(results) < expected or any(r.status == "error" for r in results):
        return 2
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Checks the winget ids of Cairn's built-in app lists.")
    parser.add_argument(
        "--github", action="store_true", help="list the ids' folders on GitHub instead of winget"
    )
    parser.add_argument("--manifests", action="store_true", help="also read each installer manifest")
    parser.add_argument("--ids", nargs="+", metavar="ID", help="check only these ids of the lists")
    args = parser.parse_args(argv)
    apps = read_lists(APPS_RS.read_text(encoding="utf-8"))
    if args.ids:
        wanted = {i.lower() for i in args.ids}
        apps = [a for a in apps if a.package_id.lower() in wanted]
        unknown = wanted - {a.package_id.lower() for a in apps}
        if unknown:
            print(f"not in the lists: {', '.join(sorted(unknown))}", file=sys.stderr)
            return 2
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if args.github:
        results = check_github(apps, token)
    else:
        winget = shutil.which("winget")
        if winget is None:
            print("winget is not installed for this account; use --github.", file=sys.stderr)
            return 2
        results = check_local(apps, winget, args.manifests, token)
    counts = {s: sum(1 for r in results if r.status == s) for s in ("found", "missing", "error")}
    print(
        f"{len(results)} of {len(apps)} ids checked: {counts['found']} found, "
        f"{counts['missing']} missing, {counts['error']} could not be checked"
    )
    return exit_code(results, len(apps))


if __name__ == "__main__":
    sys.exit(main())
