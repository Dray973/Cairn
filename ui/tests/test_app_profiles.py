"""Profiles section integration tests: the real window with the in-memory FakeEngine.

Nothing reaches the system or the disk: profiles, files and exports live inside the fake,
and the file dialogs are replaced by recorders. Administrator dialogs are cancelled, never
confirmed.
"""

from __future__ import annotations

import json
import os
from typing import Any

import pytest

from optimizer import system
from optimizer.app import CLOSE_JOURNALED, CLOSE_TEXTS
from optimizer.features.profiles import (
    APPS_NOTE,
    INVALID_TITLE,
    NO_ENGINE_TEXT,
    OUTDATED_TEXT,
    READ_ERROR_TITLE,
    RESTORE_POINT_NOTE,
    SAVE_ERROR_TITLE,
    STALE_TEXT,
)
from optimizer.widgets.profiles import ADMIN_NOTE, EMPTY_TEXT, FOOTER_NOTE, ProfilesPanel

from . import app_support
from .app_support import (
    ADMIN_DIALOG_TITLE,
    FIT_SIZES,
    App,
    AppFactory,
    FakeEngine,
    assert_section_fits,
    confirm_dialog,
    dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    resize_to,
    scanned,
    section_layout_problems,
    show_section,
    theme,
)
from .fake_profiles import FORMAT, profile_text, starter_text

PROFILE_PATH = r"C:\Users\Test\Documents\Desk PC.json"
OTHER_ACCOUNT = "Cairn is running as a different account than the signed-in user."
# Rows of the long plan: more than the sheet's list shows at any size the section has to fit
# (the tests check that the list overflows at each), and few enough that laying them out at
# every size stays quick on a busy machine.
LONG_PLAN_ROWS = 20
MAINTENANCE_ROW = {
    "key": "maintenance",
    "section": "maintenance",
    "title": "Scheduled maintenance",
    "status": "change",
    "detail": "Every Sunday at 12:00",
    "reason": None,
    "caution": "Runs every Sunday at 12:00 with administrator rights and permanently deletes files in: "
    "Temporary files. Undo removes the task; files already deleted stay deleted.",
    "risk": None,
    "restart": "none",
    "per_user": False,
    "selected": False,
}


def open_profiles(app: App) -> ProfilesPanel:
    """Waits for the first scan and shows the Profiles section."""
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Profiles")
    pump(app, 0.2)
    return app.profiles_panel


def settled(app: App) -> bool:
    return idle(app) and not app._profile_loading


def preview(app: App, starter_id: str = "gaming") -> ProfilesPanel:
    """Previews a starter from its card and waits for the plan."""
    panel = app.profiles_panel
    card = next(c for c in panel.starter_cards if c.starter_id == starter_id)
    card.preview_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "plan" and settled(app))
    return panel


def profile_calls(engine: FakeEngine) -> list[str]:
    return [name for name, _ in engine.calls if name.startswith("profile_")]


def plans(engine: FakeEngine) -> list[tuple[Any, ...]]:
    return [c for c in engine.calls_named("profile_apply") if c[3] is True]


def applies(engine: FakeEngine) -> list[tuple[Any, ...]]:
    return [c for c in engine.calls_named("profile_apply") if c[3] is False]


def status(app: App) -> tuple[str, str]:
    return str(app.status_message.cget("text")), str(app.status_message.cget("text_color"))


def borders(panel: ProfilesPanel) -> list[Any]:
    """The starter cards' border colours, in card order."""
    return [card.cget("border_color") for card in panel.starter_cards]


def open_export(app: App) -> None:
    """Presses "Export this PC's settings…" and waits for the export form."""
    panel = app.profiles_panel
    panel.export_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "export" and settled(app))


def record_statuses(app: App) -> list[tuple[str, str]]:
    """Records every status bar message from now on; a later rescan replaces the last one."""
    shown: list[tuple[str, str]] = []
    set_status = app.set_status

    def record(text: str, color: str = theme.INK_SECONDARY) -> None:
        shown.append((text, color))
        set_status(text, color)

    app.set_status = record  # type: ignore[method-assign]
    return shown


def cancel(app: App) -> None:
    dialog = next_dialog(app)
    pump(app, 0.2)
    dialog._cancel()
    pump(app, 0.2)


