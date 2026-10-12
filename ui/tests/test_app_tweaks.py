"""Optimize list in the real window with a FakeEngine: tweaks this PC cannot use (a missing
requirement, a Windows version or edition, a retired tweak) are shown with their reason, offer
only Undo when they have records and count toward no mode; values an earlier version of Cairn
set, undo limits, apply results whose values were not kept, the settled check five seconds
later, values that changed since they were applied, and the Edge policy page line.

Nothing on this PC is read or changed: every call goes to the in-memory FakeEngine.
"""

from __future__ import annotations

import logging
import threading
from typing import Any

import pytest

from optimizer import app as app_module
from optimizer.app import (
    EDGE_POLICY_TEXT,
    NOT_KEPT_TEXT,
    SETTLED_CHANGED_TEXT,
    SETTLED_KEPT_TEXT,
)
from optimizer.widgets.controls import UNAVAILABLE_LABELS, former_caveat

from .app_support import (
    App,
    AppFactory,
    ScanRow,
    assert_section_fits,
    confirm_dialog,
    ctk,
    dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    rows_by_id,
    scanned,
    show_section,
    theme,
)
from .fake_engine import RECORDED_SUFFIX

OFFICE_MISSING = "Microsoft Office (2016 or later) is not installed."
EDGE_MISSING = "Microsoft Edge is not installed."
TOO_NEW = (
    "Windows 11 24H2 and later no longer use this setting; this PC runs Windows 11 Home 26H2 "
    "(build 26300.9457)."
)
NO_RECALL = "Cairn found no sign of Recall on this PC."
NEEDS_PRO = "Needs Windows 11 Pro, Enterprise or Education; this PC runs Windows 11 Home."
RETIRED = "Cairn found no effect of this policy on Windows 11."
LIMIT = "Windows deletes the snapshots Recall saved; Undo cannot bring them back."
FORMER = {
    "target": "HKLM\\SOFTWARE\\Policies\\Test\\Former",
    "original": "not set",
    "reason": "Cairn 0.2 also set this value, which this tweak no longer uses, so Cairn no longer sets it.",
}
GAME_MODE_RESET = {
    "target": "HKLM\\Test\\gaming.game_mode",
    "label": "AllowAutoGameMode",
    "expected": "1",
    "found": "not set",
    "original": "not set",
    "back_to_original": True,
}
# A delay of the settled check that no test reaches, for tests about other things.
LATER_THAN_ANY_TEST_MS = 600_000


def labels(row: ScanRow) -> list[str]:
    return [str(w.cget("text")) for w in row.winfo_children() if isinstance(w, ctk.CTkLabel)]


def tags_of(row: ScanRow) -> str:
    """The row's tag line ("Not on this PC  ·  recommended  ·  low risk")."""
    return next(text for text in labels(row) if "risk" in text)


def record_statuses(app: App) -> list[tuple[str, str]]:
    """Records every status bar message, with its colour, from now on."""
    shown: list[tuple[str, str]] = []
    set_status = app.set_status

    def record(text: str, color: str = theme.INK_SECONDARY) -> None:
        shown.append((text, color))
        set_status(text, color)

    app.set_status = record  # type: ignore[method-assign]
    return shown


def dialog_titled(app: App, title: str) -> Any:
    pump(app, 5.0, until=lambda: any(d.title_text == title for d in dialogs(app)))
    return next(d for d in dialogs(app) if d.title_text == title)


def close_dialogs(app: App) -> None:
    pump(app, 0.3)
    for dialog in dialogs(app):
        dialog._cancel()
    pump(app, 0.2)


def test_unavailable_item_note_is_muted(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, missing={"privacy.advertising_id": OFFICE_MISSING})
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    row = rows_by_id(app)["privacy.advertising_id"]
    assert row.item["state"] == "unavailable"
    assert row.note_label is not None
    assert row.note_label.cget("text") == f"– {OFFICE_MISSING}"
    assert row.note_label.cget("text_color") == theme.INK_MUTED
    assert row.warning_labels == []
    assert row.action_button is None, "a tweak that is not on this PC offers no Apply"
    assert row.tag_label is None
    assert tags_of(row).startswith("Not on this PC")
    assert "–" in labels(row), "the state glyph is the not-on-this-PC dash"

    other = rows_by_id(app)["gaming.game_mode"]
    assert other.note_label is None
    assert other.action_button is not None and other.action_button.cget("text") == "Apply"
    assert engine.calls_named("apply") == []
    assert app.errors == []


