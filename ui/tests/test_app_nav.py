"""Sidebar navigation of the real window: the start state, that no section is a stub, showing a
section and its load hook, stepping through the sections, the wide and rail modes, the densities,
the top bar and the fit of the sections no feature's own test file checks.

The window uses the FakeEngine; administrator dialogs are never confirmed.
"""

from __future__ import annotations

from typing import Any

import pytest

from optimizer import APP_NAME, __version__
from optimizer.features import PLACEHOLDER_TEXT
from optimizer.widgets.sidebar import RAIL_WIDTH, WIDE_WIDTH

from .app_support import (
    SECTIONS,
    App,
    AppFactory,
    assert_section_fits,
    ctk,
    idle,
    pump,
    resize_to,
    scanned,
    show_section,
    theme,
)

NAMES = [s.name for s in SECTIONS]
# Sections whose fit no feature's own test file checks.
WINDOW_SECTIONS = (
    "Dashboard",
    "System",
    "Optimize",
    "Apps",
    "Startup",
    "Network",
    "Cleanup",
    "Tools",
    "History",
)
# Engine reads that belong to a section's shown hook.
SECTION_READS = (
    "startup_list",
    "journal_export_json",
    "network_list",
    "sysinfo_snapshot",
    "cleanup_scan",
    "tools_catalog",
)


def mapped_sections(app: App) -> list[str]:
    return [name for name in NAMES if app.section_frame(name).winfo_manager()]


def labels(widget: Any) -> list[str]:
    """Texts of every label under `widget`."""
    found = []
    for child in widget.winfo_children():
        if isinstance(child, ctk.CTkLabel):
            found.append(str(child.cget("text")))
        found += labels(child)
    return found


def top_bar_problems(app: App) -> list[str]:
    """Shown labels and buttons of the top bar that stick out of it or get less width than their
    text needs (1 px tolerance)."""
    bar = app.top_bar
    left, right = bar.winfo_rootx(), bar.winfo_rootx() + bar.winfo_width()
    problems = []
    for widget in bar.winfo_children():
        if not isinstance(widget, (ctk.CTkLabel, ctk.CTkButton)) or not widget.winfo_ismapped():
            continue
        text = str(widget.cget("text"))
        x0, x1 = widget.winfo_rootx(), widget.winfo_rootx() + widget.winfo_width()
        if x0 < left - 1 or x1 > right + 1:
            problems.append(f"{text!r} spans {x0 - left}..{x1 - left} of {right - left}")
        inner = widget._label if isinstance(widget, ctk.CTkLabel) else widget
        if inner.winfo_reqwidth() > widget.winfo_width() + 1:
            problems.append(f"{text!r} needs {inner.winfo_reqwidth()} px and gets {widget.winfo_width()}")
    return problems


