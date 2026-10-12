"""The GitHub workflows, their scripts, the SignPath artifact configurations and Dependabot, read as
text: every action pinned to a commit, no stored credentials, no pull_request_target, no caches in
the release, the test guards and read-only permissions, the jobs of CI and of a release, the
pinned Inno Setup, the install smoke test, the signing steps that stay off until SignPath is
configured, and the public data check (its generic rules on pull requests, and this PC's IPv4
addresses as whole addresses). Also the documents a signing application points to, checked against
the code: the code signing policy, the privacy statement and the security policy. Nothing is run on
GitHub or installed.
"""

from __future__ import annotations

import importlib.util
import re
import sys
import xml.etree.ElementTree as ET
from collections.abc import Iterator
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[2]
GITHUB = ROOT / ".github"
WORKFLOWS = GITHUB / "workflows"
SIGNING = ROOT / ".signpath" / "artifact-configurations"
BUILD_SCRIPT = ROOT / "scripts" / "build_release.ps1"
RUNNER = "windows-2025-vs2026"
GUARDS = (
    "OPTIMIZER_FORBID_RESTORE_POINT",
    "OPTIMIZER_FORBID_DRIVE_TESTS",
    "OPTIMIZER_FORBID_UPDATE_SEARCH",
    "OPTIMIZER_FORBID_APP_INSTALLS",
)
# Names written in parts, so this file names no signing service and no resolution cmdlet.
SIGN_PATH = "Sign" + "Path"
RESOLUTION_CMDLET = "Set-" + "DisplayResolution"
PIN = re.compile(r"^\s*(?:-\s+)?uses:\s+([\w.-]+/[\w./-]+)@([0-9a-f]{40})\s+#\s+v\d+(?:\.\d+)*\s*$")
GUID = re.compile(r"[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}")
SHA256 = re.compile(r"^[0-9a-f]{64}$")


# ----------------------------------------------------------------------------- YAML subset


class YamlSubset:
    """The YAML the workflows use: block mappings and sequences, plain, single- and double-quoted
    scalars, flow sequences, `|` and `>` block scalars and comments. Anything else is refused."""

    def __init__(self, text: str) -> None:
        self.lines = text.splitlines()
        self.i = 0

    def parse(self) -> Any:
        self._skip()
        if self.i >= len(self.lines):
            return None
        value = self._node(self._indent(self.lines[self.i]))
        self._skip()
        if self.i < len(self.lines):
            raise ValueError(f"line {self.i + 1}: unexpected indentation")
        return value

    @staticmethod
    def _indent(line: str) -> int:
        if "\t" in line[: len(line) - len(line.lstrip())]:
            raise ValueError("tabs in indentation")
        return len(line) - len(line.lstrip(" "))

    def _skip(self) -> None:
        while self.i < len(self.lines):
            stripped = self.lines[self.i].strip()
            if stripped and not stripped.startswith("#"):
                return
            self.i += 1

    def _node(self, indent: int) -> Any:
        content = self.lines[self.i][indent:]
        if content == "-" or content.startswith("- "):
            return self._sequence(indent)
        return self._mapping(indent)

    def _sequence(self, indent: int) -> list[Any]:
        items: list[Any] = []
        while True:
            self._skip()
            if self.i >= len(self.lines) or self._indent(self.lines[self.i]) != indent:
                break
            content = self.lines[self.i][indent:]
            if not (content == "-" or content.startswith("- ")):
                break
            item = content[2:]
            self.i += 1
            if not item.strip():
                self._skip()
                items.append(self._node(self._indent(self.lines[self.i])))
            elif self._key_split(item) is not None:
                items.append(self._mapping(indent + 2, first=item))
            else:
                items.append(scalar(strip_comment(item).strip()))
        return items

    @staticmethod
    def _key_split(content: str) -> tuple[str, str] | None:
        if content[:1] in ("'", '"', "[", "{"):
            return None
        match = re.match(r"^([^\s:#][^:#]*?):(?:\s+(.*))?$", content)
        if not match:
            return None
        return match.group(1).strip(), match.group(2) or ""

    def _mapping(self, indent: int, first: str | None = None) -> dict[str, Any]:
        result: dict[str, Any] = {}
        pending = first
        while True:
            if pending is not None:
                content, pending = pending, None
            else:
                self._skip()
                if self.i >= len(self.lines):
                    break
                line = self.lines[self.i]
                current = self._indent(line)
                if current < indent:
                    break
                if current > indent:
                    raise ValueError(f"line {self.i + 1}: unexpected indentation")
                content = line[indent:]
                if content == "-" or content.startswith("- "):
                    break
                self.i += 1
            split = self._key_split(content)
            if split is None:
                raise ValueError(f"line {self.i}: expected 'key: value', found {content!r}")
            key, rest = split
            if key in result:
                raise ValueError(f"line {self.i}: duplicate key {key}")
            rest = strip_comment(rest).strip()
            if rest in ("|", "|-", ">", ">-"):
                result[key] = self._block_scalar(indent, rest)
                continue
            if rest:
                result[key] = scalar(rest)
                continue
            self._skip()
            if self.i < len(self.lines) and self._indent(self.lines[self.i]) > indent:
                result[key] = self._node(self._indent(self.lines[self.i]))
            elif (
                self.i < len(self.lines)
                and self._indent(self.lines[self.i]) == indent
                and (self.lines[self.i][indent:].startswith("- "))
            ):
                result[key] = self._sequence(indent)
            else:
                result[key] = None
        return result

    def _block_scalar(self, indent: int, style: str) -> str:
        collected: list[str] = []
        block_indent: int | None = None
        while self.i < len(self.lines):
            line = self.lines[self.i]
            if line.strip():
                current = self._indent(line)
                if current <= indent:
                    break
                if block_indent is None:
                    block_indent = current
                if current < block_indent:
                    raise ValueError(f"line {self.i + 1}: block scalar indentation")
                collected.append(line[block_indent:])
            else:
                collected.append("")
            self.i += 1
        while collected and not collected[-1]:
            collected.pop()
        if style.startswith(">"):
            text = " ".join(collected)
        else:
            text = "\n".join(collected)
        return text if style.endswith("-") else text + "\n"


