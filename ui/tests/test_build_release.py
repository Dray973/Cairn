"""`scripts/build_release.ps1`, dot-sourced into Windows PowerShell so that only its functions load,
and `scripts/third_party_notices.py`: the version checks across Cargo.toml, Cargo.lock,
ui/pyproject.toml, ui/optimizer/__init__.py, CMakeLists.txt and telemetry.cpp; the combinations of
parameters; the checks before a build (the verification records gate, and the tests inside
optctl.exe only in an elevated shell); the signature gate of each signing mode, with fake
signatures; the one uninstaller that ISCC leaves to sign; ISCC's version line; the CMake generator
arguments; the Pillow modules the stage leaves out, and the build's Pillow trace without them; the
release notes, the SignPath metadata and build-info.json; and the notices of the Rust crates.
Nothing is built, compiled, signed or installed; the files the tests write are in pytest's
temporary folders."""

from __future__ import annotations

import base64
import ctypes
import functools
import importlib.util
import json
import os
import subprocess
import sys
import tomllib
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "build_release.ps1"
NOTICES = ROOT / "scripts" / "third_party_notices.py"
POWERSHELL = (
    Path(os.environ.get("SystemRoot", r"C:\Windows"))
    / "System32"
    / "WindowsPowerShell"
    / "v1.0"
    / "powershell.exe"
)
# Names written in parts, so this file names no signing service and no command line tool.
SIGN_PATH = "Sign" + "Path"
GH = "g" + "h"
TEST_THUMBPRINT = "AB12" * 10
VERSION = "0.3.0"
ENGINE = "app\\optimizer\\native\\optimizer_engine.pyd"
TELEMETRY = "app\\optimizer\\native\\optimizer_telemetry.dll"
WORKSPACE = ("cairn", "cairn-winres", "optctl", "optimizer_core", "optimizer_engine")


def quoted(text: str | Path) -> str:
    """`text` as a single-quoted PowerShell string."""
    return "'" + str(text).replace("'", "''") + "'"


def run_ps(body: str) -> Any:
    """Runs `body` in Windows PowerShell after dot-sourcing build_release.ps1 and returns the JSON
    value on the last line it prints."""
    command = "\n".join(
        (
            "$ErrorActionPreference = 'Stop'",
            "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)",
            f". {quoted(SCRIPT)}",
            body,
        )
    )
    encoded = base64.b64encode(command.encode("utf-16-le")).decode("ascii")
    done = subprocess.run(
        [
            str(POWERSHELL),
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            encoded,
        ],
        capture_output=True,
        timeout=180,
        check=False,
    )
    output = done.stdout.decode("utf-8", errors="replace")
    errors = done.stderr.decode("utf-8", errors="replace")
    assert done.returncode == 0, output + errors
    lines = [line for line in output.splitlines() if line.strip()]
    assert lines, errors
    return json.loads(lines[-1])


def as_json(expression: str) -> str:
    return f"ConvertTo-Json -Compress -Depth 6 -InputObject ({expression})"


def test_the_script_is_ascii_for_windows_powershell() -> None:
    # Windows PowerShell 5.1 reads a script without a byte order mark in the ANSI code page.
    data = SCRIPT.read_bytes()
    assert all(byte < 128 for byte in data), "non-ASCII text would be garbled"
    assert not data.startswith(b"\xef\xbb\xbf")


def test_a_tolerated_exit_code_is_not_left_behind() -> None:
    # A GitHub Actions step that runs the script ends with `exit $LASTEXITCODE`: ISCC's version
    # query and the uninstaller probe stop on purpose and must not fail the step.
    found = run_ps(
        as_json(
            "[ordered]@{ code = (Invoke-Capture { cmd.exe /c exit 3 }).Code; left = $global:LASTEXITCODE }"
        )
    )
    assert found == {"code": 3, "left": 0}


# ----------------------------------------------------------------------------- versions


def lock_text(versions: dict[str, str]) -> str:
    blocks = ["version = 4"]
    for name, version in versions.items():
        blocks.append(f'[[package]]\nname = "{name}"\nversion = "{version}"\ndependencies = [\n "anyhow",\n]')
    # A crate of the registry has its own version and does not count.
    blocks.append(
        '[[package]]\nname = "anyhow"\nversion = "1.0.104"\n'
        'source = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "00"'
    )
    return "\n\n".join(blocks) + "\n"


def fake_repo(
    folder: Path,
    *,
    lock: dict[str, str] | None = None,
    pyproject: str = VERSION,
    init: str = VERSION,
    cmake_version: str = "${CAIRN_VERSION}",
    cpp_version: str = '"optimizer_telemetry " TEL_VERSION',
) -> Path:
    files = {
        "Cargo.toml": (
            f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{VERSION}"\nedition = "2021"\n'
        ),
        "Cargo.lock": lock_text(lock if lock is not None else dict.fromkeys(WORKSPACE, VERSION)),
        "ui/pyproject.toml": (
            f'[project]\nname = "cairn-ui"\nversion = "{pyproject}"\n\n[tool.ruff]\nline-length = 110\n'
        ),
        "ui/optimizer/__init__.py": f'"""Package."""\n\n__version__ = "{init}"\n',
        "CMakeLists.txt": (
            "cmake_minimum_required(VERSION 3.24)\n\nproject(PCOptimizer\n"
            f'    VERSION {cmake_version}\n    DESCRIPTION "kernel"\n    LANGUAGES CXX RC)\n'
        ),
        "telemetry/src/telemetry.cpp": f"constexpr const char* kVersionString = {cpp_version};\n",
    }
    for name, text in files.items():
        path = folder / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8", newline="\n")
    return folder