def record_dialog(monkeypatch: pytest.MonkeyPatch, name: str, result: str) -> list[dict[str, Any]]:
    """Replaces the file dialog helper `name` with a recorder that returns `result`."""
    calls: list[dict[str, Any]] = []

    def recorder(parent: Any, **kwargs: Any) -> str:
        calls.append(kwargs)
        return result

    monkeypatch.setattr(system, name, recorder)
    return calls


def apply_checked(app: App) -> None:
    """Presses Apply and confirms the apply dialog, then waits for the result."""
    app.profiles_panel.sheet.apply_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: app.profiles_panel.mode == "result" and settled(app))


def test_building_the_section_reads_only_the_starters(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_profiles(app)
    assert profile_calls(engine) == ["profile_starters"]
    assert [c.starter_id for c in panel.starter_cards] == ["gaming", "privacy", "clean"]
    assert panel.starter_cards[0].counts_label.cget("text") == "2 settings"
    assert panel.starter_cards[1].counts_label.cget("text") == "2 settings · 1 app"
    assert panel.mode == "empty"
    assert panel.sheet.message_label.cget("text") == EMPTY_TEXT
    assert not panel.last_applied_card.winfo_ismapped()
    assert app.errors == []


def test_a_starter_preview_plans_once_and_changes_nothing(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, preset=["performance.sysmain"])
    open_profiles(app)
    panel = preview(app, "gaming")
    assert plans(engine) == [(starter_text("gaming"), None, "skip", True)]
    assert applies(engine) == []
    assert engine.applied == set()
    sheet = panel.sheet
    assert sheet.title_label.cget("text") == "Gaming"
    assert sheet.source_label.cget("text") == "Starter profile"
    assert sheet.summary_label.cget("text") == "1 to change · 1 already set"
    assert [r.key for r in sheet.rows] == ["tweak:gaming.game_mode"]
    assert sheet.row("tweak:gaming.game_mode").checked
    assert sheet.apply_button.cget("text") == "Apply 1 change"
    assert sheet.apply_button.cget("state") == "normal"
    assert sheet.note_label.cget("text") == FOOTER_NOTE
    assert panel.starter_cards[0].cget("border_color") == theme.ACCENT
    assert status(app)[0] == "“Gaming”: 1 to change, 1 already set. Nothing has changed yet."
    sheet.expand_all()
    assert [r.key for r in sheet.rows] == ["tweak:gaming.game_mode", "tweak:performance.sysmain"]
    assert app.errors == []


def test_a_standard_user_can_preview_but_not_apply(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    open_profiles(app)
    panel = preview(app, "gaming")
    sheet = panel.sheet
    assert len(sheet.rows) == 2
    assert sheet.apply_button.cget("state") == "disabled"
    assert sheet.note_label.cget("text") == ADMIN_NOTE
    assert panel.open_button.cget("state") == "normal"
    assert panel.export_button.cget("state") == "normal"
    sheet.apply_button.invoke()
    pump(app, 0.2)
    assert dialogs(app) == []
    app._on_apply_profile(sheet.selected_keys())
    assert next_dialog(app).title_text == ADMIN_DIALOG_TITLE
    cancel(app)
    assert applies(engine) == []
    assert app.errors == []


def test_applying_passes_exactly_the_checked_keys(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, profile_fail=["tweak:privacy.cortana"])
    open_profiles(app)
    panel = preview(app, "privacy")
    sheet = panel.sheet
    assert sheet.selected_keys() == [
        "tweak:privacy.activity_history",
        "tweak:privacy.cortana",
        "app:Microsoft.BingNews",
    ]
    sheet.row("app:Microsoft.BingNews").checkbox.toggle()
    assert sheet.apply_button.cget("text") == "Apply 2 changes"
    scans = len(engine.calls_named("scan"))
    summaries = len(engine.calls_named("journal_summary"))
    shown = record_statuses(app)

    sheet.apply_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert dialog.title_text == "Apply 2 changes from “Privacy”?"
    assert RESTORE_POINT_NOTE in text
    assert APPS_NOTE not in text
    assert "• Activity History" in text and "• Cortana" in text
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "result" and bool(dialogs(app)) and settled(app))

    assert applies(engine) == [
        (starter_text("privacy"), ["tweak:privacy.activity_history", "tweak:privacy.cortana"], "try", False)
    ]
    assert engine.applied == {"privacy.activity_history"}
    result = next_dialog(app)
    assert result.title_text == "Some changes failed"
    assert "1 applied, 1 failed." in dialog_text(result)
    assert "• Cortana (failed): Access is denied." in dialog_text(result)
    result.confirm_button.invoke()
    pump(app, 5.0, until=lambda: scanned(app))
    assert len(engine.calls_named("scan")) > scans
    assert len(engine.calls_named("journal_summary")) > summaries
    assert ("Done: 1 applied, 1 failed.", theme.WARNING) in shown

    assert sheet.row("tweak:privacy.activity_history").status_label.cget("text") == "✓ Applied"
    assert sheet.row("tweak:privacy.cortana").status_label.cget("text") == "⚠ Failed"
    assert sheet.row("app:Microsoft.BingNews").status_label.cget("text") == "○ Not selected"
    assert sheet.undo_button.winfo_ismapped() and sheet.again_button.winfo_ismapped()
    assert not sheet.apply_button.winfo_ismapped()
    card = panel.last_applied_card
    assert card.winfo_ismapped()
    assert card.name_label.cget("text") == "Privacy"
    assert card.detail_label.cget("text").startswith("1 change  ·  ")

    # History lists the change once it is shown.
    show_section(app, "History")
    pump(app, 5.0, until=lambda: settled(app) and bool(app.history_panel.groups))
    assert any(
        "Activity History" in g.title or "activity_history" in g.title for g in app.history_panel.groups
    )
    assert app.errors == []


def test_unchecked_rows_stay_unchecked_when_the_plan_is_read_again(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_profiles(app)
    panel = preview(app, "privacy")
    panel.sheet.row("tweak:privacy.cortana").checkbox.toggle()
    before = len(plans(engine))
    app._profiles_after_mutation()
    pump(app, 5.0, until=lambda: len(plans(engine)) > before and panel.mode == "plan" and settled(app))
    assert panel.sheet.selected_keys() == ["tweak:privacy.activity_history", "app:Microsoft.BingNews"]
    assert not panel.sheet.row("tweak:privacy.cortana").checked
    assert not app._profile_stale
    assert app.errors == []


def test_a_stale_plan_is_read_again_instead_of_applied(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_profiles(app)
    panel = preview(app, "privacy")
    panel.sheet.row("tweak:privacy.cortana").checkbox.toggle()
    show_section(app, "Optimize", run_hook=False)
    app._profiles_after_mutation()
    assert app._profile_stale
    show_section(app, "Profiles", run_hook=False)
    before = len(plans(engine))
    panel.sheet.apply_button.invoke()
    pump(app, 5.0, until=lambda: len(plans(engine)) > before and panel.mode == "plan" and settled(app))
    assert dialogs(app) == []
    assert applies(engine) == []
    assert STALE_TEXT in panel.sheet.warning_label.cget("text")
    assert status(app) == (STALE_TEXT, theme.WARNING)
    assert not panel.sheet.row("tweak:privacy.cortana").checked
    # The shown hook reads a stale plan again as well.
    show_section(app, "Optimize", run_hook=False)
    app._profiles_after_mutation()
    before = len(plans(engine))
    show_section(app, "Profiles")
    pump(app, 5.0, until=lambda: len(plans(engine)) > before and panel.mode == "plan" and settled(app))
    assert app.errors == []


def test_undo_restores_what_the_profile_changed(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, revert_restart="explorer")
    open_profiles(app)
    panel = preview(app, "gaming")
    apply_checked(app)
    assert engine.applied == {"gaming.game_mode", "performance.sysmain"}
    undo_filter = app._profile_undo["filter"]
    assert [t["value_name"] for t in undo_filter["registry"]] == ["gaming.game_mode", "performance.sysmain"]
    assert panel.sheet.row("tweak:gaming.game_mode").status_label.cget("text") == "✓ Applied"
    shown = record_statuses(app)

    panel.last_applied_card.undo_button.invoke()
    dialog = next_dialog(app)
    assert engine.calls_named("revert_targets") == [(undo_filter, True)]
    assert dialog.title_text == "Undo the changes from “Gaming”?"
    text = dialog_text(dialog)
    assert "2 recorded changes will be restored" in text
    assert "File Explorer has to restart for some of the restored settings." in text
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("revert_targets")) == 2 and bool(dialogs(app)))
    assert engine.calls_named("revert_targets")[-1] == (undo_filter, False)
    assert engine.applied == set()
    restored = next_dialog(app)
    assert restored.title_text == "Changes restored"
    restored._cancel()
    # The sheet stops showing the undone result: the profile is read again.
    pump(app, 5.0, until=lambda: panel.mode == "plan" and settled(app))
    assert app._profile_undo is None
    assert not panel.last_applied_card.winfo_ismapped()
    sheet = panel.sheet
    assert not sheet.undo_button.winfo_ismapped()
    assert sheet.apply_button.winfo_ismapped()
    assert sheet.selected_keys() == ["tweak:gaming.game_mode", "tweak:performance.sysmain"]
    assert sheet.row("tweak:gaming.game_mode").status_label.cget("text") == "◐ Will change"
    assert sheet.note_label.cget("text") == FOOTER_NOTE
    assert len(plans(engine)) == 2
    assert not app._profile_stale
    # The undo's own status is not replaced by the plan's.
    restart = "File Explorer has to restart for some of the restored settings."
    assert (f"Done: 2 changes restored. {restart}", theme.GOOD) in shown
    assert not any("Nothing has changed yet" in text for text, _ in shown)
    assert app.errors == []


def test_undo_after_the_changes_are_gone_says_so(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_profiles(app)
    panel = preview(app, "gaming")
    apply_checked(app)
    engine.applied.clear()
    panel.sheet.undo_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "plan" and settled(app))
    assert app._profile_undo is None
    assert dialogs(app) == []
    assert status(app)[0] == "Nothing from “Gaming” is still recorded; it may have been undone already."
    assert not panel.last_applied_card.winfo_ismapped()
    assert not panel.sheet.undo_button.winfo_ismapped()
    assert panel.sheet.selected_keys() == ["tweak:gaming.game_mode", "tweak:performance.sysmain"]
    # The next preview reports its own status again.
    panel.sheet.select_none_button.invoke()
    app._profile_replan()
    pump(app, 5.0, until=lambda: status(app)[0].endswith("Nothing has changed yet.") and settled(app))
    assert panel.sheet.selected_keys() == []
    assert app.errors == []


def test_rows_left_out_of_an_apply_stay_unchecked_after_its_undo(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_profiles(app)
    panel = preview(app, "gaming")
    panel.sheet.row("tweak:performance.sysmain").checkbox.toggle()
    apply_checked(app)
    assert engine.applied == {"gaming.game_mode"}
    assert panel.sheet.row("tweak:performance.sysmain").status_label.cget("text") == "○ Not selected"
    panel.sheet.undo_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: app._profile_undo is None and panel.mode == "plan" and settled(app))
    assert engine.applied == set()
    assert panel.sheet.selected_keys() == ["tweak:gaming.game_mode"]
    assert not panel.sheet.row("tweak:performance.sysmain").checked
    assert app.errors == []


def test_undo_from_the_card_while_another_preview_is_shown_keeps_that_preview(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_profiles(app)
    panel = preview(app, "gaming")
    apply_checked(app)
    preview(app, "privacy")
    before = len(plans(engine))
    panel.last_applied_card.undo_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: app._profile_undo is None and len(plans(engine)) > before)
    pump(app, 5.0, until=lambda: panel.mode == "plan" and settled(app))
    assert engine.applied == set()
    assert not panel.last_applied_card.winfo_ismapped()
    # The plan on screen is read once more by the after-change hook, not a second time.
    assert len(plans(engine)) == before + 1
    assert panel.sheet.title_label.cget("text") == "Privacy"
    assert app.errors == []


def test_open_file_reads_then_plans_and_cancel_reads_nothing(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    text = profile_text(
        {
            "format": FORMAT,
            "schema": 1,
            "name": "Desk PC",
            "description": "My desk settings",
            "tweaks": ["gaming.game_mode", "privacy.new_future_id"],
            "startup": [{"id": "user_run:Discord", "name": "Discord"}, {"id": "user_run:Gone"}],
        }
    )
    app, engine = make_app(elevated=True, profile_files={PROFILE_PATH: text})
    panel = open_profiles(app)
    asked = record_dialog(monkeypatch, "ask_open_path", "")
    panel.open_button.invoke()
    pump(app, 0.3)
    assert len(asked) == 1
    assert asked[0]["title"] == "Open a Cairn profile"
    assert engine.calls_named("profile_read") == []

    record_dialog(monkeypatch, "ask_open_path", PROFILE_PATH.replace("\\", "/"))
    panel.open_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "plan" and settled(app))
    assert engine.calls_named("profile_read") == [(PROFILE_PATH,)]
    assert plans(engine)[-1][0] == text
    sheet = panel.sheet
    assert sheet.title_label.cget("text") == "Desk PC"
    assert sheet.source_label.cget("text") == "File: Desk PC.json"
    assert sheet.description_label.cget("text") == "My desk settings"
    assert sheet.selected_keys() == ["tweak:gaming.game_mode", "startup:user_run:Discord"]
    assert sheet.summary_label.cget("text") == "2 to change · 2 skipped"
    assert all(c.cget("border_color") == theme.BORDER for c in panel.starter_cards)
    assert app.errors == []


def test_an_invalid_file_shows_why_and_plans_nothing(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, engine = make_app(elevated=True, profile_files={PROFILE_PATH: "[]"})
    panel = open_profiles(app)
    record_dialog(monkeypatch, "ask_open_path", PROFILE_PATH)
    panel.open_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == INVALID_TITLE
    assert "Desk PC.json: This file is not a Cairn profile." in dialog_text(dialog)
    dialog.confirm_button.invoke()
    pump(app, 0.2)
    assert plans(engine) == []
    assert panel.mode == "empty"
    assert app.errors == []


def test_a_file_that_cannot_be_read_shows_the_read_error(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, engine = make_app(elevated=True)
    panel = open_profiles(app)
    record_dialog(monkeypatch, "ask_open_path", PROFILE_PATH)
    panel.open_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == READ_ERROR_TITLE
    assert "The file could not be read: not found" in dialog_text(dialog)
    dialog.confirm_button.invoke()
    pump(app, 0.2)
    assert plans(engine) == []
    assert app.errors == []


def test_export_lists_candidates_checks_the_name_and_saves(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, engine = make_app(elevated=False)
    engine.applied.update({"gaming.game_mode", "privacy.cortana"})
    engine._startup_disabled.add("user_run:Discord")
    panel = open_profiles(app)
    panel.export_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "export" and settled(app))
    sheet = panel.sheet
    keys = ["tweak:privacy.cortana", "tweak:gaming.game_mode", "startup:user_run:Discord"]
    assert sheet.selected_keys() == keys
    assert sheet.title_label.cget("text") == "Export this PC's settings"
    assert sheet.save_button.cget("state") == "normal"
    sheet.row("startup:user_run:Discord").checkbox.toggle()

    # An empty name is refused before any dialog.
    sheet.save_button.invoke()
    pump(app, 0.2)
    assert sheet.name_error_label.winfo_ismapped()
    assert sheet.name_error_label.cget("text") == "⚠ Give the profile a name."
    assert engine.calls_named("profile_export") == []

    saved = r"C:\Users\Test\Documents\My setup.json"
    asked = record_dialog(monkeypatch, "ask_save_path", saved.replace("\\", "/"))
    sheet.name_entry.insert(0, "  My setup ")
    sheet.description_entry.insert(0, "Desk")
    # A setting that changes after the list was read is left out of the file.
    engine.applied.discard("privacy.cortana")
    sheet.save_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("profile_export")) and settled(app))
    assert asked[0]["initialfile"] == "My setup.json"
    assert asked[0]["defaultextension"] == ".json"
    assert engine.calls_named("profile_export") == [
        (saved, "My setup", "Desk", ["tweak:privacy.cortana", "tweak:gaming.game_mode"])
    ]
    assert json.loads(engine.exported[saved])["tweaks"] == ["gaming.game_mode"]
    text, color = status(app)
    assert text == (
        "Saved “My setup” (1 setting) to My setup.json. 1 setting changed since the list was read "
        "and was left out."
    )
    assert color == theme.WARNING
    assert panel.mode == "empty"

    # A second export with nothing missing reports success.
    panel.export_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "export" and settled(app))
    panel.sheet.name_entry.insert(0, "Again")
    panel.sheet.save_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("profile_export")) == 2 and settled(app))
    assert status(app) == ("Saved “Again” (1 setting · 1 startup app) to My setup.json.", theme.GOOD)
    assert app.errors == []


