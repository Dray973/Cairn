"""Optimize list in the real window with a FakeEngine: tweaks whose requirement this PC lacks
(Office, Edge, GPU scheduling) are shown as not on this PC with a muted reason, offer no
action and count toward no mode; read errors stay warnings.

Nothing on this PC is read or changed: every call goes to the in-memory FakeEngine.
"""

from __future__ import annotations

from .app_support import (
    AppFactory,
    ScanRow,
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

OFFICE_MISSING = "Microsoft Office (2016 or later) is not installed."
EDGE_MISSING = "Microsoft Edge is not installed."


def labels(row: ScanRow) -> list[str]:
    return [str(w.cget("text")) for w in row.winfo_children() if isinstance(w, ctk.CTkLabel)]


def tags_of(row: ScanRow) -> str:
    """The row's tag line ("Not on this PC  ·  recommended  ·  low risk")."""
    return next(text for text in labels(row) if "risk" in text)


def test_unavailable_item_note_is_muted(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, missing={"privacy.cortana": OFFICE_MISSING})
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Optimize")
    row = rows_by_id(app)["privacy.cortana"]
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
    app, engine = make_app(elevated=True, missing={"privacy.cortana": OFFICE_MISSING})
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
    assert "privacy.cortana" not in text
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