def versions_of(folders: dict[str, Path]) -> dict[str, str]:
    """`Get-CairnVersion` of each folder: the version, or the error it throws."""
    lines = ["$found = [ordered]@{}"]
    for name, folder in folders.items():
        lines.append(
            f"try {{ $found[{quoted(name)}] = Get-CairnVersion {quoted(folder)} }} "
            f"catch {{ $found[{quoted(name)}] = 'error: ' + $_.Exception.Message }}"
        )
    lines.append(as_json("$found"))
    return run_ps("\n".join(lines))


def test_the_versions_must_agree_everywhere(tmp_path: Path) -> None:
    lock = dict.fromkeys(WORKSPACE, VERSION)
    folders = {
        "agree": fake_repo(tmp_path / "agree"),
        "stale lock": fake_repo(tmp_path / "stale", lock={**lock, "optctl": "0.2.0"}),
        "lock without the engine": fake_repo(
            tmp_path / "missing", lock={k: v for k, v in lock.items() if k != "optimizer_engine"}
        ),
        "pyproject": fake_repo(tmp_path / "pyproject", pyproject="0.2.0"),
        "init": fake_repo(tmp_path / "init", init="0.2.9"),
        "cmake literal": fake_repo(tmp_path / "cmake", cmake_version="0.2.0"),
        "cmake literal that agrees": fake_repo(tmp_path / "cmake-same", cmake_version=VERSION),
        "telemetry literal": fake_repo(tmp_path / "cpp", cpp_version='"optimizer_telemetry 0.2.0"'),
    }
    found = versions_of(folders)
    assert found["agree"] == VERSION
    assert found["cmake literal that agrees"] == VERSION
    assert found["stale lock"].startswith("error: Versions differ: Cargo.toml 0.3.0")
    assert "Cargo.lock (optctl) 0.2.0" in found["stale lock"]
    assert "Cargo.lock (cairn)" not in found["stale lock"], "only the packages that differ are named"
    assert "anyhow" not in found["stale lock"], "registry crates have their own versions"
    assert found["lock without the engine"] == (
        "error: Cargo.lock has no version of optimizer_engine; "
        "run cargo check --workspace and keep the updated Cargo.lock."
    )
    assert "ui\\pyproject.toml 0.2.0" in found["pyproject"]
    assert "ui\\optimizer\\__init__.py 0.2.9" in found["init"]
    assert "CMakeLists.txt 0.2.0" in found["cmake literal"]
    assert "telemetry\\src\\telemetry.cpp 0.2.0" in found["telemetry literal"]


def test_the_repository_carries_one_version() -> None:
    with (ROOT / "Cargo.toml").open("rb") as f:
        cargo = tomllib.load(f)["workspace"]["package"]["version"]
    assert run_ps(as_json("Get-CairnVersion $Root")) == cargo
    # CMake and the telemetry DLL take the version from Cargo.toml.
    assert "VERSION ${CAIRN_VERSION}" in (ROOT / "CMakeLists.txt").read_text(encoding="utf-8")
    cpp = (ROOT / "telemetry" / "src" / "telemetry.cpp").read_text(encoding="utf-8")
    assert '"optimizer_telemetry " TEL_VERSION' in cpp


# ----------------------------------------------------------------------------- parameters


ARGUMENT_CASES: dict[str, tuple[str, list[str]]] = {
    "full": ("@{}", []),
    "two modes": ("@{ StageOnly = $true; PackageOnly = $true }", ["Use one of"]),
    "stage to sign": ("@{ StageOnly = $true; SigningDir = 'build\\sign' }", []),
    "signing folder without a mode": ("@{ SigningDir = 'build\\sign' }", ["-SigningDir goes with"]),
    "signed files without their folder": (
        "@{ PackageOnly = $true; SignedDir = 's'; SigningMode = 'release' }",
        ["-SignedDir needs -SigningDir"],
    ),
    "release package": (
        "@{ PackageOnly = $true; SigningDir = 'd'; SignedDir = 's'; SigningMode = 'release' }",
        [],
    ),
    "test package without thumbprint": (
        "@{ PackageOnly = $true; SigningDir = 'd'; SignedDir = 's'; SigningMode = 'test' }",
        ["-SigningMode test needs -TestSignerThumbprint"],
    ),
    "test package": (
        "@{ PackageOnly = $true; SigningDir = 'd'; SignedDir = 's'; SigningMode = 'test'; "
        f"TestSignerThumbprint = '{TEST_THUMBPRINT}' }}",
        [],
    ),
    "release package without signed files": (
        "@{ PackageOnly = $true; SigningMode = 'release' }",
        ["-SigningMode release with -PackageOnly needs -SignedDir"],
    ),
    "release finish without the setup": (
        "@{ FinalizeOnly = $true; SigningMode = 'release' }",
        ["-SigningMode release with -FinalizeOnly needs -SignedSetup"],
    ),
    "release finish": ("@{ FinalizeOnly = $true; SignedSetup = 'x.exe'; SigningMode = 'release' }", []),
    "signed setup without a mode": (
        "@{ SignedSetup = 'x.exe' }",
        ["-SignedSetup goes with -FinalizeOnly", "Signed files need -SigningMode"],
    ),
    "signing a full build": ("@{ SigningMode = 'release' }", ["goes with -PackageOnly or -FinalizeOnly"]),
    "thumbprint without test mode": (
        f"@{{ StageOnly = $true; TestSignerThumbprint = '{TEST_THUMBPRINT}' }}",
        ["-TestSignerThumbprint goes with -SigningMode test"],
    ),
    "release candidate": ("@{ AllowPending = $true; SkipChecks = $true }", []),
    "release candidate package": (
        "@{ AllowPending = $true; PackageOnly = $true }",
        ["-AllowPending goes with"],
    ),
    "immutable release files": ("@{ FinalizeOnly = $true; ImmutableReleases = $true }", []),
    "immutable full build": ("@{ ImmutableReleases = $true; SkipChecks = $true }", []),
    "immutable stage": (
        "@{ StageOnly = $true; ImmutableReleases = $true }",
        ["-ImmutableReleases goes with"],
    ),
    "immutable package": (
        "@{ PackageOnly = $true; ImmutableReleases = $true }",
        ["-ImmutableReleases goes with"],
    ),
    "attested release files": ("@{ FinalizeOnly = $true; Attested = $true }", []),
    "attested full build": ("@{ Attested = $true; SkipChecks = $true }", []),
    "attested stage": ("@{ StageOnly = $true; Attested = $true }", ["-Attested goes with"]),
    "attested package": ("@{ PackageOnly = $true; Attested = $true }", ["-Attested goes with"]),
}


