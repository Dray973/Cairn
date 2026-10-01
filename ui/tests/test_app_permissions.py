"""Permissions section in the real window with a FakeEngine: a guide with one card per device, whose
button opens the device's page in Windows Settings, and the desktop apps Windows recorded using it.
No switch and no permission state exist; nothing is changed or journaled. Also: reading on every
visit, errors and an outdated engine, the busy window, the layout at every size, and the
permission records earlier builds left, which History still shows and undoes.

Nothing on this PC is read or changed: every call goes to the in-memory FakeEngine, and opening a
Windows Settings page goes to a recorder that replaces `system.open_uri`.
"""

from __future__ import annotations

import re
import threading
from collections.abc import Iterator
from contextlib import contextmanager
from typing import Any

import pytest

from optimizer import system
from optimizer.app import CLOSE_JOURNALED
from optimizer.features.permissions import PERMISSIONS_SECTION, UNSUPPORTED_TEXT
from optimizer.widgets.permissions import (
    CAPABILITIES,
    ERROR_TEXT,
    INTRO_TEXT,
    NO_RECENT_TEXT,
    RECENT_ERROR_TEXT,
    RECENT_HEADING,
    RECENT_SOURCE,
    RECENT_UNAVAILABLE_TEXT,
    SETTINGS_PAGES,
    GuideCard,
    PermissionsPanel,
    button_text,
    guide_text,
)

from .app_support import (
    FIT_SIZES,
    App,
    AppFactory,
    assert_section_fits,
    confirm_dialog,
    confirm_dialog_text,
    ctk,
    dialogs,
    idle,
    pump,
    resize_to,
    scanned,
    show_section,
    theme,
)
from .fake_permissions import DEVICE_STORE, LOCATION_SENSOR, USER_STORE

MEET_FAMILY = "Contoso.Meet_aaaaaaaaaaaaa"
MEET_CAMERA = f"camera:app:{MEET_FAMILY}"
# Widgets that would let the user switch or pick a permission.
CHOICE_TYPES = (
    ctk.CTkSegmentedButton,
    ctk.CTkSwitch,
    ctk.CTkCheckBox,
    ctk.CTkRadioButton,
    ctk.CTkOptionMenu,
    ctk.CTkComboBox,
    ctk.CTkSlider,
)
# Texts of the permission states earlier builds showed.
STATE_TEXTS = ("Allowed", "Blocked", "Asks first", "Not set", "Allow", "Deny")


def loaded(app: App) -> bool:
    panel = app.permissions_panel
    return panel.report is not None and not panel.loading and not app._permissions_loading and idle(app)


def open_permissions(app: App) -> PermissionsPanel:
    """Shows the section and waits until the guide is read and nothing else runs."""
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, PERMISSIONS_SECTION)
    pump(app, 5.0, until=lambda: loaded(app))
    return app.permissions_panel


def status(app: App) -> str:
    return str(app.status_message.cget("text"))


def descendants(widget: Any) -> Iterator[Any]:
    for child in widget.winfo_children():
        yield child
        yield from descendants(child)


def texts(widget: Any) -> list[str]:
    """The texts of every label and button below `widget`."""
    found = []
    for child in descendants(widget):
        if isinstance(child, ctk.CTkLabel | ctk.CTkButton):
            found.append(str(child.cget("text")))
    return found


def recent_paths(card: GuideCard) -> list[str]:
    return [str(row.path_label.cget("text")) for row in card.recent_rows]


def note(card: GuideCard) -> str | None:
    return None if card.note_label is None else str(card.note_label.cget("text"))


@contextmanager
def held(engine: Any, name: str) -> Iterator[threading.Event]:
    """Holds every call of the fake's `name` on the bridge's worker until the block ends; the
    event is set once a call is waiting."""
    entered, release = threading.Event(), threading.Event()
    real = getattr(engine, name)

    def wait(*args: Any, **kwargs: Any) -> Any:
        entered.set()
        release.wait(30.0)
        return real(*args, **kwargs)

    setattr(engine, name, wait)
    try:
        yield entered
    finally:
        release.set()


