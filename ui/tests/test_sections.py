"""The section table and the window's hook registries, checked against the App class (no window
is created)."""

from __future__ import annotations

import pytest

from optimizer import app as app_module
from optimizer.app import App
from optimizer.sections import ABOUT_GLYPH, FIRST, GROUPS, SECTIONS, Section, grouped, section

TABLE = [
    ("Dashboard", "Overview", "\ue9d9"),
    ("System", "Overview", "\ue7f8"),
    ("Security", "Overview", "\uea18"),
    ("Boot history", "Overview", "\ue916"),
    ("Optimize", "Tune", "\ue9e9"),
    ("Apps", "Tune", "\uecaa"),
    ("Startup", "Tune", "\ue7e8"),
    ("Permissions", "Tune", "\ue8d7"),
    ("Network", "Tune", "\ue774"),
    ("Cleanup", "Maintain", "\uea99"),
    ("Storage", "Maintain", "\ueda2"),
    ("Updates", "Maintain", "\ue896"),
    ("Tools", "Maintain", "\ue90f"),
    ("Maintenance", "Maintain", "\ue787"),
    ("History", "Your changes", "\ue81c"),
    ("Profiles", "Your changes", "\ue7b8"),
]


def test_the_table_lists_every_section_in_order() -> None:
    assert [(s.name, s.group, s.glyph) for s in SECTIONS] == TABLE
    assert FIRST == SECTIONS[0].name == "Dashboard"
    assert GROUPS == ("Overview", "Tune", "Maintain", "Your changes")


def test_names_are_unique() -> None:
    names = [s.name for s in SECTIONS]
    assert len(set(names)) == len(names)


def test_groups_are_contiguous_and_in_order() -> None:
    seen: list[str] = []
    for s in SECTIONS:
        assert s.group in GROUPS
        if not seen or seen[-1] != s.group:
            assert s.group not in seen, f"{s.group} is split"
            seen.append(s.group)
    assert seen == list(GROUPS)
    assert [(group, [s.name for s in members]) for group, members in grouped()] == [
        (group, [s.name for s in SECTIONS if s.group == group]) for group in GROUPS
    ]
    assert grouped([SECTIONS[-1], SECTIONS[0]]) == [
        ("Overview", [SECTIONS[0]]),
        ("Your changes", [SECTIONS[-1]]),
    ]


def test_glyphs_are_single_private_use_characters() -> None:
    for glyph in [s.glyph for s in SECTIONS] + [ABOUT_GLYPH]:
        assert len(glyph) == 1 and 0xE000 <= ord(glyph) <= 0xF8FF, repr(glyph)


def test_hooks_follow_the_tab_naming() -> None:
    for s in SECTIONS:
        assert s.build.startswith("_build_") and s.build.endswith("_tab"), s.build
        assert s.shown == "" or (s.shown.startswith("_") and s.shown.endswith("_tab_shown")), s.shown
    assert [s.name for s in SECTIONS if not s.shown] == ["Dashboard", "Optimize", "Apps"]


def test_every_build_and_shown_hook_is_an_app_method() -> None:
    for s in SECTIONS:
        assert callable(getattr(App, s.build, None)), s.build
        if s.shown:
            assert callable(getattr(App, s.shown, None)), s.shown


def test_every_registered_hook_is_an_app_method() -> None:
    names = [
        *app_module.INIT_HOOKS,
        *app_module.BUSY_HOOKS,
        *app_module.AFTER_MUTATION_HOOKS,
        *app_module.TITLE_PROVIDERS,
        *app_module.STARTUP_HOOKS,
    ]
    for lane in app_module.JOB_LANES:
        names += [lane.poll, lane.poll_failed, lane.allow_close, lane.before_shutdown, lane.running_title]
    for name in names:
        assert callable(getattr(App, name, None)), name
    assert [lane.name for lane in app_module.JOB_LANES] == [
        "tools",
        "updates",
        "storage",
        "maintenance",
        "security",
    ]
    assert [lane.name for lane in app_module.JOB_LANES if lane.blocks_network] == ["tools", "updates"]


def test_unknown_section_is_a_key_error() -> None:
    assert section("History") == Section(
        "History", "Your changes", "\ue81c", "_build_history_tab", "_history_tab_shown"
    )
    with pytest.raises(KeyError):
        section("Nope")
