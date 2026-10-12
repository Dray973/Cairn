"""The Optimize list's search box, Show menu and collapsible headings in the real window with a
FakeEngine: filtering after the typing delay, Esc, Ctrl+F, the headings' counts, the empty-list
messages, what survives a rescan, and the layout with a search and with each Show choice.

Nothing on this PC is read or changed: every call goes to the in-memory FakeEngine.
"""

from __future__ import annotations

import tkinter.font as tkfont
from typing import Any

import pytest

from optimizer.widgets.controls import (
    ALL_USABLE_TEXT,
    NOTHING_CHANGED_IN_CATEGORY_TEXT,
    NOTHING_CHANGED_TEXT,
    NOTHING_CHANGED_UNREAD_ONE,
    SHOW_CHOICES,
    SHOW_MENU_WIDTH,
    ItemListPanel,
)
from optimizer.widgets.search import DEBOUNCE_MS

from .app_support import (
    App,
    AppFactory,
    assert_section_fits,
    idle,
    pump,
    rows_by_id,
    scanned,
    show_section,
    theme,
)

ALL_IDS = [
    "privacy.activity_history",
    "privacy.advertising_id",
    "performance.sysmain",
    "gaming.game_mode",
    "interface.file_extensions",
]
TOO_NEW = (
    "Windows 11 24H2 and later no longer use this setting; this PC runs Windows 11 Home 26H2 "
    "(build 26300.9457)."
)
NO_RECALL = "Cairn found no sign of Recall on this PC."
# Room the Show menu keeps for its arrow and padding beside the label, in CustomTkinter units.
SHOW_MENU_CHROME = 42
FORMER = {
    "target": "HKLM\\SOFTWARE\\Policies\\Test\\Former",
    "original": "not set",
    "reason": "Cairn 0.2 also set this value, which this tweak no longer uses, so Cairn no longer sets it.",
}


def open_optimize(app: App) -> ItemListPanel:
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    pump(app, 0.2)
    return app.optimize_list


def type_query(panel: ItemListPanel, text: str) -> None:
    """Appends `text` to the search box as typing does."""
    assert panel.search_entry is not None
    panel.search_entry.entry.insert("end", text)
    panel.search_entry._text_edited()