def test_export_returns_to_the_previous_plan_and_reports_failures(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, engine = make_app(elevated=True, profile_candidates_error="the journal is locked")
    open_profiles(app)
    panel = preview(app, "gaming")
    panel.export_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "error" and settled(app))
    assert "This PC's settings could not be read: the journal is locked" in panel.sheet.message_label.cget(
        "text"
    )
    assert panel.sheet.retry_button.winfo_ismapped()
    engine.profile_candidates_error = None
    engine.applied.add("gaming.game_mode")
    panel.sheet.retry_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "export" and settled(app))
    panel.sheet.cancel_button.invoke()
    pump(app, 0.2)
    assert panel.mode == "plan"
    assert panel.sheet.title_label.cget("text") == "Gaming"
    assert borders(panel) == [theme.ACCENT, theme.BORDER, theme.BORDER]

    panel.export_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "export" and settled(app))
    panel.sheet.name_entry.insert(0, "Mine")
    record_dialog(monkeypatch, "ask_save_path", r"C:\Users\Test\Documents\Mine.json")
    monkeypatch.setattr(engine, "profile_export", _failing_export)
    panel.sheet.save_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == SAVE_ERROR_TITLE
    dialog.confirm_button.invoke()
    pump(app, 0.2)
    assert panel.mode == "export"
    assert app.errors == []


