"""The window's sections: order, sidebar group, glyph, and the App hooks that build and refresh them."""

from __future__ import annotations

from collections.abc import Sequence
from typing import NamedTuple


class Section(NamedTuple):
    name: str  # shown in the sidebar and the top bar; also the section's id
    group: str  # sidebar group heading (one of GROUPS)
    glyph: str  # Segoe Fluent Icons code point
    build: str  # App method called once with the section's frame
    shown: str = ""  # App method run each time the section is shown with an engine; "" for none


GROUPS = ("Overview", "Tune", "Maintain", "Your changes")
SECTIONS: tuple[Section, ...] = (
    Section("Dashboard", "Overview", "\ue9d9", "_build_dashboard_tab"),
    Section("System", "Overview", "\ue7f8", "_build_system_info_tab", "_system_info_tab_shown"),
    Section("Security", "Overview", "\uea18", "_build_security_tab", "_security_tab_shown"),
    Section("Boot history", "Overview", "\ue916", "_build_boot_tab", "_boot_tab_shown"),
    Section("Optimize", "Tune", "\ue9e9", "_build_optimize_tab"),
    Section("Apps", "Tune", "\uecaa", "_build_apps_tab"),
    Section("Startup", "Tune", "\ue7e8", "_build_startup_tab", "_startup_tab_shown"),
    Section("Permissions", "Tune", "\ue8d7", "_build_permissions_tab", "_permissions_tab_shown"),
    Section("Network", "Tune", "\ue774", "_build_network_tab", "_network_tab_shown"),
    Section("Cleanup", "Maintain", "\uea99", "_build_cleanup_tab", "_cleanup_tab_shown"),
    Section("Storage", "Maintain", "\ueda2", "_build_storage_tab", "_storage_tab_shown"),
    Section("Updates", "Maintain", "\ue896", "_build_updates_tab", "_updates_tab_shown"),
    Section("Tools", "Maintain", "\ue90f", "_build_tools_tab", "_tools_tab_shown"),
    Section("Maintenance", "Maintain", "\ue787", "_build_maintenance_tab", "_maintenance_tab_shown"),
    Section("History", "Your changes", "\ue81c", "_build_history_tab", "_history_tab_shown"),
    Section("Profiles", "Your changes", "\ue7b8", "_build_profiles_tab", "_profiles_tab_shown"),
)
FIRST = "Dashboard"
ABOUT_GLYPH = "\ue946"

_BY_NAME = {s.name: s for s in SECTIONS}


def section(name: str) -> Section:
    """The row of section `name`; KeyError for an unknown name."""
    return _BY_NAME[name]


def grouped(sections: Sequence[Section] = SECTIONS) -> list[tuple[str, list[Section]]]:
    """`sections` per group, in GROUPS order; groups without a section are left out."""
    result: list[tuple[str, list[Section]]] = []
    for group in GROUPS:
        members = [s for s in sections if s.group == group]
        if members:
            result.append((group, members))
    return result