def test_loads_only_when_the_section_is_shown(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    assert engine.calls_named("permissions_list") == []
    assert app.permissions_panel.report is None

    panel = open_permissions(app)
    assert len(engine.calls_named("permissions_list")) == 1
    assert list(panel.cards) == list(CAPABILITIES)
    assert panel.summary.cget("text") == INTRO_TEXT
    assert app.section_title.cget("text") == PERMISSIONS_SECTION
    assert PERMISSIONS_SECTION not in app._stub_sections
    assert engine.calls_named("permissions_set") == []
    assert app.errors == []


def test_reloads_on_every_visit_and_coalesces(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    with held(engine, "permissions_list") as entered:
        show_section(app, PERMISSIONS_SECTION)
        pump(app, 5.0, until=entered.is_set)
        show_section(app, "Dashboard")
        show_section(app, PERMISSIONS_SECTION)
        pump(app, 0.2)
        # The second visit waits for the read in flight instead of queueing another behind it.
        assert len(engine.calls_named("permissions_list")) == 0, "the held call has not run yet"
        assert app._permissions_loading
        assert app._permissions_reload
        assert app.permissions_panel.refresh_button.cget("state") == "disabled"
    pump(app, 5.0, until=lambda: len(engine.calls_named("permissions_list")) == 2)
    pump(app, 5.0, until=lambda: loaded(app))
    assert not app._permissions_reload

    show_section(app, "Dashboard")
    show_section(app, PERMISSIONS_SECTION)
    pump(app, 5.0, until=lambda: len(engine.calls_named("permissions_list")) == 3)
    pump(app, 5.0, until=lambda: loaded(app))
    assert app.errors == []


def test_each_card_explains_and_opens_its_page_in_windows_settings(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    opened: list[str] = []
    monkeypatch.setattr(system, "open_uri", opened.append)
    app, engine = make_app(elevated=False)
    panel = open_permissions(app)
    for capability in CAPABILITIES:
        card = panel.cards[capability]
        assert card.text_label.cget("text") == guide_text(capability)
        assert card.button.cget("text") == button_text(capability)
        assert card.button.cget("state") == "normal", "a standard user opens Settings too"
    assert [panel.cards[c].button.cget("text") for c in CAPABILITIES] == [
        "Open camera settings",
        "Open microphone settings",
        "Open location settings",
    ]
    for capability in CAPABILITIES:
        panel.cards[capability].button.invoke()
    assert opened == [
        "ms-settings:privacy-webcam",
        "ms-settings:privacy-microphone",
        "ms-settings:privacy-location",
    ]
    assert opened == list(SETTINGS_PAGES.values())
    # Opening Settings changes and records nothing, and asks nothing.
    pump(app, 0.2)
    assert not dialogs(app)
    assert engine.calls_named("permissions_set") == []
    assert engine.journal_summary()["registry_active"] == 0
    assert app.errors == []


def test_no_switch_and_no_permission_state_exist(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, permission_recorded={MEET_CAMERA: "prompt"})
    open_permissions(app)
    frame = app.section_frame(PERMISSIONS_SECTION)
    choices = [type(w).__name__ for w in descendants(frame) if isinstance(w, CHOICE_TYPES)]
    assert choices == []
    shown = texts(frame)
    for state in STATE_TEXTS:
        word = re.compile(rf"\b{re.escape(state)}\b")
        assert not any(word.search(text) for text in shown), (state, shown)
    buttons = [w for w in descendants(frame) if isinstance(w, ctk.CTkButton)]
    assert sorted(str(b.cget("text")) for b in buttons) == sorted(
        ["Refresh", *(button_text(c) for c in CAPABILITIES)]
    )
    assert app.errors == []


def test_each_card_lists_the_desktop_apps_windows_recorded(make_app: AppFactory) -> None:
    app, _ = make_app(
        elevated=True,
        permission_recent=(
            ("camera", "C:\\Program Files\\Contoso\\Meet\\meet.exe", "2026-09-25T10:00:00Z", False),
            ("camera", "C:\\Tools\\Northwind\\recorder.exe", "2026-09-26T09:00:00Z", True),
            ("location", "C:\\Program Files\\Fabrikam\\Maps\\maps.exe", "2026-09-20T08:30:00Z", False),
        ),
    )
    panel = open_permissions(app)
    camera, microphone, location = (panel.cards[c] for c in CAPABILITIES)
    for capability, card in zip(CAPABILITIES, (camera, microphone, location), strict=True):
        assert card.recent_heading.cget("text") == RECENT_HEADING
        assert card.source_label.cget("text") == RECENT_SOURCE.format(noun=capability)
        assert card.source_label.cget("text").startswith("Windows' own record of the desktop apps")
    assert recent_paths(camera) == [
        "C:\\Tools\\Northwind\\recorder.exe",
        "C:\\Program Files\\Contoso\\Meet\\meet.exe",
    ]
    assert camera.recent_rows[0].time_label.cget("text") == "◐ in use now"
    assert camera.recent_rows[1].time_label.cget("text").startswith("last used 2026-09-2")
    assert note(camera) is None
    assert recent_paths(microphone) == []
    assert note(microphone) == NO_RECENT_TEXT.format(noun="microphone")
    assert recent_paths(location) == ["C:\\Program Files\\Fabrikam\\Maps\\maps.exe"]

    # Reading again with nothing new keeps the rows that are shown.
    rows = list(camera.recent_rows)
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: loaded(app))
    assert camera.recent_rows == rows
    assert app.errors == []


def test_an_outdated_engine_still_offers_the_settings_pages(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    opened: list[str] = []
    monkeypatch.setattr(system, "open_uri", opened.append)
    app, engine = make_app(elevated=True, unsupported=["permissions_list"])
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, PERMISSIONS_SECTION)
    pump(app, 0.3)
    panel = app.permissions_panel
    assert panel.summary.cget("text") == f"⚠ {UNSUPPORTED_TEXT}"
    assert panel.summary.cget("text_color") == theme.WARNING
    assert panel.refresh_button.cget("state") == "disabled"
    assert all(note(card) == RECENT_UNAVAILABLE_TEXT for card in panel.cards.values())
    assert engine.calls_named("permissions_list") == []
    panel.cards["microphone"].button.invoke()
    assert opened == ["ms-settings:privacy-microphone"]
    assert app.errors == []


def test_a_read_error_is_shown_and_refresh_reads_again(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, permission_error="access denied")
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, PERMISSIONS_SECTION)
    panel = app.permissions_panel
    pump(app, 5.0, until=lambda: panel.loaded and not panel.loading and idle(app))
    assert panel.summary.cget("text") == ERROR_TEXT.format(message="access denied")
    assert panel.summary.cget("text_color") == theme.CRITICAL
    assert all(note(card) == RECENT_ERROR_TEXT for card in panel.cards.values())
    assert panel.refresh_button.cget("state") == "normal"
    assert all(card.button.cget("state") == "normal" for card in panel.cards.values())

    engine._permission_error = None
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: loaded(app))
    assert panel.summary.cget("text") == INTRO_TEXT
    assert recent_paths(panel.cards["camera"]) == ["C:\\Program Files\\Contoso\\Meet\\meet.exe"]
    assert app.errors == []