def test_a_read_error_keeps_its_warning(make_app: AppFactory) -> None:
    app, _ = make_app(
        elevated=True,
        unreadable={"privacy.activity_history"},
        notes={"privacy.activity_history": "Has no effect on this Windows build."},
    )
    pump(app, 5.0, until=lambda: scanned(app))
    row = rows_by_id(app)["privacy.activity_history"]
    assert row.note_label is not None
    assert row.note_label.cget("text") == "⚠ Has no effect on this Windows build."
    assert row.note_label.cget("text_color") == theme.WARNING
    assert [w.cget("text") for w in row.warning_labels] == ["⚠ cannot read registry value: access denied"]
    assert tags_of(row).startswith("Could not read")
    assert app.errors == []


def test_missing_tweaks_count_toward_no_mode(make_app: AppFactory) -> None:
    # Every recommended Privacy tweak but one is applied; the other one is not on this PC.
    app, engine = make_app(elevated=True, missing={"privacy.advertising_id": OFFICE_MISSING})
    engine.applied.add("privacy.activity_history")
    pump(app, 5.0, until=lambda: scanned(app))
    assert app.toggles["privacy"].switch.get(), "the mode is complete without the missing tweak"

    # Turning the mode on plans only the tweaks that are on this PC.
    engine.applied.clear()
    app.start_scan()
    pump(app, 5.0, until=lambda: scanned(app))
    assert not app.toggles["privacy"].switch.get()
    app.toggles["privacy"].switch.toggle()
    dialog = next_dialog(app)
    assert engine.calls_named("apply_category")[-1] == ("privacy", "skip", True)
    text = dialog_text(dialog)
    assert "privacy.activity_history" in text
    assert "privacy.advertising_id" not in text
    pump(app, 0.3)
    dialog._cancel()
    pump(app, 5.0, until=lambda: idle(app) and not dialogs(app))
    assert all(dry_run for _, _, dry_run in engine.calls_named("apply_category")), "only the plan ran"
    assert engine.applied == set()
    assert app.errors == []


def test_every_missing_reason_is_shown_on_its_row(make_app: AppFactory) -> None:
    app, _ = make_app(
        elevated=False,
        extra_tweaks=[
            ("privacy.edge_telemetry", "privacy", True),
            ("privacy.office_telemetry", "privacy", True),
        ],
        missing={"privacy.edge_telemetry": EDGE_MISSING, "privacy.office_telemetry": OFFICE_MISSING},
    )
    pump(app, 5.0, until=lambda: scanned(app))
    rows = rows_by_id(app)
    for item_id, reason in (
        ("privacy.edge_telemetry", EDGE_MISSING),
        ("privacy.office_telemetry", OFFICE_MISSING),
    ):
        note = rows[item_id].note_label
        assert note is not None and note.cget("text") == f"– {reason}", item_id
        assert rows[item_id].action_button is None
    assert app.errors == []


def test_windows_version_items_show_their_label_and_reason(make_app: AppFactory) -> None:
    reasons = {
        "privacy.copilot": ("windows_version", TOO_NEW),
        "privacy.recall": ("not_on_this_pc", NO_RECALL),
        "privacy.text_image_generation": ("edition", NEEDS_PRO),
    }
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[
            ("privacy.copilot", "privacy", True),
            ("privacy.recall", "privacy", False),
            ("privacy.text_image_generation", "privacy", False),
        ],
        unavailable=reasons,
    )
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    panel = app.optimize_list
    rows = rows_by_id(app)
    for item_id, (kind, text) in reasons.items():
        row = rows[item_id]
        assert tags_of(row).startswith(UNAVAILABLE_LABELS[kind]), item_id
        assert "recommended" not in tags_of(row), "a tweak this PC cannot use is not offered"
        assert "–" in labels(row)
        assert row.note_label is not None and row.note_label.cget("text") == f"– {text}"
        assert row.note_label.cget("text_color") == theme.INK_MUTED
        assert row.action_button is None and row.tag_label is None
        assert item_id not in panel.visible_ids(), "Available here hides it"
    panel.set_show("unavailable")
    assert panel.visible_ids() == list(reasons)
    # A recommended tweak this PC cannot use leaves its mode.
    assert app._category_status("privacy")["recommended"] == 2  # type: ignore[index]
    assert engine.calls_named("apply") == []
    assert app.errors == []


