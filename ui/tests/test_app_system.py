"""System section in the real window, backed by the FakeEngine's fixed snapshot.

The section is read-only: it needs no elevation, opens no dialog and never writes the real
clipboard (the native writer is replaced in every test; see conftest.py).
"""

from __future__ import annotations

import tkinter as tk
from typing import TYPE_CHECKING

import pytest

from optimizer.features import system_info
from optimizer.widgets.sysinfo import GROUP_INDENT, LABEL_WIDTH

from .app_support import App, AppFactory, dialogs, pump, show_section, theme
from .fake_sysinfo import (
    SYSINFO_COMPUTER_NAME,
    SYSINFO_DISPLAYS_NOTE,
    SYSINFO_SECTION_IDS,
    SYSINFO_SUMMARY,
    SYSINFO_TEXT,
)

if TYPE_CHECKING:
    from optimizer.widgets.sysinfo import SectionCard

PLACEHOLDER = "Hardware and Windows details appear here."
LOADING_TEXT = "Reading system information…"
PRIVACY_NOTE = "Read-only: nothing on this PC is changed. The copied text leaves out the computer name."


def _show_system_tab(app: App) -> None:
    show_section(app, "System")


def _loaded(app: App) -> bool:
    return app.system_panel.loaded and not app.system_panel.loading


def _card(app: App, section_id: str) -> SectionCard:
    return app.system_panel.cards[section_id]


def _font_pixels(label: tk.Label) -> int:
    """Pixel size of a label's font, which Tk writes as a negative size."""
    return -int(label.tk.splitlist(label.cget("font"))[1])