def strip_comment(text: str) -> str:
    """`text` without a trailing comment (a `#` after white space, outside quotes)."""
    quote = ""
    for index, char in enumerate(text):
        if quote:
            if char == quote:
                quote = ""
        elif char in ("'", '"') and (index == 0 or text[index - 1] in " [,"):
            quote = char
        elif char == "#" and (index == 0 or text[index - 1] in " \t"):
            return text[:index]
    return text


def scalar(text: str) -> Any:
    if text.startswith("'"):
        if not text.endswith("'") or len(text) < 2:
            raise ValueError(f"unterminated quote: {text}")
        return text[1:-1].replace("''", "'")
    if text.startswith('"'):
        if not text.endswith('"') or len(text) < 2:
            raise ValueError(f"unterminated quote: {text}")
        return text[1:-1].replace('\\"', '"').replace("\\\\", "\\")
    if text.startswith("["):
        if not text.endswith("]"):
            raise ValueError(f"unterminated flow sequence: {text}")
        inner = text[1:-1].strip()
        return (
            [scalar(part.strip()) for part in re.split(r",(?=(?:[^']*'[^']*')*[^']*$)", inner)]
            if inner
            else []
        )
    if text.startswith(("{", "&", "*", "!", "%", "@", "`")):
        raise ValueError(f"unsupported YAML: {text}")
    if text in ("true", "false"):
        return text == "true"
    return text


def parse_yaml(text: str) -> Any:
    return YamlSubset(text).parse()


def test_the_parser_reads_the_subset_the_workflows_use() -> None:
    text = """
# comment
name: CI
on:
  push:
    branches: [main, 'release/*']
  workflow_dispatch:
env:
  A: '1'   # trailing comment
  B: ${{ inputs.release && '1' || '0' }}
jobs:
  one:
    steps:
      - uses: owner/repo@0123456789012345678901234567890123456789 # v1.2.3
        with:
          persist-credentials: false
      - name: Run
        run: |
          echo a  # kept
          if ($x) { 'y' }
      - run: >-
          first part
          second part
"""
    assert parse_yaml(text) == {
        "name": "CI",
        "on": {"push": {"branches": ["main", "release/*"]}, "workflow_dispatch": None},
        "env": {"A": "1", "B": "${{ inputs.release && '1' || '0' }}"},
        "jobs": {
            "one": {
                "steps": [
                    {
                        "uses": "owner/repo@0123456789012345678901234567890123456789",
                        "with": {"persist-credentials": False},
                    },
                    {"name": "Run", "run": "echo a  # kept\nif ($x) { 'y' }\n"},
                    {"run": "first part second part"},
                ]
            }
        },
    }
    with pytest.raises(ValueError):
        parse_yaml("a:\n  b: 1\n c: 2\n")


# ----------------------------------------------------------------------------- helpers


def workflow_files() -> list[Path]:
    return sorted(WORKFLOWS.glob("*.yml"))


def load(name: str) -> dict[str, Any]:
    return parse_yaml((WORKFLOWS / name).read_text(encoding="utf-8"))


def steps(workflow: dict[str, Any]) -> Iterator[tuple[str, dict[str, Any]]]:
    for job_id, job in workflow["jobs"].items():
        for step in job.get("steps") or []:
            yield job_id, step


def step_named(job: dict[str, Any], name: str) -> dict[str, Any]:
    found = [step for step in job["steps"] if step.get("name") == name]
    assert len(found) == 1, name
    return found[0]