def test_the_window_starts_on_dashboard_and_loads_no_section(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    assert list(app.nav.items) == NAMES
    assert app.current_section == "Dashboard"
    assert app.section_visible("Dashboard") and not app.section_visible("History")
    assert mapped_sections(app) == ["Dashboard"]
    assert app.section_title.cget("text") == "Dashboard"
    assert [name for name, item in app.nav.items.items() if item.selected] == ["Dashboard"]
    for read in SECTION_READS:
        assert engine.calls_named(read) == [], f"{read} before any section was shown"

    assert app.nav.about_item.name == f"About {APP_NAME}"
    # The FakeEngine reports its own version, which differs from the app's.
    assert app.nav.about_item.badge == f"⚠ v{__version__}"
    assert app.nav.about_item.badge_label.cget("text_color") == theme.WARNING
    assert app.errors == []


def test_sections_are_implemented(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    # No builder is a stub, and no section fell back to the placeholder of a missing builder.
    assert app._stub_sections == set()
    for name in NAMES:
        assert PLACEHOLDER_TEXT not in labels(app.section_frame(name)), name
    assert app.errors == []


def test_clicking_a_row_shows_its_section_and_loads_it_once(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    app.nav.items["Startup"].invoke()
    assert app.current_section == "Startup"
    assert mapped_sections(app) == ["Startup"]
    assert app.section_frame("Dashboard").winfo_manager() == ""
    assert app.section_title.cget("text") == "Startup"
    assert [name for name, item in app.nav.items.items() if item.selected] == ["Startup"]
    pump(app, 5.0, until=lambda: app.startup_panel.loaded and idle(app))
    assert len(engine.calls_named("startup_list")) == 1

    app.nav.items["Startup"].invoke()
    pump(app, 0.3)
    assert len(engine.calls_named("startup_list")) == 1, "a loaded list is not read again"
    assert app.errors == []


def test_history_reloads_on_every_visit(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "History")
    pump(app, 5.0, until=lambda: len(engine.calls_named("journal_export_json")) == 1 and idle(app))
    show_section(app, "Dashboard")
    show_section(app, "History")
    pump(app, 5.0, until=lambda: len(engine.calls_named("journal_export_json")) == 2 and idle(app))
    # A click on the shown section refreshes it too.
    app.nav.items["History"].invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("journal_export_json")) == 3 and idle(app))
    assert mapped_sections(app) == ["History"]
    assert app.errors == []


def test_a_section_can_be_shown_without_its_hook(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "System", run_hook=False)
    pump(app, 0.3)
    assert app.current_section == "System"
    assert mapped_sections(app) == ["System"]
    assert engine.calls_named("sysinfo_snapshot") == []

    with pytest.raises(KeyError):
        app.show_section("Nope")
    with pytest.raises(KeyError):
        app.section_frame("Nope")
    assert app.current_section == "System"
    assert app.errors == []


def test_stepping_wraps_around_the_sections(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "History")
    app._step_section(1)
    assert app.current_section == "Profiles"
    app._step_section(1)
    assert app.current_section == "Dashboard"
    app._step_section(-1)
    assert app.current_section == "Profiles"
    assert app._on_step_key(-1) == "break"
    assert app.current_section == "History"
    for sequence in ("<Control-Tab>", "<Control-Next>", "<Control-Shift-Tab>", "<Control-Prior>"):
        assert app.bind(sequence), sequence
    pump(app, 5.0, until=lambda: idle(app))
    assert app.errors == []


def test_the_sidebar_becomes_a_rail_below_1080_px(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)
    scale = ctk.ScalingTracker.get_window_scaling(app) or 1.0

    resize_to(app, 1000, 700)
    assert app.nav.mode == "rail"
    assert app.nav.winfo_width() == round(RAIL_WIDTH * scale)
    for item in [*app.nav.items.values(), app.nav.about_item]:
        assert item.name_label.winfo_manager() == "", item.name
        assert item.glyph_label.winfo_manager() == "grid", item.name
    assert app.nav.about_item.badge_label.winfo_manager() == ""
    assert app.brand.winfo_manager() == "grid"
    assert not app.brand.wordmark

    resize_to(app, 1080, 700)
    assert app.nav.mode == "wide"
    resize_to(app, 1440, 900)
    assert app.nav.mode == "wide"
    assert app.nav.winfo_width() == round(WIDE_WIDTH * scale)
    for item in [*app.nav.items.values(), app.nav.about_item]:
        assert item.name_label.winfo_manager() == "grid", item.name
    assert app.brand.winfo_manager() == "grid"
    assert app.errors == []


@pytest.mark.parametrize(
    ("width", "height", "density"),
    [(1440, 900, "regular"), (1120, 700, "compact"), (1240, 630, "dense"), (1120, 600, "dense")],
)
def test_the_rows_fit_above_the_about_row(
    make_app: AppFactory, width: int, height: int, density: str
) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)
    resize_to(app, width, height)
    nav = app.nav
    assert nav.density.name == density
    assert nav.footer_shown
    about_top = nav.about_item.winfo_rooty()
    left, right = nav.winfo_rootx(), nav.winfo_rootx() + nav.winfo_width()
    for name, item in nav.items.items():
        assert item.winfo_ismapped(), name
        assert item.winfo_rooty() + item.winfo_height() <= about_top, name
        assert left <= item.winfo_rootx() and item.winfo_rootx() + item.winfo_width() <= right, name
    assert app.errors == []


@pytest.mark.parametrize("elevated", [True, False])
def test_the_top_bar_is_not_clipped(make_app: AppFactory, elevated: bool) -> None:
    app, _ = make_app(elevated=elevated)
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)
    for width in (1120, 1000):
        resize_to(app, width, 700)
        for name in ("Dashboard", "Boot history", "Maintenance"):
            show_section(app, name, run_hook=False)
            pump(app, 0.2)
            assert top_bar_problems(app) == [], f"{name} at {width} px"
    assert app.errors == []


@pytest.mark.parametrize("name", WINDOW_SECTIONS)
def test_the_window_sections_fit(make_app: AppFactory, name: str) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, name)
    pump(app, 5.0, until=lambda: idle(app))
    assert_section_fits(app, name)
    assert app.errors == []