def test_system_tab_loads_once_for_a_standard_user_and_copies_text(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    copied: list[str] = []
    app._set_clipboard = copied.append  # type: ignore[method-assign]
    panel = app.system_panel
    assert not panel.loaded
    assert panel.summary.cget("text") == PLACEHOLDER
    assert panel.copy_button.cget("state") == "disabled"
    pump(app, 0.5)
    assert engine.calls_named("sysinfo_snapshot") == [], "the snapshot loads only when the section is shown"

    _show_system_tab(app)
    pump(app, 5.0, until=lambda: _loaded(app))
    assert panel.section_count == len(SYSINFO_SECTION_IDS)
    assert set(panel.cards) == set(SYSINFO_SECTION_IDS)
    assert panel.summary.cget("text") == SYSINFO_SUMMARY
    meta = panel.meta.cget("text")
    assert meta.startswith("Read ") and " in 0.4 s  ·  " in meta and meta.endswith(PRIVACY_NOTE)
    assert panel.copy_button.cget("state") == "normal"
    assert panel.refresh_button.cget("state") == "normal"
    assert ("Edition", "Windows 11 Home") in _card(app, "windows").rows
    # The window shows the computer name; only the copied text leaves it out.
    assert ("Computer name", SYSINFO_COMPUTER_NAME) in _card(app, "windows").rows
    assert _card(app, "board").title_label.cget("text") == "Motherboard and firmware"
    monitor = ("Test Monitor", "2560 × 1440  ·  165 Hz  ·  DisplayPort")
    assert _card(app, "displays").rows == [monitor]

    # Left column: Windows, Processor, Memory, Graphics; right: board, Displays, Storage, Security.
    left = [c.section_id for c in panel.cards.values() if c.master is panel._columns[0]]
    right = [c.section_id for c in panel.cards.values() if c.master is panel._columns[1]]
    assert sorted(left, key=lambda s: int(_card(app, s).grid_info()["row"])) == [
        "windows",
        "processor",
        "memory",
        "graphics",
    ]
    assert sorted(right, key=lambda s: int(_card(app, s).grid_info()["row"])) == [
        "board",
        "displays",
        "storage",
        "security",
    ]

    show_section(app, "Dashboard")
    _show_system_tab(app)
    pump(app, 0.3)
    assert len(engine.calls_named("sysinfo_snapshot")) == 1, "a second visit does not read again"

    panel.copy_button.invoke()
    assert copied == [SYSINFO_TEXT]
    assert SYSINFO_COMPUTER_NAME not in copied[0]
    assert app.status_message.cget("text") == "System summary copied to the clipboard."
    assert app.status_message.cget("text_color") == theme.GOOD
    assert app._test_clipboard == [], "the Tk clipboard was not used"  # type: ignore[attr-defined]
    assert dialogs(app) == []
    assert app.errors == []


def test_system_tab_refresh_keeps_cards_until_the_new_snapshot(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, delay=0.2)
    panel = app.system_panel
    _show_system_tab(app)
    pump(app, 10.0, until=lambda: _loaded(app))
    old_cards = dict(panel.cards)
    old_meta = panel.meta.cget("text")

    panel.refresh_button.invoke()
    assert panel.loading
    assert panel.refresh_button.cget("state") == "disabled"
    assert panel.copy_button.cget("state") == "disabled"
    assert panel.meta.cget("text") == LOADING_TEXT
    # The new read has started on the worker and sleeps there; the old cards stay meanwhile.
    pump(app, 5.0, until=lambda: len(engine.calls_named("sysinfo_snapshot")) == 2)
    assert panel.loading
    assert panel.cards == old_cards
    assert all(card.winfo_exists() for card in old_cards.values())
    assert panel.summary.cget("text") == SYSINFO_SUMMARY

    pump(app, 5.0, until=lambda: not panel.loading)
    assert panel.section_count == len(SYSINFO_SECTION_IDS)
    assert all(panel.cards[s] is not old_cards[s] for s in SYSINFO_SECTION_IDS)
    assert not any(card.winfo_exists() for card in old_cards.values())
    assert panel.meta.cget("text") == old_meta
    assert panel.refresh_button.cget("state") == "normal"
    assert panel.copy_button.cget("state") == "normal"
    assert app.errors == []


def test_system_tab_shows_a_failed_section(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False, sysinfo_errors={"graphics": "DXGI is not available"})
    _show_system_tab(app)
    pump(app, 5.0, until=lambda: _loaded(app))
    graphics = _card(app, "graphics")
    assert graphics.error_label is not None
    assert graphics.error_label.cget("text") == "⚠ Could not read: DXGI is not available"
    assert graphics.error_label.cget("fg") == theme.WARNING
    assert graphics.rows == []
    for section in SYSINFO_SECTION_IDS:
        if section != "graphics":
            assert _card(app, section).error_label is None
            assert _card(app, section).rows, f"{section} did not render"
    assert app.system_panel.copy_button.cget("state") == "normal"
    assert app.errors == []


def test_system_tab_shows_the_displays_note(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False, sysinfo_displays_unavailable=True)
    _show_system_tab(app)
    pump(app, 5.0, until=lambda: _loaded(app))
    displays = _card(app, "displays")
    assert displays.note_label is not None
    assert displays.note_label.cget("text") == SYSINFO_DISPLAYS_NOTE
    assert displays.error_label is None, "an unavailable session is a note, not an error"
    assert displays.rows == []
    assert all(_card(app, s).note_label is None for s in SYSINFO_SECTION_IDS if s != "displays")
    assert app.errors == []


def test_system_tab_reports_an_engine_failure_without_a_dialog(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False, sysinfo_failure="access denied")
    panel = app.system_panel
    _show_system_tab(app)
    pump(app, 5.0, until=lambda: not panel.loading and bool(engine.calls_named("sysinfo_snapshot")))
    assert panel.summary.cget("text") == "Could not read system information: access denied"
    assert panel.summary.cget("text_color") == theme.CRITICAL
    assert panel.section_count == 0
    assert panel.copy_button.cget("state") == "disabled", "there is no text to copy"
    assert panel.refresh_button.cget("state") == "normal"
    pump(app, 0.3)
    assert dialogs(app) == []

    # Refresh succeeds; a later failure keeps the cards and the text of that snapshot.
    engine.sysinfo_failure = None
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: _loaded(app))
    cards = dict(panel.cards)
    engine.sysinfo_failure = "the read timed out"
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: not panel.loading)
    assert panel.summary.cget("text") == "Could not read system information: the read timed out"
    assert panel.cards == cards
    assert panel.text == SYSINFO_TEXT
    assert panel.copy_button.cget("state") == "normal"
    assert panel.meta.cget("text").endswith(PRIVACY_NOTE)
    pump(app, 0.3)
    assert dialogs(app) == []
    assert app.errors == []