def test_recorded_unavailable_item_offers_only_undo(make_app: AppFactory) -> None:
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[("privacy.copilot", "privacy", False)],
        unavailable={"privacy.copilot": ("windows_version", TOO_NEW)},
    )
    engine.applied.add("privacy.copilot")
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    row = rows_by_id(app)["privacy.copilot"]
    assert "privacy.copilot" in app.optimize_list.visible_ids(), "recorded, so it is listed"
    assert tags_of(row).startswith("Not on this version of Windows")
    assert row.note_label is not None
    assert row.note_label.cget("text") == f"– {TOO_NEW} {RECORDED_SUFFIX}"
    assert row.action_button is not None and row.action_button.cget("text") == "Undo"
    assert row.tag_label is None

    app._on_item_action(row.item, "revert")
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: "privacy.copilot" not in engine.applied and scanned(app))
    assert engine.calls_named("revert")[-1] == (["privacy.copilot"], False)
    assert engine.calls_named("apply") == []
    row = rows_by_id(app)["privacy.copilot"]
    assert row.action_button is None, "nothing left to undo"
    assert app.errors == []


def test_retired_items_without_records_are_hidden(make_app: AppFactory) -> None:
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[("privacy.cortana", "privacy", True)],
        unavailable={"privacy.cortana": ("retired", RETIRED)},
    )
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    panel = app.optimize_list
    assert "privacy.cortana" not in rows_by_id(app), "no row at all"
    for value in ("available", "changed", "unavailable", "all"):
        panel.set_show(value)
        assert "privacy.cortana" not in panel.visible_ids(), value
    panel.set_show("all")
    assert panel.heading_text("privacy") == "▾ PRIVACY AND TELEMETRY  ·  2"
    assert panel.summary.cget("text").split("  ·  ")[1] == "5 items"

    # A record of an earlier version is listed with its reason and offers Undo.
    engine.applied.add("privacy.cortana")
    app.start_scan()
    pump(app, 5.0, until=lambda: scanned(app) and "privacy.cortana" in rows_by_id(app))
    panel.set_show("available")
    row = rows_by_id(app)["privacy.cortana"]
    assert "privacy.cortana" in panel.visible_ids()
    assert tags_of(row).startswith("Retired")
    assert row.note_label is not None and row.note_label.cget("text") == f"– {RETIRED} {RECORDED_SUFFIX}"
    assert row.action_button is not None and row.action_button.cget("text") == "Undo"
    assert app.errors == []


def test_mode_off_includes_mode_undo_items(make_app: AppFactory) -> None:
    # An earlier version recommended Recall; turning the mode off still restores its records.
    app, engine = make_app(
        elevated=True, extra_tweaks=[("privacy.recall", "privacy", False)], mode_undo={"privacy.recall"}
    )
    engine.applied.update({"privacy.activity_history", "privacy.recall"})
    pump(app, 5.0, until=lambda: scanned(app))
    toggle = app.toggles["privacy"]
    assert toggle.switch.get()
    assert app._recommended_ids("privacy") == [
        "privacy.activity_history",
        "privacy.advertising_id",
        "privacy.recall",
    ]
    toggle.switch.toggle()
    dialog = next_dialog(app)
    assert "delete value: privacy.recall" in dialog_text(dialog)
    pump(app, 0.3)
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: not engine.applied and scanned(app))
    assert engine.calls_named("revert")[-1] == (
        ["privacy.activity_history", "privacy.advertising_id", "privacy.recall"],
        False,
    )
    assert app.errors == []