def _failing_export(
    path: str, name: str, description: str = "", keys: list[str] | None = None
) -> dict[str, Any]:
    raise RuntimeError(f"{os.path.basename(path)} could not be saved: Access is denied.")


def test_leaving_the_export_marks_the_starter_shown_again(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, engine = make_app(elevated=True, profile_files={PROFILE_PATH: starter_text("gaming")})
    panel = open_profiles(app)
    gaming = [theme.ACCENT, theme.BORDER, theme.BORDER]
    unmarked = [theme.BORDER] * 3
    preview(app, "gaming")
    assert borders(panel) == gaming

    # Left from an apply's result: the out-of-date plan is read again under its marked card.
    apply_checked(app)
    open_export(app)
    assert borders(panel) == unmarked, "the export form shows no starter's profile"
    before = len(plans(engine))
    panel.sheet.cancel_button.invoke()
    pump(app, 5.0, until=lambda: len(plans(engine)) > before and panel.mode == "plan" and settled(app))
    assert panel.sheet.title_label.cget("text") == "Gaming"
    assert borders(panel) == gaming

    # Left by saving the profile.
    open_export(app)
    record_dialog(monkeypatch, "ask_save_path", r"C:\Users\Test\Documents\Mine.json")
    panel.sheet.name_entry.insert(0, "Mine")
    panel.sheet.save_button.invoke()
    pump(
        app,
        5.0,
        until=lambda: bool(engine.calls_named("profile_export")) and panel.mode == "plan" and settled(app),
    )
    assert borders(panel) == gaming

    # A file's profile marks no starter, even when it holds a starter's settings.
    record_dialog(monkeypatch, "ask_open_path", PROFILE_PATH)
    panel.open_button.invoke()
    pump(
        app,
        5.0,
        until=lambda: panel.sheet.source_label.cget("text") == "File: Desk PC.json" and settled(app),
    )
    assert panel.sheet.title_label.cget("text") == "Gaming"
    assert borders(panel) == unmarked
    open_export(app)
    panel.sheet.cancel_button.invoke()
    pump(app, 0.2)
    assert panel.mode == "plan"
    assert panel.sheet.source_label.cget("text") == "File: Desk PC.json"
    assert borders(panel) == unmarked
    assert app.errors == []


def test_closing_while_applying_says_the_changes_are_journaled(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"profile_apply": 0.8})
    open_profiles(app)
    panel = preview(app, "gaming")
    panel.sheet.apply_button.invoke()
    confirm_dialog(app)
    # The confirmation starts the apply before the event loop runs again.
    assert app._busy
    app._request_close()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "Applying the profile is still running." in text
    assert CLOSE_TEXTS[CLOSE_JOURNALED] in text
    dialog._cancel()
    pump(app, 5.0, until=lambda: panel.mode == "result" and settled(app))
    assert app.errors == []