def index_of(job: dict[str, Any], text: str) -> int:
    """The position of the one step whose run script contains `text`."""
    found = [i for i, step in enumerate(job["steps"]) if text in str(step.get("run", ""))]
    assert len(found) == 1, text
    return found[0]


# ----------------------------------------------------------------------------- every workflow


def test_the_workflows_exist_and_parse() -> None:
    names = {path.name for path in workflow_files()}
    assert {"ci.yml", "release.yml", "system-tests.yml"} <= names
    for path in workflow_files():
        workflow = load(path.name)
        assert workflow["name"] and workflow["on"] and workflow["jobs"], path.name
    assert load("ci.yml")["name"] == "CI", "the package job's condition names this workflow"


def test_every_action_is_pinned_to_a_full_commit() -> None:
    for path in [*workflow_files(), GITHUB / "dependabot.yml"]:
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if "uses:" not in line or line.strip().startswith("#"):
                continue
            target = line.split("uses:", 1)[1].strip()
            if target.startswith("./"):
                continue
            assert PIN.match(line), f"{path.name}:{number}: {line.strip()}"


def test_the_same_action_has_the_same_pin_everywhere() -> None:
    pins: dict[str, set[str]] = {}
    for path in workflow_files():
        for line in path.read_text(encoding="utf-8").splitlines():
            match = PIN.match(line)
            if match:
                pins.setdefault(match.group(1).lower(), set()).add(line.split("@", 1)[1].strip())
    assert pins, "the workflows use actions"
    assert {action: refs for action, refs in pins.items() if len(refs) > 1} == {}


def test_checkouts_keep_no_credentials() -> None:
    for path in workflow_files():
        checkouts = [
            step for _, step in steps(load(path.name)) if "actions/checkout@" in str(step.get("uses", ""))
        ]
        assert checkouts, path.name
        for step in checkouts:
            assert step["with"]["persist-credentials"] is False, path.name


def test_no_workflow_runs_for_pull_request_target() -> None:
    for path in workflow_files():
        assert "pull_request_target" not in path.read_text(encoding="utf-8"), path.name
        assert set(load(path.name)["on"]) <= {
            "push",
            "pull_request",
            "workflow_dispatch",
            "workflow_call",
            "schedule",
        }


def test_every_workflow_sets_the_guards() -> None:
    for path in workflow_files():
        env = load(path.name)["env"]
        assert {name: env.get(name) for name in GUARDS} == dict.fromkeys(GUARDS, "1"), path.name


def test_permissions_are_read_only_unless_a_job_needs_more() -> None:
    for path in workflow_files():
        workflow = load(path.name)
        assert workflow["permissions"] == {"contents": "read"}, path.name
        for job_id, job in workflow["jobs"].items():
            if "permissions" in job:
                assert (path.name, job_id) == ("release.yml", "release")
    assert load("release.yml")["jobs"]["release"]["permissions"] == {
        "contents": "write",
        "actions": "read",
        "id-token": "write",
        "attestations": "write",
    }


def test_every_job_names_its_runner_and_a_time_limit() -> None:
    for path in workflow_files():
        for job_id, job in load(path.name)["jobs"].items():
            if "uses" in job:
                assert job["uses"] == "./.github/workflows/ci.yml", (path.name, job_id)
                continue
            assert job["runs-on"] == RUNNER, (path.name, job_id)
            assert int(job["timeout-minutes"]) > 0, (path.name, job_id)


def test_information_steps_only_print() -> None:
    for path in workflow_files():
        for job_id, step in steps(load(path.name)):
            if step.get("name") != "Build information":
                continue
            assert (
                "foreach ($command in $commands) { try { & $command } catch { Write-Warning $_ } }"
                in step["run"]
            ), (
                path.name,
                job_id,
            )


# ----------------------------------------------------------------------------- CI


def test_ci_runs_every_check_and_builds_the_setup_only_on_main() -> None:
    ci = load("ci.yml")
    assert ci["on"]["push"] == {"branches": ["main"], "paths-ignore": ["LICENSE"]}
    assert ci["on"]["pull_request"] == {"paths-ignore": ["LICENSE"]}, "tests read README and docs"
    inputs = ci["on"]["workflow_call"]["inputs"]
    assert inputs["release"]["default"] is False and inputs["skip_ui"]["default"] is False
    assert list(ci["jobs"]) == ["rust", "msrv", "telemetry", "ui", "public", "package"]
    package = ci["jobs"]["package"]
    assert package["if"] == (
        "github.ref == 'refs/heads/main' && github.event_name != 'pull_request' && github.workflow == 'CI'"
    )
    assert package["needs"] == ["rust", "msrv", "ui"]
    assert ci["jobs"]["ui"]["if"] == "${{ !inputs.skip_ui }}"
    assert ci["jobs"]["public"]["if"] == "github.event_name == 'pull_request'"
    env = ci["env"]
    assert env["CARGO_PROFILE_DEV_DEBUG"] == env["CARGO_PROFILE_TEST_DEBUG"] == "line-tables-only"
    assert env["CAIRN_REQUIRE_RESOURCES"] == "1"
    assert env["CAIRN_RELEASE"] == "${{ inputs.release && '1' || '0' }}"