def test_read_warnings_show_in_the_banner(make_app: AppFactory) -> None:
    warning = "Camera: cannot list the desktop apps that used the camera: access denied"
    app, _ = make_app(elevated=True, permission_warnings=[warning])
    panel = open_permissions(app)
    assert panel.banner.winfo_ismapped()
    assert panel.banner.cget("text") == f"⚠ {warning}"
    assert panel.banner.cget("text_color") == theme.WARNING
    assert app.errors == []


def test_while_the_window_is_busy_settings_still_open(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    opened: list[str] = []
    monkeypatch.setattr(system, "open_uri", opened.append)
    app, _ = make_app(elevated=True)
    panel = open_permissions(app)
    app._set_busy(True, CLOSE_JOURNALED, action="Testing")
    try:
        assert panel.refresh_button.cget("state") == "disabled"
        button = panel.cards["location"].button
        assert button.cget("state") == "normal"
        button.invoke()
        assert opened == ["ms-settings:privacy-location"]
    finally:
        app._set_busy(False)
    assert panel.refresh_button.cget("state") == "normal"
    assert app.errors == []


def test_only_the_privacy_pages_are_opened(make_app: AppFactory, monkeypatch: pytest.MonkeyPatch) -> None:
    opened: list[str] = []
    monkeypatch.setattr(system, "open_uri", opened.append)
    app, _ = make_app(elevated=True)
    open_permissions(app)
    app._open_permission_settings("windowsupdate")
    assert opened == []
    assert status(app) == "Cairn has no Settings page for windowsupdate."

    def refuse(uri: str) -> None:
        raise OSError("no handler")

    monkeypatch.setattr(system, "open_uri", refuse)
    app._open_permission_settings("camera")
    assert status(app) == "Could not open Windows Settings: no handler"
    assert app.errors == []


LONG_RECENT = (
    (
        "camera",
        "C:\\Program Files\\Northwind Traders\\Northwind Meetings Desktop Client\\Current Version\\"
        "bin\\x64\\NorthwindMeetingsDesktopClientLauncher.exe",
        "2026-09-26T08:00:00Z",
        True,
    ),
    (
        "microphone",
        "C:\\Users\\Test\\AppData\\Local\\Programs\\Contoso Voice Recorder and Transcriber\\"
        "Contoso.Voice.Recorder.And.Transcriber.Desktop.exe",
        "2026-09-25T18:45:00Z",
        False,
    ),
)


def cards_fit(app: App, panel: PermissionsPanel) -> None:
    """At every size the sections must fit, every wrapped label of the panel shows its whole text,
    and every card's title and button are shown in full inside the card."""
    app.minsize(900, 560)
    for width, height in FIT_SIZES:
        resize_to(app, width, height)
        pump(app, 5.0, until=lambda: idle(app))
        where = f"{width}x{height}"
        for label in panel.wrapped_labels:
            if label.winfo_ismapped():
                need, got = label.winfo_reqwidth(), label.winfo_width()
                assert got >= need, f"{label.cget('text')[:40]!r} is cut off at {where}"
        for capability, card in panel.cards.items():
            assert card.button.winfo_ismapped(), f"{capability} at {where}"
            assert card.title_label.winfo_width() >= card.title_label._label.winfo_reqwidth()
            assert card.button.winfo_width() >= card.button._text_label.winfo_reqwidth(), where
            right = card.button.winfo_rootx() + card.button.winfo_width()
            assert right <= card.winfo_rootx() + card.winfo_width() + 1, f"{capability} button at {where}"
            for row in card.recent_rows:
                time_right = row.time_label.winfo_rootx() + row.time_label.winfo_width()
                assert time_right <= card.winfo_rootx() + card.winfo_width() + 1, f"{capability} at {where}"


def test_section_fits_at_every_size(make_app: AppFactory) -> None:
    app, _ = make_app(
        elevated=False,
        permission_recent=LONG_RECENT,
        permission_warnings=[
            "Location: the usage of 2 desktop app(s) could not be read",
            "Camera: cannot list the desktop apps that used the camera: the key is locked by another program",
        ],
    )
    panel = open_permissions(app)
    assert panel.banner.winfo_ismapped()
    cards_fit(app, panel)
    assert_section_fits(app, PERMISSIONS_SECTION)
    assert app.errors == []


def test_history_still_shows_and_undoes_earlier_permission_changes(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False, permission_recorded={MEET_CAMERA: "prompt"})
    open_permissions(app)
    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 1 and idle(app))
    row = app.history_panel.rows[0]
    # The section lists no Store apps, so the record is titled with the package name.
    assert row.change.title == "Camera permission: Contoso.Meet"
    assert row.change.kind == "permission"
    assert row.undo_button.cget("state") == "normal", "a per-user record undoes without elevation"
    row.undo_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("revert_targets")))
    key = f"{USER_STORE}\\webcam\\{MEET_FAMILY}"
    assert engine.calls_named("revert_targets")[-1][0]["registry"] == [
        {"hive": "HKCU", "key_path": key, "value_name": "Value"},
        {"hive": "HKCU", "key_path": key, "value_name": "LastSetTime"},
    ]
    pump(app, 5.0, until=lambda: engine._permission_active_count() == 0 and idle(app))
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 0 and idle(app))
    assert engine.calls_named("permissions_set") == []
    assert app.errors == []