def test_undo_limit_row_and_danger_apply_dialog(make_app: AppFactory) -> None:
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[("privacy.recall", "privacy", False)],
        undo_limits={"privacy.recall": LIMIT},
    )
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    row = rows_by_id(app)["privacy.recall"]
    assert row.undo_limit_label is not None
    assert row.undo_limit_label.cget("text") == f"⚠ {LIMIT}"
    assert row.undo_limit_label.cget("text_color") == theme.WARNING
    assert row.undo_limit_label in row.wrapped_labels

    app._on_item_action(row.item, "apply")
    dialog = next_dialog(app)
    assert dialog.title_text == "Apply Recall?"
    assert f"Description of privacy.recall.\n\n⚠ {LIMIT}" in dialog_text(dialog)
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL, "the danger style"
    pump(app, 0.3)
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: "privacy.recall" in engine.applied and scanned(app))

    # A tweak without an undo limit keeps the usual dialog.
    app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "apply")
    dialog = next_dialog(app)
    assert dialog.confirm_button.cget("fg_color") == theme.ACCENT
    assert "⚠" not in dialog_text(dialog)
    close_dialogs(app)
    assert app.errors == []


def test_undo_limit_in_a_mode_uses_the_danger_dialog(make_app: AppFactory) -> None:
    # No recommended tweak has an undo limit; should one get one, the mode warns before applying.
    app, engine = make_app(elevated=True, undo_limits={"privacy.advertising_id": LIMIT})
    pump(app, 5.0, until=lambda: scanned(app))
    toggle = app.toggles["privacy"]
    toggle.switch.toggle()
    dialog = next_dialog(app)
    assert f"⚠ Advertising Id: {LIMIT}" in dialog_text(dialog)
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL
    close_dialogs(app)
    pump(app, 5.0, until=lambda: idle(app))

    # "Apply remaining" warns the same way while the item is still to be applied.
    engine.applied.add("privacy.activity_history")
    app.start_scan()
    pump(app, 5.0, until=lambda: scanned(app) and toggle.partial)
    toggle.remaining_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Apply the rest of Privacy Mode?"
    assert f"⚠ Advertising Id: {LIMIT}" in dialog_text(dialog)
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL
    close_dialogs(app)
    pump(app, 5.0, until=lambda: idle(app))

    # A mode without one keeps the usual style.
    app.toggles["gaming"].switch.toggle()
    dialog = next_dialog(app)
    assert "⚠" not in dialog_text(dialog)
    assert dialog.confirm_button.cget("fg_color") == theme.ACCENT
    close_dialogs(app)
    assert all(dry_run for _, _, dry_run in engine.calls_named("apply_category")), "only plans ran"
    assert app.errors == []


def test_former_item_offers_apply_and_explains(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, former={"privacy.advertising_id": [FORMER]})
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    row = rows_by_id(app)["privacy.advertising_id"]
    assert row.action_button is not None and row.action_button.cget("text") == "Apply"
    assert [label.cget("text") for label in row.former_labels] == [former_caveat(FORMER)]
    assert former_caveat(FORMER).endswith(
        "Apply this tweak to restore it to not set; Undo in History restores everything."
    )
    assert row.former_labels[0].cget("text_color") == theme.WARNING

    app._on_item_action(row.item, "apply")
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert f"• restore {FORMER['target']} to not set (no longer part of this tweak)" in text
    pump(app, 0.3)
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: not engine.former_records and scanned(app))
    row = rows_by_id(app)["privacy.advertising_id"]
    assert row.former_labels == []
    assert row.action_button is not None and row.action_button.cget("text") == "Undo"
    assert app.errors == []