def test_ci_mirrors_the_local_checks() -> None:
    jobs = load("ci.yml")["jobs"]
    rust = " ".join(str(step.get("run", "")) for step in jobs["rust"]["steps"])
    assert "cargo fmt --all -- --check" in rust
    assert "cargo clippy --workspace --all-targets --locked -- -D warnings" in rust
    assert "cargo test --workspace --locked" in rust
    # The workspace run leaves out the tests inside optctl.exe (crates/cli/Cargo.toml).
    assert "cargo test -p optctl --bin optctl --locked" in rust
    msrv = " ".join(str(step.get("run", "")) for step in jobs["msrv"]["steps"])
    assert "cargo +1.80 check --workspace --all-targets --locked" in msrv
    telemetry = " ".join(str(step.get("run", "")) for step in jobs["telemetry"]["steps"])
    assert "-DTEL_AVX2=OFF" in telemetry and "telemetry_smoke.exe" in telemetry
    ui = jobs["ui"]
    tests = step_named(ui, "UI tests")
    assert tests["shell"] == "powershell"
    assert tests["env"] == {"CAIRN_REQUIRE_NATIVES": "1"}
    assert tests["run"].startswith(".\\scripts\\pytest_serial.ps1 tests -q")
    assert step_named(ui, "ruff")["run"] == "..\\.venv\\Scripts\\python.exe -m ruff check ."
    public = " ".join(str(step.get("run", "")) for step in jobs["public"]["steps"])
    assert "scripts/check_public.py --generic-only" in public


def test_the_display_step_cannot_fail_the_job() -> None:
    display = step_named(load("ci.yml")["jobs"]["ui"], "Display")["run"]
    lines = display.splitlines()
    assert lines[0] == "try {" and lines[-1] == "} catch { Write-Warning $_ }"
    calls = [line.strip() for line in lines if RESOLUTION_CMDLET in line and "Get-Command" not in line]
    assert calls == [
        f"try {{ {RESOLUTION_CMDLET} -Width 1920 -Height 1080 -Force }} catch {{ Write-Warning $_ }}"
    ]


def test_caches_are_saved_only_from_main_and_never_in_the_release() -> None:
    for job_id, step in steps(load("ci.yml")):
        if "rust-cache@" in str(step.get("uses", "")):
            assert step["with"]["save-if"] == "${{ github.ref == 'refs/heads/main' }}", job_id
    for job_id, step in steps(load("release.yml")):
        assert "cache" not in str(step.get("uses", "")), job_id
        assert "cache" not in (step.get("with") or {}), job_id
        assert "cache-dependency-path" not in (step.get("with") or {}), job_id


# ----------------------------------------------------------------------------- setup and release


def test_the_setup_is_built_with_the_pinned_inno_setup_and_the_runners_python() -> None:
    for name, job_id in (("ci.yml", "package"), ("release.yml", "release")):
        job = load(name)["jobs"][job_id]
        inno = index_of(job, ".\\.github\\scripts\\install_inno_setup.ps1")
        build = [i for i, step in enumerate(job["steps"]) if "build_release.ps1" in str(step.get("run", ""))]
        assert build and inno < build[0], name
        for i in build:
            step = job["steps"][i]
            assert step["shell"] == "powershell", "the build script runs in Windows PowerShell"
            if "-Python" in step["run"] or "Python = " in step["run"]:
                assert step["env"]["BASE_PYTHON"] == "${{ steps.python.outputs.python-path }}"
        assert any("-Python $env:BASE_PYTHON" in job["steps"][i]["run"] for i in build), name
    script = (GITHUB / "scripts" / "install_inno_setup.ps1").read_text(encoding="ascii")
    url = re.search(r"\[string\]\$Url = '([^']+)'", script)
    sha = re.search(r"\[string\]\$Sha256 = '([^']+)'", script)
    assert (
        url
        and url.group(1)
        == "https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe"
    )
    assert sha and SHA256.match(sha.group(1))
    assert "Get-FileHash" in script and "ISCC=" in script and "GITHUB_ENV" in script


def test_scripts_that_install_refuse_to_run_outside_a_github_hosted_runner() -> None:
    guard = "if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {"
    for name in ("install_inno_setup.ps1", "install_smoke.ps1"):
        text = (GITHUB / "scripts" / name).read_text(encoding="ascii")
        code = [line for line in text.splitlines() if line.strip() and not line.lstrip().startswith("#")]
        first = next(i for i, line in enumerate(code) if line.startswith("$ErrorActionPreference"))
        assert code[first + 1] == guard, name
        assert code[first + 2].strip().startswith("throw "), name