def test_system_tab_warning_rows_carry_icon_and_meter(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False)
    _show_system_tab(app)
    pump(app, 5.0, until=lambda: _loaded(app))

    storage = _card(app, "storage")
    index = [label for label, _ in storage.rows].index("C: Windows")
    assert storage.rows[index] == ("C: Windows", "⚠ 38.1 GB free of 952 GB  ·  NTFS")
    assert storage.value_labels[index].cget("fg") == theme.WARNING
    ((label, meter),) = storage.meters
    assert label == "C: Windows"
    assert meter.itemcget(meter._fill, "fill") == theme.WARNING
    pump(app, 0.3)
    assert meter.winfo_width() > 10
    fill = meter.coords(meter._fill)
    assert fill[2] == pytest.approx(meter.winfo_width() * 0.96, abs=1.0)

    memory = _card(app, "memory")
    ((label, meter),) = memory.meters
    assert label == "In use"
    assert meter.itemcget(meter._fill, "fill") == theme.ACCENT

    security = dict(_card(app, "security").rows)
    assert security["Secure Boot"] == "✓ On"
    assert security["Memory integrity"] == "⚠ Turned on, not running"
    assert security["Virtualization"] == "Enabled in firmware"
    notes = [w.cget("text") for w in _card(app, "security").winfo_children() if isinstance(w, tk.Label)]
    assert notes.count("It starts after the next restart.") == 2
    assert app.errors == []