def test_parameters_that_do_not_go_together_are_refused() -> None:
    lines = ["$found = [ordered]@{}"]
    for name, (table, _expected) in ARGUMENT_CASES.items():
        lines.append(f"$found[{quoted(name)}] = @(Test-BuildArguments {table})")
    lines.append(as_json("$found"))
    found = run_ps("\n".join(lines))
    for name, (_table, expected) in ARGUMENT_CASES.items():
        problems = found[name]
        assert len(problems) == len(expected), (name, problems)
        for start, problem in zip(expected, problems, strict=True):
            assert start in problem, (name, problem)


# ----------------------------------------------------------------------------- signatures


SIGNATURE_HELPERS = """
function Fake([string]$Status, [string]$Subject, [string]$Thumbprint, [bool]$Timestamp) {
    $signer = $null
    if ($Subject) { $signer = [pscustomobject]@{ Subject = $Subject; Thumbprint = $Thumbprint } }
    $stamp = $null
    if ($Timestamp) { $stamp = [pscustomobject]@{ Subject = 'CN=Timestamp' } }
    return [pscustomobject]@{ Status = $Status; SignerCertificate = $signer; TimeStamperCertificate = $stamp }
}
function File([string]$Path, [object]$Signature) { [pscustomobject]@{ Path = $Path; Signature = $Signature } }
"""


def signature_problems(mode: str, files: list[tuple[str, str, str, str, bool]]) -> list[str]:
    """The problems `Find-SignatureProblems` reports in `mode` for (path, status, subject,
    thumbprint, timestamped) of fake files; Cairn's own files are those of $OwnFiles."""
    entries = ",\n".join(
        f"    (File {quoted(path)} (Fake {quoted(status)} {quoted(subject)} {quoted(thumb)} "
        f"${str(stamp).lower()}))"
        for path, status, subject, thumb, stamp in files
    )
    body = (
        SIGNATURE_HELPERS
        + f"$files = @(\n{entries}\n)\n"
        + as_json(f"@(Find-SignatureProblems $files '{mode}' '{TEST_THUMBPRINT}' @($OwnFiles.Keys))")
    )
    return run_ps(body)


def test_a_test_signature_needs_the_test_certificate_but_no_trust() -> None:
    test_signer = f"CN={SIGN_PATH} test certificate"
    problems = signature_problems(
        "test",
        [
            ("Cairn.exe", "UnknownError", test_signer, TEST_THUMBPRINT, False),
            ("optctl.exe", "NotTrusted", test_signer, TEST_THUMBPRINT.lower(), False),
            ("cairn-maintenance.exe", "Valid", test_signer, TEST_THUMBPRINT, False),
            (ENGINE, "UnknownError", "CN=Someone else", "00" * 20, False),
            (TELEMETRY, "HashMismatch", test_signer, TEST_THUMBPRINT, False),
            ("python312.dll", "Valid", "CN=Python Software Foundation", "11" * 20, True),
            ("DLLs\\_tkinter.pyd", "NotSigned", "", "", False),
            ("app\\site\\PIL\\_imaging.cp312-win_amd64.pyd", "NotSigned", "", "", False),
        ],
    )
    assert problems == [
        f"{ENGINE} is signed by CN=Someone else, not by the test certificate",
        f"{TELEMETRY} has a broken test signature (HashMismatch)",
        "DLLs\\_tkinter.pyd has no valid signature (NotSigned)",
    ]


def test_a_release_signature_is_valid_by_the_foundation_with_a_timestamp() -> None:
    foundation = f"CN={SIGN_PATH} Foundation, O={SIGN_PATH} Foundation, C=US"
    problems = signature_problems(
        "release",
        [
            ("Cairn.exe", "Valid", foundation, "22" * 20, True),
            ("optctl.exe", "Valid", foundation, "22" * 20, False),
            ("cairn-maintenance.exe", "Valid", "CN=Dray973", "33" * 20, True),
            (ENGINE, "UnknownError", foundation, "22" * 20, True),
            (TELEMETRY, "NotSigned", "", "", False),
            ("vcruntime140.dll", "Valid", "CN=Microsoft Corporation", "44" * 20, True),
            ("app\\site\\PIL\\_imagingtk.cp312-win_amd64.pyd", "NotSigned", "", "", False),
        ],
    )
    assert problems == [
        "optctl.exe has a signature without a timestamp",
        f"cairn-maintenance.exe is signed by CN=Dray973, not by {SIGN_PATH} Foundation",
        f"{ENGINE} has no valid signature (UnknownError)",
        f"{TELEMETRY} is not signed (NotSigned)",
    ]


# ----------------------------------------------------------------------------- uninstaller