def test_the_install_smoke_test_installs_upgrades_and_uninstalls() -> None:
    for name, job_id in (("ci.yml", "package"), ("release.yml", "release")):
        job = load(name)["jobs"][job_id]
        smoke = index_of(job, ".\\.github\\scripts\\install_smoke.ps1 -Dist build\\dist")
        assert job["steps"][smoke]["continue-on-error"] is True, name
        finished = [
            i for i, step in enumerate(job["steps"]) if "build_release.ps1" in str(step.get("run", ""))
        ]
        assert finished and finished[-1] < smoke, "after the release files"
    script = (GITHUB / "scripts" / "install_smoke.ps1").read_text(encoding="ascii")
    assert "'https://github.com/Dray973/Cairn/releases/download/v0.2.0/Cairn-0.2.0-setup.exe'" in script
    sha = re.search(r"\[string\]\$PreviousSha256 = '([^']+)'", script)
    assert sha and SHA256.match(sha.group(1))
    for expected in (
        "'/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'",
        "--self-test",
        "$result.installed -ne $true",
        "journal pending",
        "unins000.exe",
        "Get-ScheduledTask -TaskPath '\\Cairn\\'",
        "Install-Setup $previous 'previous'",
        "Install-Setup $Setup 'upgrade'",
    ):
        assert expected in script, expected
    app_rs = (ROOT / "crates" / "core" / "src" / "app.rs").read_text(encoding="utf-8")
    key = re.search(r'pub const UNINSTALL_KEY: &str\s*=\s*r?"([^"]+)";', app_rs)
    assert key and f"$UninstallKey = 'HKLM:\\{key.group(1)}'" in script, "the uninstall entry of the setup"


def test_the_release_follows_the_checks_and_drafts_from_a_tag() -> None:
    release = load("release.yml")
    assert release["on"]["push"] == {"tags": ["v[0-9]+.[0-9]+.[0-9]+"]}
    ui = release["on"]["workflow_dispatch"]["inputs"]["ui_ci_required"]
    assert ui["type"] == "boolean" and ui["default"] is True
    checks = release["jobs"]["checks"]
    assert checks["uses"] == "./.github/workflows/ci.yml"
    assert checks["with"] == {
        "release": True,
        "skip_ui": "${{ github.event_name == 'workflow_dispatch' && !inputs.ui_ci_required }}",
    }
    assert release["env"]["CAIRN_RELEASE"] == "1"
    job = release["jobs"]["release"]
    assert job["needs"] == "checks"
    assert job["environment"] == "${{ github.ref_type == 'tag' && 'release' || 'signing-test' }}"
    assert step_named(job, "Attestation")["if"] == "github.ref_type == 'tag'"
    assert "actions/attest-build-provenance@" in step_named(job, "Attestation")["uses"]
    draft = step_named(job, "Draft release")
    assert draft["if"] == "github.ref_type == 'tag'"
    assert "--draft --verify-tag" in draft["run"] and "symbols" not in draft["run"]
    symbols = step_named(job, "Symbols and build information")
    assert symbols["with"]["retention-days"] == "90"
    notes = step_named(job, "Release notes for UI tests run locally")
    assert notes["if"] == "env.UI_CI_REQUIRED != 'true'"
    assert "UI tests ran locally (pytest_serial.ps1) for this release." in notes["run"]
    stage = job["steps"][index_of(job, "-StageOnly -SigningDir build\\sign")]
    assert "-SkipChecks" in stage["run"], "the checks ran in the checks job"


def test_signing_stays_off_until_signpath_is_configured() -> None:
    job = load("release.yml")["jobs"]["release"]
    signing = [step for step in job["steps"] if "submit-signing-request@" in str(step.get("uses", ""))]
    uploads = [step for step in job["steps"] if step.get("id") in ("unsigned-binaries", "unsigned-setup")]
    assert len(signing) == 2 and len(uploads) == 2
    for step in signing + uploads:
        assert step["if"] == "env.SIGNING_MODE != 'none'"
    for step, configuration in zip(signing, ("binaries", "setup"), strict=True):
        assert step["with"]["artifact-configuration-slug"] == configuration
        assert step["with"]["api-token"] == f"${{{{ secrets.{SIGN_PATH.upper()}_API_TOKEN }}}}"
        assert step["with"]["organization-id"] == f"${{{{ vars.{SIGN_PATH.upper()}_ORGANIZATION_ID }}}}"
        assert step["with"]["project-slug"] == "cairn"
        assert step["with"]["parameters"] == 'version: "${{ env.CAIRN_VERSION }}"\n'
    mode = step_named(job, "Version and signing mode")["run"]
    assert f"if ($env:{SIGN_PATH.upper()}_ORGANIZATION_ID) {{" in mode
    assert "$mode = 'release'; $policy = 'release-signing'" in mode
    assert "$mode = 'test'; $policy = 'test-signing'" in mode