def test_one_undo_restores_the_three_values_of_the_location_switch(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, permission_recorded={"location:device": "allow"})
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 1 and idle(app))
    row = app.history_panel.rows[0]
    assert row.change.title == "Location services on this PC"
    assert len(row.change.details) == 3
    row.undo_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("revert_targets")))
    sent = engine.calls_named("revert_targets")[-1][0]["registry"]
    assert [(t["hive"], t["key_path"], t["value_name"]) for t in sent] == [
        ("HKLM", f"{DEVICE_STORE}\\location", "Value"),
        ("HKLM", f"{DEVICE_STORE}\\location", "LastSetTime"),
        ("HKLM", LOCATION_SENSOR, "SensorPermissionState"),
    ]
    pump(app, 5.0, until=lambda: engine._permission_active_count() == 0 and idle(app))
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 0 and idle(app))
    assert app.errors == []


def test_revert_all_restores_earlier_permission_changes(make_app: AppFactory) -> None:
    app, engine = make_app(
        elevated=True, permission_recorded={"camera:device": "allow", "location:apps": "deny"}
    )
    open_permissions(app)
    lists = len(engine.calls_named("permissions_list"))
    app.revert_button.invoke()
    assert "Revert all changes?" in confirm_dialog_text(app)
    pump(app, 5.0, until=lambda: (False,) in engine.calls_named("revert_all"))
    pump(app, 5.0, until=lambda: engine._permission_active_count() == 0 and idle(app))
    for dialog in dialogs(app):
        pump(app, 0.3)
        dialog._cancel()
    # Nothing the guide shows depends on the journal, so it is not read again.
    assert len(engine.calls_named("permissions_list")) == lists
    assert engine.calls_named("permissions_set") == []
    assert app.errors == []