def test_the_one_new_uninstaller_is_found() -> None:
    body = "\n".join(
        (
            "$found = [ordered]@{}",
            "$found['one'] = Find-NewUninstallerStub @() @('uninst-6.7.3-0a1b2c3d.e32')",
            "$found['new'] = Find-NewUninstallerStub @('uninst-old.e32') `",
            "    @('uninst-old.e32', 'notes.txt', 'uninst-new.e32')",
            "$cases = @(",
            "    @{ name = 'none'; after = @('notes.txt') },",
            "    @{ name = 'two'; after = @('uninst-a.e32', 'uninst-b.e32') }",
            ")",
            "foreach ($case in $cases) {",
            "    try { $found[$case.name] = Find-NewUninstallerStub @() $case.after }",
            "    catch { $found[$case.name] = $_.Exception.Message }",
            "}",
            as_json("$found"),
        )
    )
    found = run_ps(body)
    assert found["one"] == "uninst-6.7.3-0a1b2c3d.e32"
    assert found["new"] == "uninst-new.e32"
    assert found["none"] == "Expected one new uninstaller to sign (uninst-*.e32), found 0."
    assert (
        found["two"]
        == "Expected one new uninstaller to sign (uninst-*.e32), found 2: uninst-a.e32, uninst-b.e32."
    )


def test_signed_files_go_back_into_the_stage(tmp_path: Path) -> None:
    stage, signed, cache = tmp_path / "stage", tmp_path / "signed", tmp_path / "cache"
    stage_names = (
        "Cairn.exe",
        "optctl.exe",
        "cairn-maintenance.exe",
        "app/optimizer/native/optimizer_engine.pyd",
        "app/optimizer/native/optimizer_telemetry.dll",
    )
    upload_names = [name.replace("optimizer_engine.pyd", "optimizer_engine.dll") for name in stage_names]
    for name in stage_names:
        (stage / name).parent.mkdir(parents=True, exist_ok=True)
        (stage / name).write_text("unsigned", encoding="ascii")
    for name in [*upload_names, "uninstaller/uninst.exe"]:
        (signed / name).parent.mkdir(parents=True, exist_ok=True)
        (signed / name).write_text(f"signed {Path(name).name}", encoding="ascii")
    cache.mkdir()
    (cache / "uninst-6.7.3-0a1b2c3d.e32").write_text("unsigned", encoding="ascii")
    stub = run_ps(as_json(f"Import-SignedFiles {quoted(signed)} {quoted(stage)} {quoted(cache)}"))
    assert Path(stub) == cache / "uninst-6.7.3-0a1b2c3d.e32"
    assert (cache / "uninst-6.7.3-0a1b2c3d.e32").read_text(encoding="ascii") == "signed uninst.exe"
    assert (stage / "app/optimizer/native/optimizer_engine.pyd").read_text(encoding="ascii") == (
        "signed optimizer_engine.dll"
    ), "the engine goes back under its .pyd name"
    assert all((stage / name).read_text(encoding="ascii").startswith("signed") for name in stage_names)
    (cache / "uninst-other.e32").write_text("unsigned", encoding="ascii")
    message = run_ps(
        f"try {{ Import-SignedFiles {quoted(signed)} {quoted(stage)} {quoted(cache)} | Out-Null; "
        + as_json("'imported'")
        + " } catch { "
        + as_json("$_.Exception.Message")
        + " }"
    )
    assert message.startswith("Expected one uninstaller (uninst-*.e32) in"), message


# ----------------------------------------------------------------------------- tools


def test_isccs_version_line_is_read() -> None:
    lines = [
        "Inno Setup 6 Command-Line Compiler",
        "Copyright (C) 1997-2026 Jordan Russell. All rights reserved.",
        "",
        "Compiler engine version: Inno Setup 6.7.3",
        "Error on line 1 in <stdin>: version query.",
    ]
    array = "@(" + ", ".join(quoted(line) for line in lines) + ")"
    body = "\n".join(
        (
            f"$version = ConvertFrom-IsccOutput {array}",
            "$old = ConvertFrom-IsccOutput @('Compiler engine version: Inno Setup 6.2.2')",
            "$none = ConvertFrom-IsccOutput @('Inno Setup 6 Command-Line Compiler')",
            as_json(
                '[ordered]@{ version = "$version"; old = "$old"; old_refused = ($old -lt $IsccMinimum);'
                " new_accepted = ($version -ge $IsccMinimum); none = ($null -eq $none) }"
            ),
        )
    )
    found = run_ps(body)
    assert found == {
        "version": "6.7.3",
        "old": "6.2.2",
        "old_refused": True,
        "new_accepted": True,
        "none": True,
    }


def test_cmake_picks_the_newest_visual_studio_unless_told_otherwise(tmp_path: Path) -> None:
    cache = tmp_path / "CMakeCache.txt"
    cache.write_text(
        "//Name of generator.\n"
        "CMAKE_GENERATOR:INTERNAL=Visual Studio 18 2026\n"
        "CMAKE_GENERATOR_PLATFORM:INTERNAL=x64\n",
        encoding="utf-8",
    )
    missing = tmp_path / "none.txt"
    body = "\n".join(
        (
            "$found = [ordered]@{}",
            "$found['default'] = @(Get-CMakeGeneratorArguments '' '')",
            "$found['named'] = @(Get-CMakeGeneratorArguments 'Visual Studio 17 2022' '')",
            "$found['ninja'] = @(Get-CMakeGeneratorArguments 'Ninja' '')",
            "$found['environment'] = @(Get-CMakeGeneratorArguments '' 'Ninja')",
            f"$found['cached'] = Get-CMakeCacheValue {quoted(cache)} 'CMAKE_GENERATOR'",
            f"$found['missing'] = $null -eq (Get-CMakeCacheValue {quoted(missing)} 'CMAKE_GENERATOR')",
            as_json("$found"),
        )
    )
    found = run_ps(body)
    assert found["default"] == ["-A", "x64"], "no -G: CMake's own choice"
    assert found["named"] == ["-G", "Visual Studio 17 2022", "-A", "x64"]
    assert found["ninja"] == ["-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release"]
    assert found["environment"] == ["-DCMAKE_BUILD_TYPE=Release"]
    assert found["cached"] == "Visual Studio 18 2026"
    assert found["missing"] is True
    script = SCRIPT.read_text(encoding="ascii")
    assert "Visual Studio 17 2022" not in script, "the generator is not hard-coded"