def test_release_notes_show_release_verify_once_releases_are_immutable() -> None:
    # GitHub attests a release when it publishes it as immutable; the repository turns that on
    # after its first automated release, and the variable tells the build when it is on.
    job = load("release.yml")["jobs"]["release"]
    assert job["env"]["IMMUTABLE_RELEASES"] == "${{ vars.IMMUTABLE_RELEASES }}"
    files = step_named(job, "Release files")["run"]
    assert "if ($env:IMMUTABLE_RELEASES -eq 'true') { $arguments.ImmutableReleases = $true }" in files
    for path in workflow_files():
        for job_id, step in steps(load(path.name)):
            if "ImmutableReleases" in str(step.get("run", "")):
                assert (path.name, step.get("name")) == ("release.yml", "Release files"), job_id


def test_release_notes_show_attestation_verify_only_when_the_run_attests() -> None:
    # Only a tag run attests the setup: the notes of a rehearsal on main, or of a build made
    # anywhere else, would send people to an attestation that does not exist.
    job = load("release.yml")["jobs"]["release"]
    assert job["env"]["REF_TYPE"] == "${{ github.ref_type }}"
    assert step_named(job, "Attestation")["if"] == "github.ref_type == 'tag'"
    files = step_named(job, "Release files")["run"]
    assert "if ($env:REF_TYPE -eq 'tag') { $arguments.Attested = $true }" in files
    for path in workflow_files():
        for job_id, step in steps(load(path.name)):
            if "Attested" in str(step.get("run", "")):
                assert (path.name, step.get("name")) == ("release.yml", "Release files"), job_id


def test_a_rerun_replaces_only_a_drafts_files_and_notes() -> None:
    # A rerun builds a new setup: the draft's notes name its SHA-256, so they are replaced with
    # the files, and a published release is never changed.
    draft = step_named(load("release.yml")["jobs"]["release"], "Draft release")["run"]
    assert "--json isDraft --jq .isDraft" in draft
    assert "if ($draft -ne 'true') { throw" in draft
    assert "release edit $env:REF_NAME --notes-file build/dist/release-notes.md" in draft


def test_no_signing_secret_or_id_is_in_the_repository() -> None:
    app_rs = (ROOT / "crates" / "core" / "src" / "app.rs").read_text(encoding="utf-8")
    known = {match.upper() for match in GUID.findall(app_rs)}
    files = [path for folder in (GITHUB, SIGNING.parent) for path in folder.rglob("*") if path.is_file()]
    files += [BUILD_SCRIPT, ROOT / "docs" / "code-signing-policy.md"]
    for path in files:
        text = path.read_text(encoding="utf-8")
        assert {match.upper() for match in GUID.findall(text)} <= known, path.name
        for line in text.splitlines():
            if "api-token:" in line or "organization-id:" in line:
                assert "${{ secrets." in line or "${{ vars." in line, f"{path.name}: {line.strip()}"


def test_dependabot_proposes_weekly_updates() -> None:
    dependabot = parse_yaml((GITHUB / "dependabot.yml").read_text(encoding="utf-8"))
    assert dependabot["version"] == "2"
    assert {
        (u["package-ecosystem"], u["directory"], u["schedule"]["interval"]) for u in dependabot["updates"]
    } == {
        ("github-actions", "/", "weekly"),
        ("cargo", "/", "weekly"),
        ("pip", "/", "weekly"),
    }


def test_the_toolchains_come_from_the_repository() -> None:
    for path in workflow_files():
        for job_id, step in steps(load(path.name)):
            run = str(step.get("run", ""))
            if "rustup toolchain install" in run:
                assert run in (
                    "rustup toolchain install",
                    "rustup toolchain install 1.80 --profile minimal",
                ), (
                    path.name,
                    job_id,
                )
            if "actions/setup-python@" in str(step.get("uses", "")):
                assert step["with"]["python-version"] == "3.12.10", (path.name, job_id)
    pins = (ROOT / "requirements-dev.txt").read_text(encoding="utf-8")
    for pin in ("ruff==0.16.8", "pytest==9.1.1", "pytest-timeout==2.4.0"):
        assert re.search(rf"(?m)^{re.escape(pin)}(\s|$)", pins), pin


# ----------------------------------------------------------------------------- signing configuration