def test_not_kept_apply_shows_the_warning_dialog(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The check five seconds later has tests of its own; here it must not replace the status.
    monkeypatch.setattr(app_module, "SETTLED_CHECK_MS", LATER_THAN_ANY_TEST_MS)
    app, engine = make_app(elevated=True, not_kept={"gaming.game_mode": [GAME_MODE_RESET]})
    pump(app, 5.0, until=lambda: scanned(app))
    statuses = record_statuses(app)
    app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "apply")
    confirm_dialog(app)
    dialog = dialog_titled(app, "1 change not kept")
    text = dialog_text(dialog)
    assert "0 applied, 1 not kept." in text
    assert NOT_KEPT_TEXT in text
    assert "• Game Mode: AllowAutoGameMode is not set, not 1" in text
    assert "by Windows" not in text and "Windows did not keep" not in text
    assert ("⚠ Done: 0 applied, 1 not kept.", theme.WARNING) in statuses
    pump(app, 0.3)
    dialog.confirm_button.invoke()

    # The rescan keeps the result in the status bar and shows the value on the row.
    pump(app, 5.0, until=lambda: scanned(app))
    assert app.status_message.cget("text") == "⚠ Done: 0 applied, 1 not kept."
    row = rows_by_id(app)["gaming.game_mode"]
    assert row.item["state"] == "partial"
    assert row.note_label is not None and row.note_label.cget("text").startswith("⚠ Changed back since Cairn")
    assert app.errors == []


def test_settled_check_runs_once_after_the_delay(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(app_module, "SETTLED_CHECK_MS", 50)
    monkeypatch.setattr(app_module, "SETTLED_RETRY_MS", 50)
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    statuses = record_statuses(app)
    app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "apply")
    confirm_dialog(app)
    pump(app, 15.0, until=lambda: app.status_message.cget("text") == SETTLED_KEPT_TEXT)
    assert engine.calls_named("verify") == [(["gaming.game_mode"],)]
    assert ("Done: 1 applied.", theme.GOOD) in statuses
    assert statuses[-1] == (SETTLED_KEPT_TEXT, theme.GOOD)
    pump(app, 0.5)
    assert len(engine.calls_named("verify")) == 1, "once per apply"
    assert app.errors == []


def test_settled_check_reports_values_that_changed_back(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(app_module, "SETTLED_CHECK_MS", 50)
    monkeypatch.setattr(app_module, "SETTLED_RETRY_MS", 50)
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    # The value changes back after the apply read it, by the time of the check.
    verify = engine.verify

    def changed_back(ids: list[str]) -> dict[str, Any]:
        engine.drifted["performance.sysmain"] = [
            {**GAME_MODE_RESET, "label": "service SysMain", "found": "Automatic"}
        ]
        return verify(ids)

    engine.verify = changed_back  # type: ignore[method-assign]
    pump(app, 10.0, until=lambda: idle(app))
    scans = len(engine.calls_named("scan"))
    app._on_item_action(rows_by_id(app)["performance.sysmain"].item, "apply")
    confirm_dialog(app)
    changed = SETTLED_CHANGED_TEXT.format(settings="1 setting", titles="Sysmain")
    pump(app, 15.0, until=lambda: app.status_message.cget("text") == changed and scanned(app))
    assert engine.calls_named("verify")[-1] == (["performance.sysmain"],)
    pump(app, 10.0, until=lambda: rows_by_id(app)["performance.sysmain"].item["state"] == "partial")
    assert len(engine.calls_named("scan")) >= scans + 2, "the apply's rescan and the check's"
    assert app.status_message.cget("text") == changed, "the rescan keeps the message"
    assert app.status_message.cget("text_color") == theme.WARNING
    assert app.errors == []


def test_settled_check_claims_nothing_it_could_not_read(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    monkeypatch.setattr(app_module, "SETTLED_CHECK_MS", 50)
    monkeypatch.setattr(app_module, "SETTLED_RETRY_MS", 50)
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    caplog.set_level(logging.WARNING, logger="optimizer.app")
    # By the time of the check the value cannot be read back.
    verify = engine.verify

    def unreadable(ids: list[str]) -> dict[str, Any]:
        engine.unreadable.add("gaming.game_mode")
        return verify(ids)

    engine.verify = unreadable  # type: ignore[method-assign]
    statuses = record_statuses(app)
    app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "apply")
    confirm_dialog(app)
    unread = "HKLM\\Test\\gaming.game_mode could not be read (access denied)."
    pump(app, 15.0, until=lambda: unread in caplog.text and idle(app))
    pump(app, 0.3)
    assert engine.calls_named("verify") == [(["gaming.game_mode"],)]
    assert SETTLED_KEPT_TEXT not in [text for text, _ in statuses]
    assert app.status_message.cget("text") == "Done: 1 applied.", "the apply's result stays"
    assert app.errors == []


def test_settled_check_claims_nothing_when_it_read_nothing(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(app_module, "SETTLED_CHECK_MS", 50)
    monkeypatch.setattr(app_module, "SETTLED_RETRY_MS", 50)
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))

    def read_nothing(ids: list[str]) -> dict[str, Any]:
        engine._record("verify", list(ids))
        return {"entries": [], "checked": 0, "unchecked": 0, "warnings": [], "duration_ms": 1}

    engine.verify = read_nothing  # type: ignore[method-assign]
    statuses = record_statuses(app)
    app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "apply")
    confirm_dialog(app)
    pump(app, 15.0, until=lambda: bool(engine.calls_named("verify")) and idle(app))
    pump(app, 0.3)
    assert engine.calls_named("verify") == [(["gaming.game_mode"],)]
    assert SETTLED_KEPT_TEXT not in [text for text, _ in statuses]
    assert app.status_message.cget("text") == "Done: 1 applied.", "the apply's result stays"
    assert app.errors == []