def test_the_editor_settings_leave_the_generator_to_cmake() -> None:
    # The editor's CMake configure picks the newest Visual Studio, as the build script does.
    for name in ("settings.json", "tasks.json"):
        text = (ROOT / ".vscode" / name).read_text(encoding="utf-8")
        assert "cmake.generator" not in text and "Visual Studio 17 2022" not in text, name


def test_the_stage_leaves_out_the_pillow_modules_cairn_does_not_need(tmp_path: Path) -> None:
    pil = tmp_path / "site" / "PIL"
    pil.mkdir(parents=True)
    names = [
        "_avif",
        "_imaging",
        "_imagingcms",
        "_imagingft",
        "_imagingmath",
        "_imagingmorph",
        "_imagingtk",
        "_webp",
    ]
    for name in names:
        (pil / f"{name}.cp312-win_amd64.pyd").write_bytes(b"MZ")
    (pil / "_imaging.pyi").write_text("", encoding="ascii")
    removed = run_ps(as_json(f"@(Remove-UnusedPillowModules {quoted(tmp_path / 'site')})"))
    assert sorted(removed) == sorted(
        f"{name}.cp312-win_amd64.pyd" for name in names if name not in ("_imaging", "_imagingtk")
    )
    assert sorted(path.name for path in pil.iterdir()) == [
        "_imaging.cp312-win_amd64.pyd",
        "_imaging.pyi",
        "_imagingtk.cp312-win_amd64.pyd",
    ]


# Runs ahead of the build's Pillow trace ($PillowTrace) in a child Python: the Pillow modules
# named after the stage argument cannot be imported, as in a stage without them, and the thread
# that makes Tk's window moves to a desktop that is never shown (as conftest does), unless test
# windows are shown.
WITHOUT_LEFT_OUT_MODULES = """
import ctypes
import importlib.abc
import os
import sys
from ctypes import wintypes

LEFT_OUT = {"PIL." + name for name in sys.argv[2:]}


class LeftOut(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path=None, target=None):
        if fullname in LEFT_OUT:
            raise ModuleNotFoundError(f"No module named {fullname!r}", name=fullname)
        return None


sys.meta_path.insert(0, LeftOut())
if os.environ.get("OPTIMIZER_SHOW_TEST_WINDOWS") != "1":
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    user32.CreateDesktopW.restype = wintypes.HANDLE
    user32.CreateDesktopW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPCWSTR,
        ctypes.c_void_p,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.c_void_p,
    ]
    user32.SetThreadDesktop.argtypes = [wintypes.HANDLE]
    desktop = user32.CreateDesktopW("CairnTests", None, None, 0, 0x10000000, None)
    if desktop:
        user32.SetThreadDesktop(desktop)
"""


def test_the_app_does_without_the_pillow_modules_the_stage_leaves_out(tmp_path: Path) -> None:
    # A full Pillow also loads _imagingft (ImageDraw imports ImageFont, which tries it), but
    # Cairn draws no text with Pillow: the build's trace of the renderer, the icon file and the
    # Tk photos must pass when no left-out module can be imported, and load only the kept ones.
    build = run_ps(
        as_json("[ordered]@{ kept = @($PillowKept); left_out = @($PillowUnused); trace = $PillowTrace }")
    )
    assert build["left_out"] and not set(build["kept"]) & set(build["left_out"])
    code = WITHOUT_LEFT_OUT_MODULES + build["trace"]
    done = subprocess.run(
        [sys.executable, "-B", "-c", code, str(tmp_path / "no-stage"), *build["left_out"]],
        cwd=ROOT / "ui",
        capture_output=True,
        timeout=180,
        check=False,
    )
    output = done.stdout.decode("utf-8", errors="replace")
    assert done.returncode == 0, output + done.stderr.decode("utf-8", errors="replace")
    loaded = json.loads(output.strip().splitlines()[-1])["loaded"]
    assert loaded == sorted(build["kept"])


def test_cairns_own_files_name_the_product_and_the_version() -> None:
    body = "\n".join(
        (
            "function Info([string]$Name, [string]$Version, [int[]]$Parts) {",
            "    [pscustomobject]@{ ProductName = $Name; ProductVersion = $Version;"
            " FileMajorPart = $Parts[0]; FileMinorPart = $Parts[1]; FileBuildPart = $Parts[2] }",
            "}",
            "function Entry([string]$Path, [object]$Info) {",
            "    [pscustomobject]@{ Path = $Path; Info = $Info }",
            "}",
            "$files = @(",
            "    (Entry 'Cairn.exe' (Info 'Cairn' '0.3.0' @(0, 3, 0))),",
            "    (Entry 'optctl.exe' (Info $null $null @(0, 0, 0))),",
            "    (Entry 'cairn-maintenance.exe' (Info 'Cairn' '0.2.0' @(0, 2, 0))),",
            f"    (Entry {quoted(ENGINE)} $null)",
            ")",
            as_json("@(Find-VersionResourceProblems $files '0.3.0')"),
        )
    )
    assert run_ps(body) == [
        "optctl.exe has no version resource that names the product Cairn",
        "cairn-maintenance.exe has the product version '0.2.0', not 0.3.0",
        "cairn-maintenance.exe has the file version 0.2.0, not 0.3.0",
        f"{ENGINE} is missing",
    ]