def test_an_outdated_engine_turns_the_section_off(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, unsupported=["profile_apply"])
    panel = open_profiles(app)
    assert panel.mode == "unsupported"
    assert panel.unsupported_label.cget("text") == f"⚠ {OUTDATED_TEXT}"
    assert panel.open_button.cget("state") == "disabled"
    assert panel.export_button.cget("state") == "disabled"
    assert profile_calls(engine) == []
    assert NO_ENGINE_TEXT != OUTDATED_TEXT
    assert_section_fits(app, "Profiles")


def test_rows_of_another_account_are_skipped_with_a_warning(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, other_account=OTHER_ACCOUNT)
    open_profiles(app)
    panel = preview(app, "privacy")
    sheet = panel.sheet
    assert OTHER_ACCOUNT in sheet.warning_label.cget("text")
    assert sheet.selected_keys() == ["tweak:privacy.activity_history", "tweak:privacy.cortana"]
    sheet.expand_all()
    assert sheet.row("app:Microsoft.BingNews").status_label.cget("text") == "– Belongs to another account"
    assert app.errors == []


def test_a_cautioned_row_starts_unchecked_with_its_caution(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, profile_extra_rows=[MAINTENANCE_ROW])
    open_profiles(app)
    panel = preview(app, "gaming")
    sheet = panel.sheet
    row = sheet.row("maintenance")
    assert row is not None and row.checkbox is not None
    assert not row.checked
    assert row.caution_label is not None
    assert row.caution_label.cget("text") == f"⚠ {MAINTENANCE_ROW['caution']}"
    assert sheet.selected_keys() == ["tweak:gaming.game_mode", "tweak:performance.sysmain"]
    sheet.select_all_button.invoke()
    assert "maintenance" in sheet.selected_keys()
    sheet.select_none_button.invoke()
    assert sheet.selected_keys() == []
    assert sheet.apply_button.cget("state") == "disabled"
    assert app.errors == []