def test_the_signing_configurations_restrict_every_file() -> None:
    namespace = {"a": "http://signpath.io/artifact-configuration/v1"}
    script = BUILD_SCRIPT.read_text(encoding="ascii")
    own = script.split("$OwnFiles = [ordered]@{", 1)[1].split("}", 1)[0]
    uploads = {value.replace("\\", "/") for value in re.findall(r"= '([^']+)'", own)}
    assert len(uploads) == 5
    expected = {"binaries": uploads | {"uninstaller/uninst.exe"}, "setup": {"Cairn-${version}-setup.exe"}}
    for name, files in expected.items():
        text = (SIGNING / f"{name}.xml").read_text(encoding="utf-8")
        assert "60 characters" in text and "50 characters" in text, "the padding of Inno Setup's strings"
        root = ET.fromstring(text)
        parameters = root.findall("a:parameters/a:parameter", namespace)
        assert [p.attrib for p in parameters] == [{"name": "version"}], "no default and no extra attribute"
        found: set[str] = set()
        for element in root.iter():
            tag = element.tag.split("}", 1)[1]
            if tag not in ("pe-file", "pe-file-set"):
                continue
            assert element.get("product-name") == "Cairn", name
            assert element.get("product-version") == "${version}", name
            folders = [
                parent.get("path")
                for parent in root.iter()
                if element in list(parent) and parent.tag.endswith("}directory")
            ]
            prefix = folders[0] + "/" if folders else ""
            if tag == "pe-file":
                found.add(prefix + element.get("path", ""))
            else:
                found.update(
                    prefix + include.get("path", "") for include in element.findall("a:include", namespace)
                )
            assert element.findall(".//a:authenticode-sign", namespace), name
        assert found == files, name


# ----------------------------------------------------------------------------- release documents


CORE = ROOT / "crates" / "core" / "src"


def tool_entries() -> dict[str, dict[str, str]]:
    """The maintenance tools of `tools/catalog.rs` by `ToolId` variant: id, title, duration hint
    and the services each one depends on."""
    text = (CORE / "tools" / "catalog.rs").read_text(encoding="utf-8")
    as_str = text.split("fn as_str(self)", 1)[1].split("\n    }\n", 1)[0]
    ids = dict(re.findall(r'ToolId::(\w+) => "(\w+)"', as_str))
    entries: dict[str, dict[str, str]] = {}
    for entry in text.split("static CATALOG:", 1)[1].split("\n];", 1)[0].split("ToolInfo {")[1:]:
        fields = {
            "variant": re.search(r"\bid: ToolId::(\w+),", entry),
            "title": re.search(r'\btitle: "([^"]+)",', entry),
            "hint": re.search(r'\bduration_hint: "([^"]+)",', entry),
            "services": re.search(r"\bservices: &\[([^\]]*)\],", entry),
        }
        assert all(fields.values()), entry
        found = {name: match.group(1) for name, match in fields.items() if match}
        entries[found["variant"]] = {**found, "id": ids[found["variant"]]}
    assert entries and len(entries) == len(ids)
    return entries


def tools_that_download() -> dict[str, str]:
    """Titles of the tools that need the internet (their hint says so, or they depend on the
    Windows Update service), by `ToolId` variant."""
    return {
        variant: entry["title"]
        for variant, entry in tool_entries().items()
        if "internet" in entry["hint"] or "WINDOWS_UPDATE" in entry["services"]
    }


def test_the_privacy_statement_names_every_tool_that_downloads() -> None:
    downloads = tools_that_download()
    assert downloads.get("DismRestore") == "Repair the component store", "DISM downloads its repair files"
    privacy = (ROOT / "docs" / "privacy.md").read_text(encoding="utf-8")
    network = privacy.split("## When Cairn uses the network", 1)[1].split("\n## ", 1)[0]
    for title in downloads.values():
        assert f"**{title}.**" in network, title


def test_scheduled_maintenance_runs_no_tool_that_downloads() -> None:
    privacy = (ROOT / "docs" / "privacy.md").read_text(encoding="utf-8")
    assert "Scheduled maintenance makes no network connection." in privacy
    entries = tool_entries()
    used: set[str] = set()
    for path in sorted((CORE / "maintenance").rglob("*.rs")):
        if path.name == "tests.rs":
            continue
        code = path.read_text(encoding="utf-8").split("#[cfg(test)]", 1)[0]
        used.update(re.findall(r"ToolId::(\w+)", code))
        used.update(variant for variant, entry in entries.items() if f'"{entry["id"]}"' in code)
    assert {"SfcVerify", "DismCheck"} <= used <= set(entries), used
    assert not used & set(tools_that_download())


def test_the_signing_policy_promises_approval_of_release_signatures_only() -> None:
    # A manual run on main asks for a test signature; only a tag run asks for a release
    # signature, and the approval by hand that the policy promises is for those.
    mode = step_named(load("release.yml")["jobs"]["release"], "Version and signing mode")["run"]
    assert "if ($env:REF_TYPE -eq 'tag') { $mode = 'release'; $policy = 'release-signing' }" in mode
    policy = " ".join((ROOT / "docs" / "code-signing-policy.md").read_text(encoding="utf-8").split())
    approvals = re.findall(r"[^.]*\bapproved by hand\b[^.]*\.", policy)
    assert approvals, "the policy says who approves a signature"
    assert all("release signature" in sentence for sentence in approvals), approvals