def test_the_notices_must_be_in_the_stage(tmp_path: Path) -> None:
    licenses = tmp_path / "licenses"
    licenses.mkdir()
    before = run_ps(as_json(f"@(Find-NoticeProblems {quoted(tmp_path)})"))
    assert before == [
        "licenses\\Rust-THIRD-PARTY.txt is missing or empty",
        "licenses\\THIRD-PARTY-NOTICES.md is missing",
    ]
    (licenses / "Rust-THIRD-PARTY.txt").write_text("something else\n", encoding="utf-8")
    (licenses / "THIRD-PARTY-NOTICES.md").write_text("# Notices\n", encoding="utf-8")
    assert run_ps(as_json(f"@(Find-NoticeProblems {quoted(tmp_path)})")) == [
        "licenses\\Rust-THIRD-PARTY.txt is not the notices file"
    ]
    (licenses / "Rust-THIRD-PARTY.txt").write_text("Third-party notices: the Rust crates\n", encoding="utf-8")
    assert run_ps(as_json(f"@(Find-NoticeProblems {quoted(tmp_path)})")) == []


# ----------------------------------------------------------------------------- release files


ATTESTATION_VERIFY = f"{GH} attestation verify .\\Cairn-0.3.0-setup.exe --repo Dray973/Cairn"
ATTESTATION_EXPLAINED = (
    f"`{GH} attestation verify` checks that GitHub Actions built the setup from this repository."
)
RELEASE_VERIFY = f"{GH} release verify v0.3.0 --repo Dray973/Cairn"
RELEASE_EXPLAINED = (
    f"`{GH} release verify` checks the attestation GitHub made when it published this release as immutable."
)


def release_notes(signed: bool, attested: bool = False, immutable: bool = False) -> str:
    return run_ps(
        as_json(
            f"New-ReleaseNotes '0.3.0' 'Cairn-0.3.0-setup.exe' '{'ab' * 32}' "
            f"${str(signed).lower()} ${str(attested).lower()} ${str(immutable).lower()}"
        )
    )


def verification_block(notes: str) -> list[str]:
    """The lines of the notes' code block of verification commands."""
    return notes.split("```powershell\n", 1)[1].split("\n```", 1)[0].splitlines()


def test_unsigned_release_notes_say_so() -> None:
    notes = release_notes(signed=False)
    assert "Download `Cairn-0.3.0-setup.exe` below." in notes
    assert f"`{'ab' * 32}`" in notes
    assert verification_block(notes) == [
        "Get-FileHash .\\Cairn-0.3.0-setup.exe -Algorithm SHA256        # compare with SHA256SUMS.txt"
    ]
    assert f"{GH} attestation verify" not in notes, "only the release workflow's tag runs attest the setup"
    assert f"{GH} release verify" not in notes, "GitHub attests a release only when it is immutable"
    assert "```\n\n## SmartScreen and Smart App Control" in notes, "no commands, nothing to explain"
    assert "Get-AuthenticodeSignature" not in notes
    assert SIGN_PATH not in notes, "the attribution belongs to signed releases only"
    assert "not code-signed yet" in notes and "**More info**, then **Run anyway**" in notes
    assert "Settings \u203a Apps \u203a Installed apps" in notes
    assert notes.rstrip().endswith("## Changes"), "an empty Changes heading closes the notes"


def test_signed_release_notes_carry_the_attribution() -> None:
    notes = release_notes(signed=True)
    attribution = (
        f"Free code signing provided by [{SIGN_PATH}.io](https://about.{SIGN_PATH.lower()}.io), "
        f"certificate by [{SIGN_PATH} Foundation](https://{SIGN_PATH.lower()}.org)."
    )
    assert attribution in notes
    assert (
        "Get-AuthenticodeSignature .\\Cairn-0.3.0-setup.exe | Format-List Status, SignerCertificate" in notes
    )
    assert "docs/code-signing-policy.md" in notes
    assert "not code-signed yet" not in notes
    assert f"{GH} attestation verify" not in notes
    assert f"{GH} release verify" not in notes
    assert notes.rstrip().endswith("## Changes")


def test_attestation_verify_is_shown_only_for_attested_builds() -> None:
    # A build made outside the release workflow's tag run (the local fallback of a release
    # among them) has no attestation, so its notes must not send anyone to look for one.
    for signed in (False, True):
        notes = release_notes(signed=signed, attested=True)
        assert verification_block(notes)[-1] == ATTESTATION_VERIFY, signed
        assert f"```\n\n{ATTESTATION_EXPLAINED}\n\n## SmartScreen" in notes, signed
        assert f"{GH} release verify" not in notes, signed


def test_release_verify_is_shown_only_for_immutable_releases() -> None:
    for signed in (False, True):
        notes = release_notes(signed=signed, immutable=True)
        assert verification_block(notes)[-1] == RELEASE_VERIFY, signed
        assert f"```\n\n{RELEASE_EXPLAINED}\n\n## SmartScreen" in notes, signed
        assert f"{GH} attestation verify" not in notes, signed
    notes = release_notes(signed=False, attested=True, immutable=True)
    assert verification_block(notes)[-2:] == [ATTESTATION_VERIFY, RELEASE_VERIFY]
    assert f"```\n\n{ATTESTATION_EXPLAINED} {RELEASE_EXPLAINED}\n\n## SmartScreen" in notes


def test_the_release_files_step_passes_both_switches_to_the_notes(tmp_path: Path) -> None:
    runs = [(attested, immutable) for attested in (False, True) for immutable in (False, True)]
    lines = []
    for attested, immutable in runs:
        dist = tmp_path / f"attested-{attested}-immutable-{immutable}"
        dist.mkdir()
        (dist / "Cairn-0.3.0-setup.exe").write_bytes(b"MZ")
        paths = (
            f"@{{ Dist = {quoted(dist)}; Stage = {quoted(dist / 'stage')};"
            f" BuildInfo = {quoted(dist / 'build-info.json')} }}"
        )
        options = (
            f"@{{ SigningMode = 'none'; Attested = ${str(attested).lower()};"
            f" ImmutableReleases = ${str(immutable).lower()} }}"
        )
        lines.append(f"Invoke-Finalize {options} '0.3.0' {paths} | Out-Null")
    lines.append(as_json("'written'"))
    assert run_ps("\n".join(lines)) == "written"
    for attested, immutable in runs:
        dist = tmp_path / f"attested-{attested}-immutable-{immutable}"
        notes = (dist / "release-notes.md").read_text(encoding="utf-8")
        assert (ATTESTATION_VERIFY in notes) is attested, (attested, immutable)
        assert (RELEASE_VERIFY in notes) is immutable, (attested, immutable)