def test_an_explorer_restart_is_offered_after_applying(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, profile_restarts={"gaming.game_mode": "explorer"})
    open_profiles(app)
    panel = preview(app, "gaming")
    shown = record_statuses(app)
    panel.sheet.apply_button.invoke()
    dialog = next_dialog(app)
    assert "File Explorer has to restart for some of these changes." in dialog_text(dialog)
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "result" and bool(dialogs(app)) and settled(app))
    result = next_dialog(app)
    assert result.title_text == "Profile applied"
    assert result.confirm_button.cget("text") == "Restart Explorer now"
    result.confirm_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("restart_explorer")))
    pump(app, 5.0, until=lambda: settled(app) and ("File Explorer restarted.", theme.GOOD) in shown)
    assert ("Done: 2 applied. File Explorer has to restart for some of these changes.", theme.GOOD) in shown
    assert app.errors == []


def test_preview_again_reads_the_plan_after_a_result(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_profiles(app)
    panel = preview(app, "gaming")
    apply_checked(app)
    before = len(plans(engine))
    panel.sheet.again_button.invoke()
    pump(app, 5.0, until=lambda: len(plans(engine)) > before and panel.mode == "plan" and settled(app))
    assert panel.sheet.summary_label.cget("text") == "2 already set"
    assert panel.sheet.apply_button.cget("state") == "disabled"
    assert app.errors == []


def big_profile(count: int) -> tuple[list[tuple[str, str, bool]], str]:
    ids = [f"privacy.extra_setting_{i:02d}" for i in range(count)]
    text = profile_text(
        {
            "format": FORMAT,
            "schema": 1,
            "name": "A long profile name that takes a lot of room in the header of the sheet",
            "description": "A description long enough to wrap onto a second line at the smaller window "
            "sizes the section has to fit, so its wrap length is checked as well.",
            "tweaks": ids,
        }
    )
    return [(i, "privacy", False) for i in ids], text


def big_app(make_app: AppFactory, monkeypatch: pytest.MonkeyPatch) -> tuple[App, FakeEngine, ProfilesPanel]:
    """A window whose profile file holds `LONG_PLAN_ROWS` settings, with the Open dialog
    answering its path."""
    extra, text = big_profile(LONG_PLAN_ROWS)
    app, engine = make_app(
        elevated=True,
        extra_tweaks=extra,
        profile_files={PROFILE_PATH: text},
        other_account=OTHER_ACCOUNT,
    )
    panel = open_profiles(app)
    record_dialog(monkeypatch, "ask_open_path", PROFILE_PATH)
    return app, engine, panel


def open_big_plan(app: App, panel: ProfilesPanel) -> None:
    panel.open_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "plan" and settled(app))
    pump(app, 0.3)
    assert len(panel.sheet.rows) == LONG_PLAN_ROWS


