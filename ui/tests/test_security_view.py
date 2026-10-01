"""Pure helpers of the Security section: sorting, severity chips and colours, score texts,
the Windows Update progress text, which fix buttons are enabled and the URI allowlist.

Nothing here builds a window or calls the engine.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from typing import Any

import pytest

from optimizer import theme
from optimizer.widgets.security import (
    ADMIN_TOOL_NOTE,
    BITLOCKER_URI,
    BUSY_NOTE,
    LEFT_OUT_TEXT,
    PER_USER_NOTE,
    READ_ONLY_TEXT,
    SEVERITY_CHIPS,
    allowed_uri,
    elapsed_text,
    fix_state,
    meta_text,
    passed_prefix,
    scan_text,
    score_texts,
    sort_checks,
    state_style,
)

from .fake_health import CHECKS, check, score_of


def fix(kind: str, **action: Any) -> dict[str, Any]:
    return {"label": "Fix", "note": None, "action": {"kind": kind, **action}}


def test_findings_sort_by_severity_then_engine_order() -> None:
    checks = [
        check("antivirus", "good", "On"),
        check("remote_assistance", "attention", "Invitations are allowed", "low"),
        check("firewall", "attention", "Off for public networks", "high"),
        check("tpm", "unknown", "Unknown"),
        check("uac", "attention", "Off", "critical"),
        check("smb1", "attention", "Installed", "high"),
        check("pending_updates", "checking", "Checking for waiting updates…"),
        check("remote_desktop", "not_applicable", "Not available on Windows 11 Home"),
        check("smart_app_control", "attention", "Off", "info"),
    ]
    to_fix, not_checked, passed = sort_checks(checks)
    assert [c["id"] for c in to_fix] == ["uac", "firewall", "smb1", "remote_assistance"]
    assert [c["id"] for c in not_checked] == ["tpm", "pending_updates"]
    # Informational findings and not-applicable checks stay with the passed ones, in order.
    assert [c["id"] for c in passed] == ["antivirus", "remote_desktop", "smart_app_control"]


def test_chips_icons_and_colours() -> None:
    assert SEVERITY_CHIPS["critical"] == ("Critical", "⚠", theme.CRITICAL)
    assert SEVERITY_CHIPS["high"] == ("Important", "⚠", theme.SERIOUS)
    assert SEVERITY_CHIPS["medium"] == ("Recommended", "⚠", theme.WARNING)
    assert SEVERITY_CHIPS["low"] == ("Optional", "○", theme.INK_SECONDARY)
    assert SEVERITY_CHIPS["info"] == ("Info", "–", theme.INK_MUTED)
    assert state_style(check("uac", "attention", "Off", "critical")) == ("⚠", theme.CRITICAL)
    assert state_style(check("tpm", "attention", "TPM 1.2", "low")) == ("○", theme.INK_SECONDARY)
    assert state_style(check("tpm", "good", "TPM 2.0")) == ("✓", theme.GOOD)
    assert state_style(check("tpm", "unknown", "Unknown")) == ("○", theme.INK_MUTED)
    assert state_style(check("pending_updates", "checking", "…")) == ("◐", theme.ACCENT)
    assert state_style(check("remote_desktop", "not_applicable", "…")) == ("–", theme.INK_MUTED)
    # Every status colour ships with an icon.
    for _chip, icon, _color in SEVERITY_CHIPS.values():
        assert icon


@pytest.mark.parametrize(
    ("score", "expected"),
    [
        (
            {"value": 100, "grade": "good", "to_fix": 0, "unknown": 0},
            ("100", "✓ Well protected", theme.GOOD, "Nothing to fix"),
        ),
        (
            {"value": 82, "grade": "fair", "to_fix": 3, "unknown": 1},
            (
                "82",
                "⚠ Needs attention",
                theme.WARNING,
                f"3 to fix  ·  1 could not be checked  ·  {LEFT_OUT_TEXT}",
            ),
        ),
        (
            {"value": 60, "grade": "at_risk", "to_fix": 1, "unknown": 0},
            ("60", "⚠ At risk", theme.CRITICAL, "1 to fix"),
        ),
        (
            {"value": 97, "grade": "good", "to_fix": 0, "unknown": 2},
            (
                "97",
                "✓ Well protected",
                theme.GOOD,
                f"Nothing to fix  ·  2 could not be checked  ·  {LEFT_OUT_TEXT}",
            ),
        ),
    ],
)
def test_score_texts_for_each_grade(score: dict[str, Any], expected: tuple[str, str, str, str]) -> None:
    assert score_texts(score) == expected


def test_score_texts_tolerate_missing_values() -> None:
    assert score_texts({}) == ("–", "", theme.INK_SECONDARY, "Nothing to fix")


def test_meta_text() -> None:
    text = meta_text({"taken_at": "2026-09-28T10:12:00+00:00", "duration_ms": 1300})
    assert text.startswith("Checked ")
    assert " in 1.3 s  ·  " in text
    assert text.endswith(READ_ONLY_TEXT)
    assert "under 0.1 s" in meta_text({"taken_at": "2026-09-28T10:12:00+00:00", "duration_ms": 20})
    assert meta_text({}) == READ_ONLY_TEXT


def test_scan_text_counts_from_the_start() -> None:
    now = datetime(2026, 9, 28, 10, 0, 30, tzinfo=UTC)
    offline = {"online": False, "started_at": (now - timedelta(seconds=12)).isoformat(), "elapsed_ms": 11_000}
    assert scan_text(offline, now) == "Checking for waiting updates…  ·  12 s"
    online = {"online": True, "started_at": (now - timedelta(seconds=80)).isoformat(), "elapsed_ms": 0}
    assert scan_text(online, now) == "Checking Windows Update online…  ·  1 min 20 s"
    # Without a start time the engine's elapsed time is used; an engine "Z" time parses too.
    assert scan_text({"online": False, "elapsed_ms": 5_400}, now) == "Checking for waiting updates…  ·  5 s"
    zulu = {"online": True, "started_at": "2026-09-28T10:00:00Z", "elapsed_ms": 0}
    assert scan_text(zulu, now) == "Checking Windows Update online…  ·  30 s"
    assert elapsed_text(0) == "0 s"
    assert elapsed_text(59.9) == "59 s"
    assert elapsed_text(3600) == "60 min 0 s"


def test_fix_state_of_a_per_user_tweak() -> None:
    extensions = check("file_extensions", "attention", "Hidden", "low")
    tweak = fix("tweak", id="interface.file_extensions")
    assert fix_state(extensions, tweak, elevated=False, other_user=False, busy=False) == (True, "")
    assert fix_state(extensions, tweak, elevated=True, other_user=True, busy=False) == (False, PER_USER_NOTE)
    assert fix_state(extensions, tweak, elevated=True, other_user=None, busy=False) == (False, PER_USER_NOTE)
    assert fix_state(extensions, tweak, elevated=True, other_user=False, busy=True) == (False, BUSY_NOTE)
    # A machine-wide tweak does not depend on the account.
    machine = check("smb1", "attention", "Installed", "high")
    assert fix_state(machine, tweak, elevated=True, other_user=True, busy=False) == (True, "")


def test_fix_state_of_tools_and_other_actions() -> None:
    ra = check("remote_assistance", "attention", "Invitations are allowed", "low")
    admin_tool = fix("windows_tool", tool="remote_settings", requires_admin=True)
    assert fix_state(ra, admin_tool, elevated=False, other_user=False, busy=False) == (True, ADMIN_TOOL_NOTE)
    assert fix_state(ra, admin_tool, elevated=True, other_user=False, busy=True) == (True, "")
    user_tool = fix("windows_tool", tool="uac_settings", requires_admin=False)
    assert fix_state(ra, user_tool, elevated=False, other_user=True, busy=True) == (True, "")
    for other in (
        fix("uri", uri="ms-settings:windowsupdate"),
        fix("update_scan", online=True),
        fix("elevate"),
    ):
        assert fix_state(ra, other, elevated=False, other_user=None, busy=True) == (True, "")


@pytest.mark.parametrize(
    "uri",
    [
        "ms-settings:windowsupdate",
        "ms-settings:deviceencryption",
        "windowsdefender://threat/",
        "windowsdefender://history",
        "WindowsDefender://network/",
        "windowsdefender://administratorprotection/",
        BITLOCKER_URI,
    ],
)
def test_allowed_uris(uri: str) -> None:
    assert allowed_uri(uri)


@pytest.mark.parametrize(
    "uri",
    [
        "http://example.com/",
        "https://example.com/",
        "file:///C:/Windows/System32/cmd.exe",
        "ms-settings:",
        "windowsdefender://quickscan/",
        "windowsdefender://fullscan/",
        "windowsdefender://enablertp/",
        "windowsdefender://update/",
        "windowsdefender://updateandquickscan/",
        "windowsdefender://wdoscan/",
        "windowsdefender://reboot/",
        "windowsdefender://",
        "shell:startup",
        "shell:::{00000000-0000-0000-0000-000000000000}",
        "",
    ],
)
def test_refused_uris(uri: str) -> None:
    assert not allowed_uri(uri)


def test_passed_lines_carry_their_group() -> None:
    assert passed_prefix(check("firewall", "good", "On")) == "Firewall & network  ·  "
    assert passed_prefix({"group": "nowhere"}) == ""


def test_fake_score_matches_the_engine_vectors() -> None:
    """The fake computes the score with a copy of the engine's penalty table; these are the
    engine's own test vectors."""

    def attention(severity: str) -> dict[str, Any]:
        return check("firewall", "attention", "x", severity)

    one_high = score_of([attention("high")])
    assert (one_high["value"], one_high["grade"]) == (80, "fair")
    one_medium = score_of([attention("medium")])
    assert (one_medium["value"], one_medium["grade"]) == (92, "good")
    one_critical = score_of([attention("critical")])
    assert (one_critical["value"], one_critical["grade"]) == (60, "at_risk")
    two_high = score_of([attention("high"), attention("high")])
    assert (two_high["value"], two_high["grade"]) == (60, "fair")
    unknown = score_of([check("tpm", "unknown", "x"), check("tpm", "checking", "x")])
    assert (unknown["value"], unknown["unknown"], unknown["checked"]) == (100, 2, 0)
    assert len(CHECKS) == 24