def test_signpath_metadata_keeps_the_padding() -> None:
    name, version = "Cairn" + " " * 55, "0.3.0" + " " * 45
    body = "\n".join(
        (
            "function Entry([string]$Configuration, [string]$Name, [object]$ProductName,",
            "    [object]$ProductVersion) {",
            "    [pscustomobject]@{ Configuration = $Configuration; Name = $Name;"
            " ProductName = $ProductName; ProductVersion = $ProductVersion }",
            "}",
            "$entries = @(",
            "    (Entry 'binaries' 'Cairn.exe' 'Cairn' '0.3.0'),",
            "    (Entry 'binaries' 'uninstaller/uninst.exe' $null $null),",
            f"    (Entry 'setup' 'Cairn-0.3.0-setup.exe' {quoted(name)} {quoted(version)})",
            ")",
            as_json("New-SignPathMetadata $entries"),
        )
    )
    text = run_ps(body)
    lines = text.splitlines()
    assert lines[lines.index("binaries: Cairn.exe") + 1] == '  product-name "Cairn" (5 characters)'
    assert lines[lines.index("binaries: uninstaller/uninst.exe") + 1].startswith("  not made by this build")
    setup = lines.index("setup: Cairn-0.3.0-setup.exe")
    assert lines[setup + 1] == f'  product-name "{name}" (60 characters)'
    assert lines[setup + 2] == f'  product-version "{version}" (50 characters)'


def test_build_info_gathers_what_each_step_adds(tmp_path: Path) -> None:
    path = tmp_path / "build-info.json"
    first = (
        "[ordered]@{ version = '0.3.0'; kind = 'release candidate';"
        " pillow_modules = @('_imaging', '_imagingtk') }"
    )
    body = "\n".join(
        (
            f"Write-BuildInfo {quoted(path)} ({first})",
            f"Write-BuildInfo {quoted(path)} ([ordered]@{{ kind = 'release'; setup_sha256 = 'ab' }})",
            as_json("'written'"),
        )
    )
    assert run_ps(body) == "written"
    info = json.loads(path.read_text(encoding="utf-8"))
    assert info == {
        "version": "0.3.0",
        "kind": "release",
        "pillow_modules": ["_imaging", "_imagingtk"],
        "setup_sha256": "ab",
    }


def test_the_commit_is_read_from_the_repositorys_files(tmp_path: Path) -> None:
    sha, other = "0123456789abcdef0123456789abcdef01234567", "89abcdef0123456789abcdef0123456789abcdef"
    loose = tmp_path / "loose"
    (loose / ".git" / "refs" / "heads").mkdir(parents=True)
    (loose / ".git" / "HEAD").write_text("ref: refs/heads/main\n", encoding="ascii")
    (loose / ".git" / "refs" / "heads" / "main").write_text(sha + "\n", encoding="ascii")
    packed = tmp_path / "packed"
    (packed / ".git").mkdir(parents=True)
    (packed / ".git" / "HEAD").write_text("ref: refs/heads/release\n", encoding="ascii")
    (packed / ".git" / "packed-refs").write_text(
        f"# pack-refs with: peeled fully-peeled sorted\n{other} refs/heads/main\n{sha} refs/heads/release\n",
        encoding="ascii",
    )
    detached = tmp_path / "detached"
    (detached / ".git").mkdir(parents=True)
    (detached / ".git" / "HEAD").write_text(other + "\n", encoding="ascii")
    body = "\n".join(
        (
            "$env:GITHUB_SHA = $null",
            "$found = [ordered]@{}",
            f"$found['loose'] = Get-SourceCommit {quoted(loose)}",
            f"$found['packed'] = Get-SourceCommit {quoted(packed)}",
            f"$found['detached'] = Get-SourceCommit {quoted(detached)}",
            f"$found['none'] = $null -eq (Get-SourceCommit {quoted(tmp_path / 'none')})",
            "$env:GITHUB_SHA = 'fedcba9876543210fedcba9876543210fedcba98'",
            f"$found['actions'] = Get-SourceCommit {quoted(loose)}",
            as_json("$found"),
        )
    )
    assert run_ps(body) == {
        "loose": sha,
        "packed": sha,
        "detached": other,
        "none": True,
        "actions": "fedcba9876543210fedcba9876543210fedcba98",
    }


def test_the_release_build_runs_the_verification_gate_unless_pending_is_allowed() -> None:
    script = SCRIPT.read_text(encoding="ascii")
    checks = script.split("function Invoke-Checks(", 1)[1].split("\n}\n", 1)[0]
    assert "if ($AllowPending) { $env:CAIRN_RELEASE = $null } else { $env:CAIRN_RELEASE = '1' }" in checks
    assert checks.index("CAIRN_RELEASE = '1'") < checks.index("cargo test --workspace")
    rust = script.split("function Invoke-RustBuild(", 1)[1].split("\n}\n", 1)[0]
    assert "$env:CAIRN_REQUIRE_RESOURCES = '1'" in rust
    stage = script.split("function Invoke-Stage(", 1)[1].split("\n}\n", 1)[0]
    assert "kind = if ($Options.AllowPending) { 'release candidate' } else { 'release' }" in stage