def rows_in_view(panel: ProfilesPanel) -> float:
    """The part of the sheet's list rows that is in view: below 1 while the rows are longer
    than the list, so it scrolls."""
    first, last = panel.sheet.list._parent_canvas.yview()
    return float(last) - float(first)


def record_rows_in_view(monkeypatch: pytest.MonkeyPatch, panel: ProfilesPanel) -> list[float]:
    """From now on, records `rows_in_view` each time a section's layout is measured."""
    in_view: list[float] = []
    measure = app_support.section_layout_problems

    def measured(app: App, name: str) -> list[str]:
        in_view.append(rows_in_view(panel))
        return measure(app, name)

    monkeypatch.setattr(app_support, "section_layout_problems", measured)
    return in_view


def test_the_empty_section_fits_the_window(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_profiles(app)
    assert panel.mode == "empty"
    assert_section_fits(app, "Profiles")
    for widget in (panel.open_button, panel.export_button, panel.intro_label):
        assert widget.winfo_ismapped()
    assert app.errors == []


def test_a_long_plan_fits_the_window(make_app: AppFactory, monkeypatch: pytest.MonkeyPatch) -> None:
    app, engine, panel = big_app(make_app, monkeypatch)
    open_big_plan(app, panel)
    in_view = record_rows_in_view(monkeypatch, panel)
    assert_section_fits(app, "Profiles")
    # Measured once at each size: the rows are longer than the list there, so the list scrolls
    # while the header and the buttons stay in view.
    assert len(in_view) == len(FIT_SIZES)
    assert all(part < 1 for part in in_view), in_view
    for widget in (panel.sheet.apply_button, panel.open_button, panel.export_button, panel.intro_label):
        assert widget.winfo_ismapped()
    assert app.errors == []


def test_a_plan_and_a_result_shown_at_a_narrow_width_fit(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, engine, panel = big_app(make_app, monkeypatch)
    app.minsize(900, 600)
    resize_to(app, 900, 600)
    # Shown at this width, without another resize.
    open_big_plan(app, panel)
    assert rows_in_view(panel) < 1
    assert section_layout_problems(app, "Profiles") == []
    assert panel.sheet.apply_button.winfo_ismapped()
    apply_checked(app)
    for dialog in dialogs(app):
        dialog._cancel()
    pump(app, 0.3)
    assert section_layout_problems(app, "Profiles") == []
    assert panel.sheet.undo_button.winfo_ismapped() and panel.sheet.again_button.winfo_ismapped()
    assert_section_fits(app, "Profiles")


def test_the_export_form_fits_the_window(make_app: AppFactory, monkeypatch: pytest.MonkeyPatch) -> None:
    app, engine, panel = big_app(make_app, monkeypatch)
    engine.applied.update({"gaming.game_mode", "privacy.cortana"})
    panel.export_button.invoke()
    pump(app, 5.0, until=lambda: panel.mode == "export" and settled(app))
    panel.sheet.save_button.invoke()
    pump(app, 0.2)
    assert panel.sheet.name_error_label.winfo_ismapped()
    assert_section_fits(app, "Profiles", sizes=(*FIT_SIZES, (900, 600)))