def test_system_panel_tolerates_malformed_snapshots(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    panel = app.system_panel
    # The section is visible, so the cards are laid out; nothing is read from the engine.
    show_section(app, "System", run_hook=False)
    panel.show({"summary": "no sections"})
    assert panel.summary.cget("text") == "Could not read system information: the engine returned no sections"
    assert not panel.loaded and panel.section_count == 0

    # Rows and groups of the wrong type are skipped. Both cards of a repeated id are shown;
    # `cards` keeps the later one, and the next snapshot removes both.
    panel.show(
        {
            "summary": "odd",
            "text": "odd\n",
            "sections": [
                {"id": "extra", "title": "Extra", "rows": [{"label": "A", "value": None}, "junk"]},
                {"id": "extra", "title": "Extra again", "rows": [], "groups": ["junk", {"rows": None}]},
                "junk",
                {"id": "untyped", "title": "Untyped", "rows": 5, "groups": {"title": "not a list"}},
                {
                    "id": "shares",
                    "title": "Shares",
                    "rows": [
                        {"label": "Not a number", "value": "a", "fraction": float("nan")},
                        {"label": "Text", "value": "b", "fraction": "0.5"},
                        {"label": "Flag", "value": "c", "fraction": True},
                        {"label": "Above one", "value": "d", "fraction": 7, "level": 3},
                    ],
                },
            ],
        }
    )
    pump(app, 0.2)
    assert panel.loaded and panel.section_count == 3
    assert _card(app, "extra").title_label.cget("text") == "Extra again"
    assert _card(app, "extra").rows == []
    assert _card(app, "untyped").rows == []
    shares = _card(app, "shares")
    assert shares.rows == [("Not a number", "a"), ("Text", "b"), ("Flag", "c"), ("Above one", "d")]
    # Only a finite number is a share; one above 1 fills the meter.
    ((label, meter),) = shares.meters
    assert label == "Above one"
    assert meter.winfo_width() > 10
    assert meter.coords(meter._fill)[2] == pytest.approx(meter.winfo_width(), abs=1.0)
    assert sum(len(column.winfo_children()) for column in panel._columns) == 4
    panel.show({"sections": []})
    assert sum(len(column.winfo_children()) for column in panel._columns) == 0
    assert panel.copy_button.cget("state") == "disabled", "a snapshot without text has nothing to copy"
    assert engine.calls_named("sysinfo_snapshot") == []
    assert app.errors == []


def _label_lines(card: SectionCard, index: int) -> list[str]:
    return str(card.row_labels[index].cget("text")).split("\n")


def _line_widths(card: SectionCard, index: int) -> list[int]:
    """Width in pixels of each line of a row label, in the label's own font."""
    label = card.row_labels[index]
    return [
        int(label.tk.call("font", "measure", label.cget("font"), line)) for line in _label_lines(card, index)
    ]


def test_long_row_labels_break_at_a_separator_not_mid_word(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    panel = app.system_panel
    show_section(app, "System", run_hook=False)
    long_slot = "Controller0-ChannelA-DIMM1"
    panel.show(
        {
            "summary": "labels",
            "text": "labels\n",
            "sections": [
                {
                    "id": "memory",
                    "title": "Memory",
                    "rows": [
                        {"label": "Installed", "value": "32 GB"},
                        {"label": long_slot, "value": "16 GB DDR5 DIMM"},
                        {"label": "ChannelB DIMM1", "value": "16 GB DDR5 DIMM"},
                    ],
                    "groups": [{"title": "Group", "rows": [{"label": long_slot, "value": "grouped"}]}],
                }
            ],
        }
    )
    pump(app, 0.2)
    card = _card(app, "memory")
    # The rows keep the labels as the engine sent them.
    assert [label for label, _ in card.rows] == ["Installed", long_slot, "ChannelB DIMM1", long_slot]
    assert _label_lines(card, 0) == ["Installed"]
    assert _label_lines(card, 2) == ["ChannelB DIMM1"]
    for index, indent in ((1, 0), (3, GROUP_INDENT)):
        lines = _label_lines(card, index)
        wrap = int(card.row_labels[index].cget("wraplength"))
        assert wrap == round((LABEL_WIDTH - indent) * card._scale)
        # Broken after a hyphen, each line narrow enough that Tk splits no word itself.
        assert len(lines) > 1 and "".join(lines) == long_slot, lines
        assert all(line.endswith("-") for line in lines[:-1]), lines
        assert all(width <= wrap for width in _line_widths(card, index)), (lines, wrap)
        assert lines[-1].endswith("DIMM1")
    assert engine.calls_named("sysinfo_snapshot") == []
    assert app.errors == []


def test_section_card_lays_out_again_when_the_scaling_changes(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False)
    _show_system_tab(app)
    pump(app, 5.0, until=lambda: _loaded(app))
    card = _card(app, "storage")
    scale = card._scale
    assert _font_pixels(card.title_label) == round(13 * scale)
    assert int(card.value_labels[0].cget("wraplength")) == round(300 * scale)
    rows = list(card.rows)
    before = list(card._content)
    children = len(card.winfo_children())

    card._set_scaling(scale * 2, scale * 2)
    pump(app, 0.3)
    assert not any(widget.winfo_exists() for widget in before)
    assert len(card.winfo_children()) == children
    assert card.rows == rows
    assert app.system_panel.cards["storage"] is card
    assert _font_pixels(card.title_label) == round(13 * scale * 2)
    assert _font_pixels(card.value_labels[0]) == round(11 * scale * 2)
    assert int(card.value_labels[0].cget("wraplength")) == round(300 * scale * 2)
    ((label, meter),) = card.meters
    assert label == "C: Windows"
    assert meter.itemcget(meter._fill, "fill") == theme.WARNING
    assert int(meter.cget("height")) == round(6 * scale * 2)
    assert meter.coords(meter._fill)[2] == pytest.approx(meter.winfo_width() * 0.96, abs=1.0)

    # The scaling it already has leaves the widgets in place.
    current = list(card._content)
    card._set_scaling(scale * 2, scale * 2)
    assert card._content == current and all(widget.winfo_exists() for widget in current)

    card._set_scaling(scale, scale)
    pump(app, 0.2)
    assert _font_pixels(card.title_label) == round(13 * scale)
    assert card.rows == rows
    assert app.errors == []


def test_outdated_engine_disables_system_tab(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, unsupported=["sysinfo_snapshot"])
    panel = app.system_panel
    assert panel.refresh_button.cget("state") == "disabled"
    assert panel.copy_button.cget("state") == "disabled"
    assert "The engine is out of date: rebuild and deploy it to use this section." in panel.summary.cget(
        "text"
    )
    assert panel.summary.cget("text").startswith("⚠ ")
    _show_system_tab(app)
    pump(app, 0.5)
    assert not panel.loading and not panel.loaded
    assert panel.section_count == 0
    app.load_system_info()
    assert not panel.loading
    assert engine.calls_named("sysinfo_snapshot") == []
    assert app.errors == []


def test_set_clipboard_falls_back_to_tk_when_the_native_write_fails(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, _ = make_app(elevated=False)
    attempts: list[tuple[int, str]] = []

    def native(hwnd: int, text: str) -> bool:
        attempts.append((hwnd, text))
        return False

    monkeypatch.setattr(system_info, "write_clipboard_text", native)
    app._set_clipboard("first")
    assert attempts == [(app.winfo_id(), "first")]
    assert app._test_clipboard == ["first"]  # type: ignore[attr-defined]

    # The copy button goes the same way and still reports success.
    _show_system_tab(app)
    pump(app, 5.0, until=lambda: _loaded(app))
    app.system_panel.copy_button.invoke()
    assert attempts[-1] == (app.winfo_id(), SYSINFO_TEXT)
    assert app._test_clipboard == [SYSINFO_TEXT]  # type: ignore[attr-defined]
    assert app.status_message.cget("text") == "System summary copied to the clipboard."

    # A native write that succeeds leaves the Tk clipboard alone.
    monkeypatch.setattr(system_info, "write_clipboard_text", lambda hwnd, text: True)
    app._set_clipboard("second")
    assert app._test_clipboard == [SYSINFO_TEXT]  # type: ignore[attr-defined]
    assert app.errors == []