def test_the_signing_policy_promises_an_attestation_only_from_the_release_workflow() -> None:
    # Only the release workflow's tag runs attest the setup: 0.2.0 was built elsewhere and has
    # no attestation, and neither has any later release built elsewhere.
    attestation = step_named(load("release.yml")["jobs"]["release"], "Attestation")
    assert attestation["if"] == "github.ref_type == 'tag'"
    policy = (ROOT / "docs" / "code-signing-policy.md").read_text(encoding="utf-8")
    prose = re.sub(r"```.*?```", "", policy, flags=re.DOTALL)
    paragraphs = [" ".join(block.split()) for block in prose.split("\n\n")]
    attested = [paragraph for paragraph in paragraphs if "attestation" in paragraph]
    assert len(attested) >= 2, "the status and the verification paragraphs speak of the attestation"
    for paragraph in attested:
        assert "release workflow" in paragraph, paragraph


def test_the_security_policy_works_without_private_reporting() -> None:
    # Private vulnerability reporting is a repository setting that can be off; the policy then
    # still names a way to reach the maintainer that publishes no details.
    security = (ROOT / "SECURITY.md").read_text(encoding="utf-8")
    reporting = " ".join(security.split("## Reporting a vulnerability", 1)[1].split("\n## ", 1)[0].split())
    assert "(https://github.com/Dray973/Cairn/security/advisories/new)" in reporting
    assert "(https://github.com/Dray973/Cairn/issues/new)" in reporting
    assert "private contact" in reporting and "without any details" in reporting


# ----------------------------------------------------------------------------- public data check


@pytest.fixture
def check_public() -> ModuleType:
    spec = importlib.util.spec_from_file_location("check_public", ROOT / "scripts" / "check_public.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_the_public_check_of_pull_requests_needs_nothing_of_the_pc(
    check_public: ModuleType,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    def no_machine() -> None:
        raise AssertionError("the generic check reads nothing of the PC")

    monkeypatch.setattr(check_public, "machine_needles", no_machine)
    monkeypatch.setattr(check_public, "environment_needles", no_machine)
    clean = tmp_path / "clean"
    clean.mkdir()
    (clean / "notes.md").write_text(
        "Write to the GitHub noreply address 1+someone@users.noreply.github.com.\n"
        "A URL check refuses https://user@example.com/ and mail@host.test.\n"
        "The fixture user lives in C:\\Users\\Test.\n",
        encoding="utf-8",
    )
    assert check_public.main(["--generic-only", "--root", str(clean)]) == 0
    dirty = tmp_path / "dirty"
    dirty.mkdir()
    (dirty / "notes.md").write_text(
        "Mail someone@" + "mail.example-company.com.\nC:\\Users\\" + "somebody\\Desktop\n", encoding="utf-8"
    )
    assert check_public.main(["--generic-only", "--root", str(dirty)]) == 1
    output = capsys.readouterr().out
    assert "notes.md:1: e-mail address" in output
    assert "notes.md:2: path under C:\\Users other than Test" in output


def test_the_public_check_finds_this_pcs_ipv4_addresses_as_whole_addresses(
    check_public: ModuleType, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    assert "Read-Values 'IPv4 addresses' {" in check_public.IDENTIFIER_QUERY
    # A documentation address and a link-local one stand in for this PC's; loopback is every PC's.
    found = {"IPv4 addresses": ["127.0.0.1", "0.0.0.0", "192.0.2.5", "169.254.20.7"]}
    monkeypatch.setattr(check_public, "query_identifiers", lambda: found)
    monkeypatch.setattr(check_public, "machine_guid", lambda: None)
    needles, runs, failed = check_public.machine_needles()
    assert failed == ["MachineGuid"] and runs == set()
    rule = "IPv4 address of this PC"
    assert [(n.rule, n.value, n.whole_word) for n in needles] == [
        (rule, "169.254.20.7", True),
        (rule, "192.0.2.5", True),
    ]
    notes = tmp_path / "notes.md"
    notes.write_text(
        "The gateway is 192.0.2.5.\n"
        "Another host is 192.0.2.50 and the loopback 127.0.0.1.\n"
        "A cable-less adapter keeps 169.254.20.7/16.\n",
        encoding="utf-8",
    )
    hits = check_public.check_file(notes, "notes.md", needles, [n.pattern() for n in needles])
    assert [str(hit) for hit in hits] == [f"notes.md:1: {rule}", f"notes.md:3: {rule}"]