@pytest.mark.parametrize("own_value", ["recorded", "already_set"])
def test_settled_check_claims_nothing_after_restoring_an_earlier_versions_value(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch, own_value: str
) -> None:
    monkeypatch.setattr(app_module, "SETTLED_CHECK_MS", 50)
    monkeypatch.setattr(app_module, "SETTLED_RETRY_MS", 50)
    # The apply puts back the value an earlier version set and ends its record, so the check
    # cannot read it. Game Mode's own value is recorded and read, or was already set before
    # Cairn ran, so nothing of it is recorded and the check reads nothing at all.
    preset = {"gaming.game_mode"} if own_value == "already_set" else set()
    app, engine = make_app(elevated=True, preset=preset, former={"gaming.game_mode": [FORMER]})
    if own_value == "recorded":
        engine.applied.add("gaming.game_mode")
    pump(app, 5.0, until=lambda: scanned(app))
    statuses = record_statuses(app)
    row = rows_by_id(app)["gaming.game_mode"]
    assert row.action_button is not None and row.action_button.cget("text") == "Apply"
    app._on_item_action(row.item, "apply")
    confirm_dialog(app)
    pump(app, 15.0, until=lambda: bool(engine.calls_named("verify")) and idle(app))
    pump(app, 0.3)
    assert engine.calls_named("verify") == [(["gaming.game_mode"],)]
    assert not engine.former_records, "the earlier version's value was restored"
    assert SETTLED_KEPT_TEXT not in [text for text, _ in statuses]
    assert app.status_message.cget("text") == "Done: 1 applied.", "the apply's result stays"
    assert app.errors == []


def test_settled_check_waits_while_busy_and_gives_up(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    monkeypatch.setattr(app_module, "SETTLED_CHECK_MS", 50)
    monkeypatch.setattr(app_module, "SETTLED_RETRY_MS", 20)
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    caplog.set_level(logging.INFO, logger="optimizer.app")
    # The rescan after the apply holds the window until it is released.
    release = threading.Event()
    scan = engine.scan

    def held_scan() -> dict[str, Any]:
        release.wait(30)
        return scan()

    engine.scan = held_scan  # type: ignore[method-assign]
    try:
        app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "apply")
        confirm_dialog(app)
        pump(app, 15.0, until=lambda: "given up" in caplog.text)
        assert app._busy, "given up while the rescan still runs"
    finally:
        release.set()
    pump(app, 10.0, until=lambda: scanned(app))
    pump(app, 0.3)
    assert engine.calls_named("verify") == []
    assert app.status_message.cget("text") == "Done: 1 applied.", "the result stays"
    assert app.errors == []


