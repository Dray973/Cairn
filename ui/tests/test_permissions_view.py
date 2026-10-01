"""Pure helpers of the Permissions guide: the Windows Settings pages it opens, the texts of its cards,
the times of recent use, the warning banner, and the README's description of the section. No window
is created."""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

from optimizer import APP_NAME
from optimizer.widgets import history, permissions
from optimizer.widgets.permissions import (
    CAPABILITIES,
    CAPABILITY_LABELS,
    CAPABILITY_NOUNS,
    IN_USE_TEXT,
    MAX_BANNER_LINES,
    SETTINGS_PAGES,
    banner_text,
    button_text,
    guide_text,
    recent_time,
    settings_page,
)

from .fake_permissions import CHANGE_REFUSED, FakePermissions
from .fake_permissions import SETTINGS_PAGES as ENGINE_PAGES

README = Path(__file__).resolve().parents[2] / "README.md"


def fake_guide() -> dict[str, Any]:
    """The fake engine's guide, without a FakeEngine around it."""
    fake = FakePermissions.__new__(FakePermissions)
    fake._record = lambda *_a: None  # type: ignore[method-assign]
    fake._init_permissions({})
    return fake.permissions_list()


def test_each_capability_opens_its_privacy_page_in_windows_settings() -> None:
    assert CAPABILITIES == ("camera", "microphone", "location")
    assert SETTINGS_PAGES == {
        "camera": "ms-settings:privacy-webcam",
        "microphone": "ms-settings:privacy-microphone",
        "location": "ms-settings:privacy-location",
    }
    assert {c: settings_page(c) for c in CAPABILITIES} == SETTINGS_PAGES
    for other in ("", "Camera", "webcam", "windowsupdate", "ms-settings:privacy-webcam"):
        assert settings_page(other) is None, other
    # The pages are the engine's (the fake mirrors them).
    assert ENGINE_PAGES == SETTINGS_PAGES
    pages = [(c["capability"], c["settings_uri"]) for c in fake_guide()["capabilities"]]
    assert pages == list(SETTINGS_PAGES.items())


def test_buttons_name_the_device_whose_settings_they_open() -> None:
    assert [button_text(c) for c in CAPABILITIES] == [
        "Open camera settings",
        "Open microphone settings",
        "Open location settings",
    ]


def test_cards_explain_that_windows_settings_manages_the_permissions() -> None:
    assert guide_text("camera") == (
        "Windows 11 manages which apps may use your camera itself, in Settings › Privacy & security › "
        f"Camera. On this version of Windows an app like {APP_NAME} cannot change these permissions."
    )
    for capability in CAPABILITIES:
        text = guide_text(capability)
        assert f"Settings › Privacy & security › {CAPABILITY_LABELS[capability]}." in text
        assert f"use your {CAPABILITY_NOUNS[capability]} itself" in text
        assert "cannot change these permissions" in text


def test_recent_use_shows_in_use_or_the_local_time_of_the_last_use() -> None:
    assert IN_USE_TEXT.split(" ", 1)[0] == "◐", "the state carries a glyph"
    assert recent_time({"path": "a.exe", "last_used": "2026-09-25T10:00:00Z", "in_use": True}) == IN_USE_TEXT
    used = recent_time({"path": "a.exe", "last_used": "2026-09-25T10:00:00Z", "in_use": False})
    assert used == f"last used {history.local_time('2026-09-25T10:00:00Z')}"
    assert recent_time({"path": "a.exe", "last_used": None, "in_use": False}) == ""


def test_banner_lists_the_read_warnings() -> None:
    assert banner_text({"warnings": []}) == ""
    assert banner_text({}) == ""
    one = {"warnings": ["Camera: cannot list the desktop apps that used the camera: denied"]}
    assert banner_text(one) == "⚠ Camera: cannot list the desktop apps that used the camera: denied"
    many = {"warnings": [f"Camera: problem {n}" for n in range(5)]}
    lines = banner_text(many).split("\n")
    assert len(lines) == MAX_BANNER_LINES + 1
    assert lines[-1] == "⚠ …and 2 more"


def test_the_section_offers_no_switch_and_no_permission_state() -> None:
    # Nothing of the switches, states or changes of earlier builds is left in the guide.
    gone = ("STATE_STYLE", "SWITCH_TITLES", "segment_value", "can_change", "PermissionRow", "FOOTER_TEXT")
    for name in gone:
        assert not hasattr(permissions, name), name
    source = Path(permissions.__file__).read_text(encoding="utf-8")
    for word in ("Allowed", "Blocked", "Asks first", "CTkSegmentedButton", "CTkSwitch"):
        assert word not in source, word
    guide = fake_guide()
    assert set(guide) == {"capabilities", "warnings"}
    for capability in guide["capabilities"]:
        assert set(capability) == {"capability", "label", "settings_uri", "recent_desktop_apps"}
        for use in capability["recent_desktop_apps"]:
            assert set(use) == {"path", "last_used", "in_use"}


def test_the_refusal_says_where_permissions_are_changed() -> None:
    assert CHANGE_REFUSED.startswith(f"{APP_NAME} does not change app permissions: ")
    assert "Settings › Privacy & security" in CHANGE_REFUSED
    assert CHANGE_REFUSED.endswith("Nothing was changed.")


def readme_lines() -> list[str]:
    return README.read_text(encoding="utf-8").splitlines()


def test_the_readme_describes_the_section_as_a_guide() -> None:
    lines = readme_lines()
    section = next(line for line in lines if line.startswith("- **Permissions**:"))
    assert "Windows 11 manages these permissions itself, in Settings › Privacy & security" in section
    assert "cannot change them" in section
    assert "opens its page in Settings" in section
    assert "read-only list" in section
    promises = ("Allow or Deny", "turned off and on here", "Every change is recorded", "switches Windows")
    for promise in promises:
        assert promise not in section, promise


def test_the_readme_promises_no_permission_change() -> None:
    text = README.read_text(encoding="utf-8")
    lines = readme_lines()
    for promise in (
        "permission switch",
        "per-user permissions",
        "location switch for the whole PC",
        "permissions of single apps",
        "app\npermission",
        "app permission,",
    ):
        assert promise not in text, promise
    start = next(i for i, line in enumerate(lines) if line.startswith(f"{APP_NAME} tunes"))
    intro = " ".join(lines[start : lines.index("", start)])
    assert "can be undone" in intro
    assert "permission" not in intro, "the opening list names what Cairn changes and undoes"
    accounts = next(line for line in lines if line.startswith("- **Another account.**"))
    assert "permission" not in accounts
    standard = next(line for line in lines if line.startswith("- **Without administrator rights**"))
    assert "permission" not in standard
    commands = next(line for line in lines if "`perm" in line)
    assert re.search(r"`perm list`", commands), commands
    assert "refuse" in commands, commands