def test_search_filters_rows_after_the_debounce(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_optimize(app)
    assert panel.visible_ids() == ALL_IDS
    assert panel.show_choice == "available" and panel.show_menu is not None
    assert panel.show_menu.get() == "Available here"

    type_query(panel, "game")
    assert panel.visible_ids() == ALL_IDS, "nothing is filtered while the delay runs"
    pump(app, 5.0, until=lambda: panel.visible_ids() == ["gaming.game_mode"])
    assert panel.query == "game"
    # Only the heading of the category with a match stays, with its count.
    assert panel.heading_shown("gaming") and not panel.heading_shown("privacy")
    assert panel.heading_text("gaming") == "▾ GAMING  ·  1"
    assert "showing 1" in panel.summary.cget("text")
    assert DEBOUNCE_MS == 200
    assert app.errors == []


def test_search_and_category_filter_combine(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_optimize(app)
    panel.set_query("privacy")
    assert panel.visible_ids() == ["privacy.activity_history", "privacy.advertising_id"], "id parts match"
    panel._filter_changed("Privacy")
    assert panel.visible_ids() == ["privacy.activity_history", "privacy.advertising_id"]
    panel._filter_changed("Gaming")
    assert panel.visible_ids() == []
    assert panel.filtered_message == 'Only tweaks that the filters hide match "privacy".'
    panel.set_query("")
    assert panel.visible_ids() == ["gaming.game_mode"], "the category filter stays"
    assert panel.filtered_message is None
    panel._filter_changed("All")
    assert panel.visible_ids() == ALL_IDS
    assert "showing" not in panel.summary.cget("text")
    assert app.errors == []


def test_escape_clears_the_search(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_optimize(app)
    type_query(panel, "sysmain")
    pump(app, 5.0, until=lambda: panel.visible_ids() == ["performance.sysmain"])
    assert panel.search_entry is not None and panel.search_entry.clear_shown
    assert panel.search_entry._escape(None) == "break"  # type: ignore[arg-type]
    assert panel.visible_ids() == ALL_IDS, "cleared at once, without the delay"
    assert panel.query == ""
    assert not panel.search_entry.clear_shown
    assert app.errors == []


def test_ctrl_f_focuses_the_search_only_in_optimize(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_optimize(app)
    assert panel.search_entry is not None
    entry = panel.search_entry.entry
    for sequence in ("<Control-f>", "<Control-F>"):
        assert app.bind(sequence), f"{sequence} is bound"
    panel.set_query("edge")
    entry.select_clear()
    assert app._on_find_key() == "break"
    assert entry.select_present(), "the search text is selected, so typing replaces it"

    entry.select_clear()
    show_section(app, "Dashboard")
    assert app._on_find_key() is None, "other sections keep the key"
    assert not entry.select_present()
    assert app.errors == []


def test_collapsed_heading_hides_rows_and_a_search_shows_matches(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_optimize(app)
    assert panel.heading_text("privacy") == "▾ PRIVACY AND TELEMETRY  ·  2"
    heading = panel._headings["privacy"]
    heading.invoke()
    assert panel.collapsed("privacy")
    assert panel.heading_text("privacy") == "▸ PRIVACY AND TELEMETRY  ·  2"
    assert panel.heading_shown("privacy"), "a collapsed heading stays"
    assert panel.visible_ids() == ALL_IDS[2:]
    assert "showing" not in panel.summary.cget("text"), "collapsing is no filter"

    # A search opens every category with matches and leaves the collapsed state alone.
    panel.set_query("advertising")
    assert panel.visible_ids() == ["privacy.advertising_id"]
    assert panel.heading_text("privacy") == "▾ PRIVACY AND TELEMETRY  ·  1 of 2"
    # A click during the search folds the category for that search only.
    panel.toggle_group("privacy")
    assert panel.visible_ids() == []
    assert panel.heading_text("privacy") == "▸ PRIVACY AND TELEMETRY  ·  1 of 2"
    assert panel.filtered_message is None, "a folded match is no empty list"
    panel.set_query("advert")
    assert panel.visible_ids() == ["privacy.advertising_id"], "a new search opens it again"

    panel.set_query("")
    assert panel.collapsed("privacy"), "clearing the search brings the collapsed state back"
    assert panel.visible_ids() == ALL_IDS[2:]
    panel.toggle_group("privacy")
    assert panel.visible_ids() == ALL_IDS
    assert app.errors == []


def test_search_and_show_survive_a_rescan(make_app: AppFactory) -> None:
    app, _ = make_app(
        elevated=True,
        extra_tweaks=[("privacy.copilot", "privacy", False)],
        unavailable={"privacy.copilot": ("windows_version", TOO_NEW)},
    )
    panel = open_optimize(app)
    assert "privacy.copilot" not in panel.visible_ids(), "not available here"
    assert "1 not available here" in panel.summary.cget("text")
    assert panel.heading_text("privacy") == "▾ PRIVACY AND TELEMETRY  ·  2 of 3"
    panel.set_show("all")
    panel.toggle_group("gaming")
    panel.set_query("privacy")
    before = panel.visible_ids()
    assert before == ["privacy.activity_history", "privacy.advertising_id", "privacy.copilot"]

    app.start_scan()
    pump(app, 5.0, until=lambda: scanned(app))
    assert panel.visible_ids() == before
    assert panel.query == "privacy" and panel.search_entry is not None
    assert panel.search_entry.entry.get() == "privacy"
    assert panel.show_choice == "all" and panel.show_menu is not None
    assert panel.show_menu.get() == "All tweaks"
    panel.set_query("")
    assert panel.collapsed("gaming"), "the collapsed headings stay"
    assert "gaming.game_mode" not in panel.visible_ids()
    assert app.errors == []


def test_no_match_message(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_optimize(app)
    panel.set_query("  zzz ")
    assert panel.visible_ids() == []
    assert panel.filtered_message == 'No tweak matches "zzz".'
    assert panel._filtered_label is not None
    assert panel._filtered_label.cget("text_color") == theme.INK_MUTED
    assert not any(panel.heading_shown(c) for c in panel.categories), "no empty headings"
    panel.set_query("")
    assert panel.filtered_message is None

    panel.set_show("changed")
    assert panel.filtered_message == NOTHING_CHANGED_TEXT
    assert NOTHING_CHANGED_TEXT == "✓ No tweak Cairn applied has been changed since."
    assert panel._filtered_label.cget("text_color") == theme.GOOD
    panel.set_show("unavailable")
    assert panel.filtered_message == ALL_USABLE_TEXT == "Every tweak can be used on this PC."
    panel.set_show("available")
    assert panel.filtered_message is None
    assert app.errors == []


def test_a_search_that_matches_only_tweaks_this_pc_cannot_use_says_so(make_app: AppFactory) -> None:
    app, _ = make_app(
        elevated=True,
        extra_tweaks=[("privacy.recall", "privacy", False), ("privacy.notepad_ai", "privacy", False)],
        unavailable={
            "privacy.recall": ("not_on_this_pc", NO_RECALL),
            "privacy.notepad_ai": ("not_on_this_pc", "Notepad 11.2503.16 or later is not installed."),
        },
        descriptions={
            "privacy.recall": "Turns off Recall snapshots and its AI features.",
            "privacy.notepad_ai": "Turns off the AI features in Notepad.",
        },
    )
    panel = open_optimize(app)
    panel.set_query("recall")
    assert panel.visible_ids() == []
    assert panel.filtered_message == (
        'No tweak available here matches "recall", but 1 tweak this PC cannot use does. Choose All '
        "tweaks instead of Available here to see it."
    )
    assert panel._filtered_label is not None
    assert panel._filtered_label.cget("text_color") == theme.INK_MUTED
    assert "1 not available here" in panel.summary.cget("text")
    panel.set_show("all")
    assert panel.visible_ids() == ["privacy.recall"]

    panel.set_show("available")
    panel.set_query("ai features")
    assert panel.visible_ids() == []
    assert panel.filtered_message == (
        'No tweak available here matches "ai features", but 2 tweaks this PC cannot use do. Choose '
        "All tweaks instead of Available here to see them."
    )
    panel.set_show("unavailable")
    assert panel.visible_ids() == ["privacy.recall", "privacy.notepad_ai"]

    # In a category, the matches of other categories count as well.
    panel.set_show("available")
    panel.set_query("recall")
    panel._filter_changed("Gaming")
    assert panel.filtered_message == 'Only tweaks that the filters hide match "recall".'
    panel._filter_changed("Privacy")
    assert panel.filtered_message is not None and panel.filtered_message.startswith(
        'No tweak available here matches "recall", but 1 tweak'
    )
    panel.set_query("game")
    assert panel.filtered_message == 'Only tweaks that the filters hide match "game".'
    assert app.errors == []


def test_a_search_whose_matches_the_show_choice_hides_says_so(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    panel = open_optimize(app)
    panel.set_query("advertising")
    assert panel.visible_ids() == ["privacy.advertising_id"]
    for value in ("changed", "unavailable"):
        panel.set_show(value)
        assert panel.visible_ids() == [], value
        assert panel.filtered_message == 'Only tweaks that the filters hide match "advertising".', value
    panel.set_query("zzz")
    assert panel.filtered_message == 'No tweak matches "zzz".'
    assert app.errors == []


def test_show_texts_speak_for_the_chosen_category(make_app: AppFactory) -> None:
    drifted = {
        "target": "HKLM\\Test\\gaming.game_mode",
        "label": "AllowAutoGameMode",
        "expected": "1",
        "found": "0",
        "original": "not set",
        "back_to_original": False,
    }
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[("privacy.copilot", "privacy", False)],
        unavailable={"privacy.copilot": ("windows_version", TOO_NEW)},
        drift={"gaming.game_mode": [drifted]},
    )
    engine.applied.add("gaming.game_mode")
    panel = open_optimize(app)
    # Gaming has a changed tweak and Privacy one this PC cannot use; Performance has neither.
    panel._filter_changed("Performance")
    panel.set_show("changed")
    assert panel.visible_ids() == []
    assert panel.filtered_message == "✓ No tweak Cairn applied in this category has been changed since."
    assert panel._filtered_label is not None
    assert panel._filtered_label.cget("text_color") == theme.GOOD
    panel.set_show("unavailable")
    assert panel.filtered_message == "Every tweak in this category can be used on this PC."
    assert panel._filtered_label.cget("text_color") == theme.INK_MUTED
    panel._filter_changed("Gaming")
    assert panel.filtered_message == "Every tweak in this category can be used on this PC."
    panel.set_show("changed")
    assert panel.visible_ids() == ["gaming.game_mode"] and panel.filtered_message is None
    assert app.errors == []


def test_changed_and_unavailable_choices_list_their_rows(make_app: AppFactory) -> None:
    drifted = {
        "target": "HKLM\\Test\\gaming.game_mode",
        "label": "AllowAutoGameMode",
        "expected": "1",
        "found": "0",
        "original": "not set",
        "back_to_original": False,
    }
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[("privacy.copilot", "privacy", False)],
        unavailable={"privacy.copilot": ("windows_version", TOO_NEW)},
        drift={"gaming.game_mode": [drifted]},
    )
    engine.applied.add("gaming.game_mode")
    panel = open_optimize(app)
    panel.set_show("changed")
    assert panel.visible_ids() == ["gaming.game_mode"]
    assert panel.heading_text("gaming") == "▾ GAMING  ·  1"
    panel.set_show("unavailable")
    assert panel.visible_ids() == ["privacy.copilot"]
    assert panel.heading_text("privacy") == "▾ PRIVACY AND TELEMETRY  ·  1 of 3"
    panel.set_show("all")
    assert len(panel.visible_ids()) == 6
    assert app.errors == []


def test_changed_lists_a_retired_tweak_whose_recorded_value_changed(make_app: AppFactory) -> None:
    # A retired tweak's row lists no drift, but the scan reads its recorded target as not held.
    changed = {
        "target": "HKLM\\Test\\privacy.cortana",
        "label": "AllowCortana",
        "expected": "0",
        "found": "1",
        "original": "not set",
        "back_to_original": False,
    }
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[("privacy.cortana", "privacy", True)],
        unavailable={"privacy.cortana": ("retired", "Cairn found no effect of this policy on Windows 11.")},
        drift={"privacy.cortana": [changed]},
    )
    engine.applied.add("privacy.cortana")
    panel = open_optimize(app)
    panel.set_show("changed")
    assert panel.visible_ids() == ["privacy.cortana"]
    assert panel.filtered_message is None
    row = rows_by_id(app)["privacy.cortana"]
    assert row.item["drift"] == [] and [a["held"] for a in row.item["actions"]] == [False]
    assert row.action_button is not None and row.action_button.cget("text") == "Undo"
    assert app.errors == []


def test_changed_since_applied_claims_nothing_for_a_tweak_the_scan_could_not_read(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True, unreadable={"gaming.game_mode"})
    engine.applied.add("gaming.game_mode")
    panel = open_optimize(app)
    panel.set_show("changed")
    assert panel.visible_ids() == []
    assert panel.filtered_message == NOTHING_CHANGED_UNREAD_ONE
    assert panel._filtered_label is not None
    assert panel._filtered_label.cget("text_color") == theme.INK_MUTED
    # Another category's text speaks only for the tweaks it read.
    panel._filter_changed("Privacy")
    assert panel.filtered_message == NOTHING_CHANGED_IN_CATEGORY_TEXT
    assert panel._filtered_label.cget("text_color") == theme.GOOD
    assert app.errors == []


def test_filtering_keeps_a_running_or_failed_scans_summary(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_optimize(app)

    def locked() -> dict[str, Any]:
        engine._record("scan")
        raise RuntimeError("database is locked")

    engine.scan = locked  # type: ignore[method-assign]
    app.start_scan()
    pump(app, 5.0, until=lambda: "Scan failed" in panel.summary.cget("text") and idle(app))
    panel.set_query("game")
    assert panel.visible_ids() == ["gaming.game_mode"], "the last scan's rows are filtered"
    assert panel.summary.cget("text") == "Scan failed: database is locked"
    assert panel.summary.cget("text_color") == theme.CRITICAL

    del engine.scan  # the class method again
    app.start_scan()
    pump(app, 5.0, until=lambda: scanned(app))
    assert "showing 1" in panel.summary.cget("text"), "a new scan's summary counts again"
    assert app.errors == []


def test_show_menu_labels_fit(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    # The menu's font in CustomTkinter units: 11 px, as the panel creates it.
    font = tkfont.Font(root=app, family=theme.FONT_FAMILY, size=-11)
    for label, _ in SHOW_CHOICES:
        assert font.measure(label) + SHOW_MENU_CHROME <= SHOW_MENU_WIDTH, label
    panel = open_optimize(app)
    assert panel.show_menu is not None and panel.show_menu.cget("width") == SHOW_MENU_WIDTH
    assert app.errors == []


# One layout check per test (each lays the section out at the three fit sizes): with a search,
# and with each Show choice.
LAYOUT_CASES = [("query", "privacy")] + [("show", value) for _, value in SHOW_CHOICES]


@pytest.mark.parametrize(("kind", "value"), LAYOUT_CASES, ids=[f"{k}-{v}" for k, v in LAYOUT_CASES])
def test_layout_fits_with_search_and_each_show_choice(make_app: AppFactory, kind: str, value: str) -> None:
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[
            ("privacy.copilot", "privacy", False),
            ("security.remote_assistance", "security", True),
        ],
        unavailable={"privacy.copilot": ("windows_version", TOO_NEW)},
        undo_limits={"gaming.game_mode": "Recordings deleted while this is applied cannot be brought back."},
        former={"privacy.advertising_id": [FORMER]},
    )
    engine.applied.add("gaming.game_mode")
    panel = open_optimize(app)
    assert panel.filter_bar is not None and len(panel.filter_bar.cget("values")) == 6, "a Security segment"
    assert rows_by_id(app)["security.remote_assistance"].item["category"] == "security"
    if kind == "query":
        panel.set_query(value)
    else:
        panel.set_show(value)
    assert_section_fits(app, "Optimize")
    pump(app, 5.0, until=lambda: idle(app))
    assert app.errors == []