def test_an_undo_drops_the_pending_check(make_app: AppFactory, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(app_module, "SETTLED_CHECK_MS", LATER_THAN_ANY_TEST_MS)
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "apply")
    confirm_dialog(app)
    pump(app, 10.0, until=lambda: scanned(app) and "gaming.game_mode" in engine.applied)
    assert app._settled_job is not None and app._settled_ids == ["gaming.game_mode"]
    app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "revert")
    confirm_dialog(app)
    pump(app, 10.0, until=lambda: scanned(app) and not engine.applied)
    assert app._settled_job is None and app._settled_ids == [], "the undo dropped the check"
    assert engine.calls_named("verify") == []
    assert app.errors == []


def test_a_new_apply_replaces_the_pending_check_and_closing_cancels_it(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(app_module, "SETTLED_CHECK_MS", LATER_THAN_ANY_TEST_MS)
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "apply")
    confirm_dialog(app)
    pump(app, 10.0, until=lambda: scanned(app) and app._settled_ids == ["gaming.game_mode"])
    app._on_item_action(rows_by_id(app)["performance.sysmain"].item, "apply")
    confirm_dialog(app)
    pump(app, 10.0, until=lambda: scanned(app) and app._settled_ids == ["performance.sysmain"])
    app._request_close(force=True)
    assert app._settled_job is None
    assert app.errors == []


def test_drift_note_on_row(make_app: AppFactory) -> None:
    changed = {**GAME_MODE_RESET, "found": "0", "original": "not set", "back_to_original": False}
    app, engine = make_app(elevated=True, drift={"gaming.game_mode": [changed]})
    engine.applied.add("gaming.game_mode")
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    row = rows_by_id(app)["gaming.game_mode"]
    assert row.item["state"] == "partial"
    assert row.note_label is not None
    assert row.note_label.cget("text") == (
        "⚠ Changed since Cairn applied it: AllowAutoGameMode is 0, not 1. Apply sets it again."
    )
    assert row.note_label.cget("text_color") == theme.WARNING
    assert row.action_button is not None and row.action_button.cget("text") == "Apply"
    assert app.status_message.cget("text") == (
        "Scan complete: 6 items checked in 12 ms. ⚠ 1 change no longer holds; see History."
    )
    assert app.status_message.cget("text_color") == theme.WARNING
    assert app.errors == []


def test_edge_apply_names_the_edge_policy_page(make_app: AppFactory) -> None:
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[("privacy.edge_feedback", "privacy", True)],
        requires={"privacy.edge_feedback": "edge"},
    )
    pump(app, 5.0, until=lambda: scanned(app) and bool(app._titles))
    app._on_item_action(rows_by_id(app)["privacy.edge_feedback"].item, "apply")
    confirm_dialog(app)
    dialog = dialog_titled(app, "Changes applied")
    assert EDGE_POLICY_TEXT == "Open edge://policy in Edge to see which policies it uses."
    assert f"1 applied. {EDGE_POLICY_TEXT}" in dialog_text(dialog)
    close_dialogs(app)
    pump(app, 5.0, until=lambda: scanned(app))

    # Other tweaks need no such line, so their apply ends without a dialog.
    app._on_item_action(rows_by_id(app)["gaming.game_mode"].item, "apply")
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: "gaming.game_mode" in engine.applied and scanned(app))
    pump(app, 0.3)
    assert not dialogs(app)
    assert app.errors == []


def test_optimize_fits_with_every_kind_of_row(make_app: AppFactory) -> None:
    app, engine = make_app(
        elevated=True,
        extra_tweaks=[("privacy.copilot", "privacy", False), ("privacy.recall", "privacy", False)],
        unavailable={"privacy.copilot": ("windows_version", TOO_NEW)},
        undo_limits={"privacy.recall": LIMIT},
        former={"privacy.advertising_id": [FORMER]},
        drift={"gaming.game_mode": [GAME_MODE_RESET]},
    )
    engine.applied.update({"gaming.game_mode", "privacy.copilot"})
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    assert_section_fits(app, "Optimize")
    for row in app.optimize_list.rows:
        for label in row.wrapped_labels:
            assert label.cget("wraplength") == app.optimize_list.row_wrap
    assert app.errors == []