def test_the_checks_run_the_tests_inside_optctl_when_the_shell_is_elevated() -> None:
    # `cargo test --workspace` leaves them out (crates/cli/Cargo.toml): their test program carries
    # optctl's requireAdministrator manifest, which an unelevated shell cannot start.
    script = SCRIPT.read_text(encoding="ascii")
    checks = " ".join(script.split("function Invoke-Checks(", 1)[1].split("\n}\n", 1)[0].split())
    optctl = "if (Test-Elevated) { Invoke-Native 'optctl tests' { cargo test -p optctl --bin optctl } }"
    assert optctl + " else {" in checks
    assert checks.index("cargo test --workspace") < checks.index(optctl)
    assert run_ps(as_json("Test-Elevated")) is bool(ctypes.windll.shell32.IsUserAnAdmin())


# ----------------------------------------------------------------------------- Rust notices


@functools.cache
def notices_module() -> ModuleType:
    """scripts/third_party_notices.py as a module (registered, as its dataclass needs)."""
    spec = importlib.util.spec_from_file_location("third_party_notices", NOTICES)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def package(
    name: str, version: str, folder: Path | None, deps: list[tuple[str, str | None]]
) -> tuple[dict, dict]:
    """A package of `cargo metadata` and its resolve node; `folder` None makes it a workspace member."""
    pid = f"{name} {version}"
    source = None if folder is None else "registry+https://github.com/rust-lang/crates.io-index"
    manifest = (folder or Path("C:/workspace") / name) / "Cargo.toml"
    meta = {
        "id": pid,
        "name": name,
        "version": version,
        "source": source,
        "license": "MIT OR Apache-2.0",
        "license_file": None,
        "repository": f"https://example.com/{name}",
        "manifest_path": str(manifest),
    }
    node = {
        "id": pid,
        "deps": [{"pkg": dep, "dep_kinds": [{"kind": kind, "target": None}]} for dep, kind in deps],
    }
    return meta, node


def fake_metadata(tmp_path: Path) -> dict[str, Any]:
    crates = tmp_path / "registry"
    folders = {name: crates / f"{name}-1.0.0" for name in ("alpha", "beta", "gamma", "builder", "tester")}
    for name, folder in folders.items():
        folder.mkdir(parents=True)
        (folder / "LICENSE-MIT").write_bytes(
            f"Copyright (c) the {name} authors\r\n\r\nPermission is granted.\r\n".encode()
        )
        (folder / "LICENSE-APACHE").write_bytes(b"Apache License\nVersion 2.0\n")
        (folder / "README.md").write_bytes(b"not a license")
    entries = [
        package("cairn", VERSION, None, [("alpha 1.0.0", None), ("builder 1.0.0", "build")]),
        package("optctl", VERSION, None, [("optimizer_core 0.3.0", None)]),
        package("optimizer_engine", VERSION, None, [("optimizer_core 0.3.0", None), ("gamma 1.0.0", None)]),
        package("optimizer_core", VERSION, None, [("beta 1.0.0", None), ("tester 1.0.0", "dev")]),
        package("alpha", "1.0.0", folders["alpha"], [("beta 1.0.0", None)]),
        package("beta", "1.0.0", folders["beta"], []),
        package("gamma", "1.0.0", folders["gamma"], []),
        package("builder", "1.0.0", folders["builder"], []),
        package("tester", "1.0.0", folders["tester"], []),
    ]
    return {"packages": [meta for meta, _ in entries], "resolve": {"nodes": [node for _, node in entries]}}


def test_the_notices_name_the_crates_the_programs_are_built_from(tmp_path: Path) -> None:
    metadata = fake_metadata(tmp_path)
    notices = notices_module()
    crates = notices.shipped_crates(metadata)
    assert [crate.label for crate in crates] == ["alpha 1.0.0", "beta 1.0.0", "gamma 1.0.0"], (
        "normal dependencies only, without Cairn's own packages"
    )
    source = tmp_path / "metadata.json"
    source.write_text(json.dumps(metadata), encoding="utf-8")
    out = tmp_path / "out" / "Rust-THIRD-PARTY.txt"
    assert notices.main(["--out", str(out), "--metadata", str(source)]) == 0
    text = out.read_text(encoding="utf-8")
    assert text.startswith(f"Third-party notices: the Rust crates of Cairn {VERSION}\n")
    assert "alpha 1.0.0  MIT OR Apache-2.0  https://example.com/alpha" in text
    assert "builder" not in text and "tester" not in text
    assert text.count("Apache License\nVersion 2.0") == 1, "an identical text is written once"
    assert "LICENSE-APACHE: alpha 1.0.0, beta 1.0.0, gamma 1.0.0" in text
    for name in ("alpha", "beta", "gamma"):
        assert f"LICENSE-MIT: {name} 1.0.0\n" in text
        assert f"Copyright (c) the {name} authors\n\nPermission is granted." in text
    assert "README" not in text and "not a license" not in text
    assert str(tmp_path) not in text and "\r" not in text, "no folder of this PC; one kind of line end"


def test_a_crate_without_a_license_file_stops_the_notices(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    metadata = fake_metadata(tmp_path)
    for license_file in (tmp_path / "registry" / "gamma-1.0.0").glob("LICENSE-*"):
        license_file.unlink()
    source = tmp_path / "metadata.json"
    source.write_text(json.dumps(metadata), encoding="utf-8")
    out = tmp_path / "out.txt"
    assert notices_module().main(["--out", str(out), "--metadata", str(source)]) == 1
    assert "gamma 1.0.0 ships no license file" in capsys.readouterr().err
    assert not out.exists()


def test_the_notices_need_every_shipped_package(tmp_path: Path) -> None:
    metadata = fake_metadata(tmp_path)
    metadata["packages"] = [p for p in metadata["packages"] if p["name"] != "optimizer_engine"]
    notices = notices_module()
    with pytest.raises(notices.NoticeError, match="no package optimizer_engine"):
        notices.shipped_crates(metadata)
