"""Security and Boot history sections in the real window with a FakeEngine: when the checkup
loads, the automatic and the online Windows Update search, every kind of fix, another account,
a standard user, a copy that may not ask for administrator rights, "Turn off at startup", the tool
buttons and the views of the boot history (what slowed starts and shutdowns), an outdated engine,
closing while a search runs and the layout.

Nothing on this PC is read or changed: every call goes to the in-memory FakeEngine, and
opening a page goes to a recorder that replaces `system.open_uri`.
"""

from __future__ import annotations

import time
from typing import Any

import pytest

from optimizer import system
from optimizer.app import ADMIN_UNAVAILABLE_TITLE
from optimizer.features.health import (
    BOOT_SECTION,
    CHECKUP_STALE_S,
    SCAN_STATUS,
    SCAN_STATUS_ONLINE,
    SECURITY_SECTION,
    UNSUPPORTED_TEXT,
    UPDATE_POLL_HIDDEN_S,
)
from optimizer.widgets.boot import ALREADY_OFF_TEXT, FAST_STARTUP_TEXT, NEEDS_ADMIN_TEXT, BootPanel
from optimizer.widgets.security import PER_USER_NOTE, CheckRow, SecurityPanel

from .app_support import (
    ADMIN_DIALOG_TITLE,
    App,
    AppFactory,
    assert_section_fits,
    confirm_dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    resize_to,
    scanned,
    show_section,
    theme,
)

FIREWALL_ATTENTION = {"state": "attention", "severity": "high", "summary": "Off for public networks"}
UAC_ATTENTION = {"state": "attention", "severity": "low", "summary": "Notify without dimming the desktop"}
SAC_OFF = {
    "state": "attention",
    "severity": "info",
    "summary": "Off",
    "detail": "Once off, it can only be turned on again by reinstalling Windows, so Cairn does not "
    "recommend a change.",
    "fixes": [
        {
            "label": "Open Smart App Control",
            "note": None,
            "action": {"kind": "uri", "uri": "windowsdefender://smartapp/"},
        }
    ],
}
STARTUP_LIST_NOTE = "The startup list could not be read, so slow apps are not matched to startup entries."
FAST_STARTS_NOTE = (
    "Windows times only full starts, such as restarts: 5 Fast Startup starts since 2026-09-01 are not listed."
)
# Two items that slowed starts and a service that slowed a shutdown.
SLOW_BOTH_PHASES = (
    ("app", "Discord.exe", "Discord", "C:\\Apps\\Discord.exe", ("user_run:Discord",)),
    ("driver", "contoso_storage.sys", "Contoso Storage Driver", None, ()),
    ("service", "Fabrikam Sync", "Fabrikam Sync Service", None, (), "shutdown"),
)
START_TYPES_ERROR = "Could not read the start types: the System log could not be opened"
SHUTDOWNS_ERROR = "Could not read the unexpected shutdowns: the System log could not be opened"


def status(app: App) -> tuple[str, str]:
    return str(app.status_message.cget("text")), str(app.status_message.cget("text_color"))


def job_status(app: App) -> str:
    return str(app.status_tool.cget("text"))


def checkup_loaded(app: App) -> bool:
    return app.security_panel.loaded and not app._checkup_loading and idle(app)


def open_security(app: App) -> SecurityPanel:
    """Shows Security once the startup scan is done and waits for the checkup."""
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, SECURITY_SECTION)
    pump(app, 5.0, until=lambda: checkup_loaded(app))
    return app.security_panel


def boot_loaded(app: App) -> bool:
    return app.boot_panel.loaded and not app._boot_loading and idle(app)


def open_boot(app: App) -> BootPanel:
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, BOOT_SECTION)
    pump(app, 5.0, until=lambda: boot_loaded(app))
    return app.boot_panel


def button(row: CheckRow, label: str) -> Any:
    return next(b for fix, b in row.fix_buttons if fix["label"] == label)


def fixes_recorder(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    opened: list[str] = []
    monkeypatch.setattr(system, "open_uri", opened.append)
    return opened


def test_security_loads_only_when_shown_and_once_until_stale(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, health_update_scan_due=False)
    pump(app, 5.0, until=lambda: scanned(app))
    pump(app, 0.3)
    assert engine.calls_named("health_security_checkup") == []
    panel = open_security(app)
    assert engine.calls_named("health_security_checkup") == [()]
    assert len(panel.rows) == 24
    assert panel.value_label.cget("text") == "86"
    show_section(app, "Dashboard")
    show_section(app, SECURITY_SECTION)
    pump(app, 0.3)
    assert len(engine.calls_named("health_security_checkup")) == 1
    # An old checkup is read again on the next visit.
    assert app._checkup_at is not None
    app._checkup_at -= CHECKUP_STALE_S + 1
    show_section(app, "Dashboard")
    show_section(app, SECURITY_SECTION)
    pump(
        app,
        5.0,
        until=lambda: len(engine.calls_named("health_security_checkup")) == 2 and checkup_loaded(app),
    )
    # "Check again" reads it at once.
    panel.refresh_button.invoke()
    pump(
        app,
        5.0,
        until=lambda: len(engine.calls_named("health_security_checkup")) == 3 and checkup_loaded(app),
    )
    assert engine.calls_named("health_update_scan_start") == []
    assert app.errors == []


def test_blocks_sort_and_the_passed_list_collapses(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, health_update_scan_due=False)
    panel = open_security(app)
    assert panel.fix_heading.cget("text") == "To fix (3)"
    assert panel.unknown_heading.cget("text") == "Could not check (1)"
    assert panel.passed_heading.cget("text") == "Passed (20)"
    # Most severe first: encryption (medium) before the two low findings.
    fix_ids = [row.check_id for row in panel.fix_frame.winfo_children() if isinstance(row, CheckRow)]
    assert fix_ids == ["encryption", "remote_assistance", "file_extensions"]
    assert not panel.passed_frame.winfo_ismapped()
    assert panel.passed_button.cget("text") == "Show 20 passed checks"
    panel.passed_button.invoke()
    pump(app, 0.3)
    assert panel.passed_frame.winfo_ismapped()
    assert panel.passed_button.cget("text") == "Hide passed checks"
    passed = panel.rows["firewall"]
    assert passed.compact
    assert passed.summary_label.cget("text") == "On for every network type"
    panel.passed_button.invoke()
    pump(app, 0.2)
    assert not panel.passed_frame.winfo_ismapped()
    assert panel.grade_label.cget("text") == "⚠ Needs attention"
    assert app.errors == []


def test_an_informational_finding_keeps_its_chip_detail_and_fix(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    opened = fixes_recorder(monkeypatch)
    app, _ = make_app(
        elevated=True, health_update_scan_due=False, health_check_overrides={"smart_app_control": SAC_OFF}
    )
    panel = open_security(app)
    # It costs no points, so it is listed with the passed checks and not under "To fix".
    assert panel.fix_heading.cget("text") == "To fix (3)"
    assert panel.passed_heading.cget("text") == "Passed (20)"
    row = panel.rows["smart_app_control"]
    assert row.compact and row.informational
    panel.passed_button.invoke()
    # The smallest window the sections are laid out for.
    app.minsize(900, 560)
    resize_to(app, 1000, 600)
    assert row.icon_label.cget("text") == "–"
    assert row.chip_label is not None and row.chip_label.cget("text") == "Info"
    assert row.chip_label.winfo_ismapped()
    assert row.summary_label.cget("text") == "Off"
    assert row.detail_label is not None and row.detail_label.cget("text") == SAC_OFF["detail"]
    assert row.detail_label.winfo_ismapped()
    # The detail wraps inside the row.
    right = row.detail_label.winfo_rootx() + row.detail_label.winfo_reqwidth()
    assert right <= row.winfo_rootx() + row.winfo_width()
    button(row, "Open Smart App Control").invoke()
    assert opened == ["windowsdefender://smartapp/"]
    # A passed check stays one line: no chip, no detail, no fix.
    firewall = panel.rows["firewall"]
    assert not firewall.informational
    assert firewall.chip_label is None and firewall.detail_label is None
    assert firewall.fix_buttons == []
    # A finding under "To fix" carries the same parts in its card.
    card = panel.rows["file_extensions"]
    assert card.chip_label is not None and card.chip_label.cget("text") == "Optional"
    assert card.detail_label is not None
    assert app.errors == []


def test_a_due_search_starts_once_and_its_end_reloads_the_checkup(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_security(app)
    assert engine.calls_named("health_update_scan_start") == [(False,)]
    assert job_status(app) == SCAN_STATUS
    pending = panel.rows["pending_updates"]
    pump(
        app,
        1.0,
        until=lambda: str(pending.summary_label.cget("text")).startswith("Checking for waiting updates…"),
    )
    assert pending.stop_button is not None and pending.stop_button.winfo_ismapped()
    assert not button(pending, "Check online now").winfo_ismapped()
    engine.update_scan_finish([{"title": "2026-09 Security Update", "security": True}])
    pump(
        app,
        5.0,
        until=lambda: len(engine.calls_named("health_security_checkup")) == 2 and checkup_loaded(app),
    )
    pump(app, 0.5)
    # The reload after the search starts no second one.
    assert engine.calls_named("health_update_scan_start") == [(False,)]
    assert job_status(app) == ""
    pending = panel.rows["pending_updates"]
    assert pending.summary_label.cget("text") == "1 security update waiting"
    assert pending.check["state"] == "attention"
    assert button(pending, "Check online now").winfo_ismapped()
    # An offline search ends without a status line.
    assert "Windows Update check" not in status(app)[0]
    assert app.errors == []


def test_check_online_now_and_stop(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, health_update_scan_due=False)
    panel = open_security(app)
    pending = panel.rows["pending_updates"]
    button(pending, "Check online now").invoke()
    assert engine.calls_named("health_update_scan_start") == [(True,)]
    assert job_status(app) == SCAN_STATUS_ONLINE
    pump(app, 1.0, until=lambda: pending.stop_button is not None and pending.stop_button.winfo_ismapped())
    assert str(pending.summary_label.cget("text")).startswith("Checking Windows Update online…  ·  ")
    # A second start while one runs is refused.
    app._on_security_fix(pending.check, {"label": "x", "action": {"kind": "update_scan", "online": True}})
    assert status(app) == ("Windows Update is already being checked.", theme.WARNING)
    assert len(engine.calls_named("health_update_scan_start")) == 1
    assert pending.stop_button is not None
    pending.stop_button.invoke()
    assert engine.calls_named("health_update_scan_cancel") == [()]
    pump(
        app,
        5.0,
        until=lambda: len(engine.calls_named("health_security_checkup")) == 2 and checkup_loaded(app),
    )
    assert status(app) == ("The Windows Update check was stopped.", theme.WARNING)
    assert job_status(app) == ""
    assert panel.rows["pending_updates"].summary_label.cget("text") == "The check was stopped"
    assert app.errors == []


def test_a_stopped_search_is_not_started_again(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_security(app)
    assert engine.calls_named("health_update_scan_start") == [(False,)]
    pending = panel.rows["pending_updates"]
    pump(app, 1.0, until=lambda: pending.stop_button is not None and pending.stop_button.winfo_ismapped())
    assert pending.stop_button is not None
    pending.stop_button.invoke()
    pump(
        app,
        5.0,
        until=lambda: len(engine.calls_named("health_security_checkup")) == 2 and checkup_loaded(app),
    )
    # "Check again" may start the automatic search, but a stopped one counts as finished.
    panel.refresh_button.invoke()
    pump(
        app,
        5.0,
        until=lambda: len(engine.calls_named("health_security_checkup")) == 3 and checkup_loaded(app),
    )
    pump(app, 0.3)
    assert engine.calls_named("health_update_scan_start") == [(False,)]
    assert not app._update_scan_active
    assert panel.rows["pending_updates"].summary_label.cget("text") == "The check was stopped"
    assert app.errors == []


def test_a_change_during_a_checkup_keeps_its_automatic_search(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, SECURITY_SECTION)
    assert app._checkup_loading
    # A journaled change ends while the first checkup is being read: it is read again, and
    # the second read starts the search the first one was allowed to start.
    app._health_after_mutation()
    pump(
        app,
        5.0,
        until=lambda: len(engine.calls_named("health_security_checkup")) == 2 and checkup_loaded(app),
    )
    pump(app, 0.3)
    assert len(engine.calls_named("health_security_checkup")) == 2
    assert engine.calls_named("health_update_scan_start") == [(False,)]
    assert app._update_scan_active
    assert job_status(app) == SCAN_STATUS
    # A read that may not start a search (the one after a search ended) stays that way.
    other, other_engine = make_app(elevated=True)
    pump(other, 5.0, until=lambda: scanned(other))
    show_section(other, SECURITY_SECTION, run_hook=False)
    other.load_security(auto_scan=False)
    other._health_after_mutation()
    pump(
        other,
        5.0,
        until=lambda: len(other_engine.calls_named("health_security_checkup")) == 2 and checkup_loaded(other),
    )
    pump(other, 0.3)
    assert other_engine.calls_named("health_update_scan_start") == []
    assert app.errors == [] and other.errors == []


def test_an_online_search_reports_its_result(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, health_update_scan_due=False)
    panel = open_security(app)
    button(panel.rows["pending_updates"], "Check online now").invoke()
    engine.update_scan_finish([{"title": "Contoso driver update"}, {"title": "Fabrikam app update"}])
    pump(app, 5.0, until=lambda: status(app)[0].startswith("Windows Update check finished"))
    assert status(app) == ("Windows Update check finished: 2 updates waiting.", theme.GOOD)
    pump(app, 5.0, until=lambda: checkup_loaded(app))
    button(app.security_panel.rows["pending_updates"], "Check online now").invoke()
    engine.update_scan_finish([])
    pump(app, 5.0, until=lambda: status(app)[0] == "Windows Update check finished: nothing waiting.")
    pump(app, 5.0, until=lambda: checkup_loaded(app))
    button(app.security_panel.rows["pending_updates"], "Check online now").invoke()
    engine.update_scan_finish(error="Windows Update could not be reached; check the internet connection.")
    pump(app, 5.0, until=lambda: status(app)[0].startswith("Windows Update check failed"))
    assert status(app)[1] == theme.WARNING
    assert app.errors == []


def test_a_refused_start_is_a_warning(make_app: AppFactory) -> None:
    app, engine = make_app(
        elevated=True,
        health_update_scan_due=False,
        health_update_scan_error="The Windows Update service is disabled.",
    )
    panel = open_security(app)
    button(panel.rows["pending_updates"], "Check online now").invoke()
    assert status(app) == (
        "Could not check Windows Update: The Windows Update service is disabled.",
        theme.WARNING,
    )
    assert job_status(app) == ""
    assert not app._update_scan_active
    assert app.errors == []


def test_a_uri_fix_opens_the_page(make_app: AppFactory, monkeypatch: pytest.MonkeyPatch) -> None:
    opened = fixes_recorder(monkeypatch)
    app, _ = make_app(
        elevated=True, health_update_scan_due=False, health_check_overrides={"firewall": FIREWALL_ATTENTION}
    )
    panel = open_security(app)
    button(panel.rows["firewall"], "Open Firewall & network protection").invoke()
    assert opened == ["windowsdefender://network/"]
    # A page outside the allowlist is never opened.
    app._on_security_fix(
        panel.rows["firewall"].check,
        {"label": "Scan", "action": {"kind": "uri", "uri": "windowsdefender://quickscan/"}},
    )
    assert opened == ["windowsdefender://network/"]
    assert status(app)[1] == theme.WARNING
    assert app.errors == []


def test_tool_fixes(make_app: AppFactory) -> None:
    app, engine = make_app(
        elevated=False, health_update_scan_due=False, health_check_overrides={"uac": UAC_ATTENTION}
    )
    panel = open_security(app)
    button(panel.rows["uac"], "Open UAC settings").invoke()
    pump(app, 5.0, until=lambda: status(app)[0] == "Opened UAC settings.")
    assert engine.calls_named("tools_open_windows") == [("uac_settings",)]
    # A tool that needs administrator rights is not started without them.
    remote = panel.rows["remote_assistance"]
    assert "Opens only with administrator rights." in remote.notes
    button(remote, "Open Remote settings").invoke()
    pump(app, 0.3)
    assert engine.calls_named("tools_open_windows") == [("uac_settings",)]
    assert status(app) == (
        "Open Remote settings needs administrator rights; restart as administrator to open it.",
        theme.WARNING,
    )
    assert app.errors == []


def test_the_tweak_fix_applies_through_the_journaled_flow(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, health_update_scan_due=False)
    panel = open_security(app)
    button(panel.rows["file_extensions"], "Show file extensions").invoke()
    text = confirm_dialog_text(app)
    assert "File Extensions" in text
    pump(app, 5.0, until=lambda: bool(engine.calls_named("apply")))
    assert engine.calls_named("apply") == [(["interface.file_extensions"], "try", False)]
    # The change makes the checkup outdated; Security is visible, so it is read again.
    pump(
        app,
        5.0,
        until=lambda: len(engine.calls_named("health_security_checkup")) == 2 and checkup_loaded(app),
    )
    assert app.errors == []


def test_the_tweak_fix_waits_for_a_scan(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, health_update_scan_due=False)
    panel = open_security(app)
    app._last_scan = None
    button(panel.rows["file_extensions"], "Show file extensions").invoke()
    pump(app, 0.3)
    assert status(app) == ("Run a scan first: open Optimize to show file extensions.", theme.WARNING)
    assert app.current_section == "Optimize"
    assert dialogs(app) == []
    assert engine.calls_named("apply") == []
    assert app.errors == []


def test_per_user_tweak_fix_is_disabled_for_another_account(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, other_user=True, health_update_scan_due=False)
    panel = open_security(app)
    row = panel.rows["file_extensions"]
    tweak = button(row, "Show file extensions")
    assert tweak.cget("state") == "disabled"
    assert PER_USER_NOTE in row.notes
    app._on_security_fix(row.check, row.fix_buttons[0][0])
    assert status(app) == (PER_USER_NOTE, theme.WARNING)
    assert dialogs(app) == []
    assert engine.calls_named("apply") == []
    assert app.errors == []


def test_fixes_that_change_windows_wait_while_busy(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, health_update_scan_due=False)
    panel = open_security(app)
    tweak = button(panel.rows["file_extensions"], "Show file extensions")
    tool = button(panel.rows["remote_assistance"], "Open Remote settings")
    app._set_busy(True)
    assert tweak.cget("state") == "disabled"
    assert tool.cget("state") == "normal"
    app._set_busy(False)
    assert tweak.cget("state") == "normal"
    assert app.errors == []


def test_a_standard_user_sees_what_needs_administrator_rights(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False, health_update_scan_due=False)
    panel = open_security(app)
    encryption = panel.rows["encryption"]
    assert encryption.check["state"] == "unknown"
    assert encryption.summary_label.cget("text") == "Needs administrator rights to check"
    assert panel.note_row.winfo_ismapped()
    assert panel.elevate_button.winfo_ismapped()
    assert "Drive encryption can only be checked" in panel.note_label.cget("text")
    # The elevate fix offers the relaunch; the test never confirms it.
    button(encryption, "Restart as administrator").invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == ADMIN_DIALOG_TITLE
    pump(app, 0.3)
    dialog._cancel()
    # Boot history reads nothing without administrator rights.
    show_section(app, BOOT_SECTION)
    pump(app, 0.5)
    assert engine.calls_named("health_boot_history") == []
    boot = app.boot_panel
    assert boot.message_label.cget("text") == NEEDS_ADMIN_TEXT
    assert boot.message_button.winfo_ismapped()
    assert not boot.chart_card.winfo_ismapped()
    boot.message_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == ADMIN_DIALOG_TITLE
    pump(app, 0.3)
    dialog._cancel()
    assert app.errors == []


def test_a_copy_that_may_not_elevate_explains_instead_of_relaunching(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, _ = make_app(elevated=False, health_update_scan_due=False)
    app.can_elevate = False
    relaunched: list[bool] = []
    monkeypatch.setattr(app, "_relaunch_elevated", lambda: relaunched.append(True))
    panel = open_security(app)
    button(panel.rows["encryption"], "Restart as administrator").invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == ADMIN_UNAVAILABLE_TITLE
    pump(app, 0.3)
    dialog.confirm_button.invoke()
    show_section(app, BOOT_SECTION)
    pump(app, 0.5)
    app.boot_panel.message_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == ADMIN_UNAVAILABLE_TITLE
    pump(app, 0.3)
    dialog.confirm_button.invoke()
    pump(app, 0.3)
    assert dialogs(app) == []
    assert relaunched == []
    assert app.errors == []


def test_boot_history_shows_the_chart_and_views(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_boot(app)
    assert engine.calls_named("health_boot_history") == [(60,)]
    assert panel.chart_card.winfo_ismapped()
    pump(app, 0.3)
    assert panel.chart.shown_bars == 12
    assert str(panel.summary_label.cget("text")).startswith("Last full start ")
    assert panel.fast_label.winfo_ismapped()
    assert panel.fast_label.cget("text") == FAST_STARTUP_TEXT
    # Every bar is a full start, so the legend names no Fast Startup bar.
    legend = [str(label.cget("text")) for label in panel.legend.winfo_children()]
    assert "Fast Startup" not in legend
    assert panel.view == "slow"
    assert panel.view_selector.get() == "Slows startup (2)"
    assert len(panel.slow_cards) == 2
    panel.select_view("starts")
    pump(app, 0.2)
    assert panel.view_selector.get() == "Recent starts"
    panel.select_view("shutdowns")
    pump(app, 0.2)
    # The history is read once per visit until something changes.
    show_section(app, "Dashboard")
    show_section(app, BOOT_SECTION)
    pump(app, 0.3)
    assert len(engine.calls_named("health_boot_history")) == 1
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("health_boot_history")) == 2 and boot_loaded(app))
    assert app.errors == []


def test_turn_off_at_startup_from_the_boot_history(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_boot(app)
    discord = next(card for card in panel.slow_cards if card.item["kind"] == "app")
    assert discord.startup_button is not None
    # The button names the startup entry it turns off.
    assert discord.startup_button.cget("text") == "Turn off “Discord” at startup"
    discord.startup_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("health_boot_history")) == 2 and boot_loaded(app))
    assert engine.calls_named("startup_set_enabled") == [("user_run:Discord", False, "skip")]
    discord = next(card for card in app.boot_panel.slow_cards if card.item["kind"] == "app")
    assert discord.startup_button is not None
    assert discord.startup_button.cget("text") == ALREADY_OFF_TEXT
    assert discord.startup_button.cget("state") == "disabled"
    assert status(app)[0] == "Discord disabled at startup. Undo it from History."
    assert app.errors == []


def test_driver_rows_open_device_manager(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    # Device Manager is one of the engine's Windows tools; this fake's tool list is shorter.
    opened: list[str] = []
    engine.tools_open_windows = opened.append  # type: ignore[method-assign]
    panel = open_boot(app)
    driver = next(card for card in panel.slow_cards if card.item["kind"] == "driver")
    assert driver.startup_button is None
    assert driver.tool_button is not None
    assert driver.tool_button.cget("text") == "Open Device Manager"
    driver.tool_button.invoke()
    pump(app, 5.0, until=lambda: status(app)[0] == "Opened Device Manager.")
    assert opened == ["device_manager"]
    assert app.errors == []


def test_a_disabled_log_offers_event_viewer(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, health_boot_access="log_disabled")
    panel = open_boot(app)
    assert panel.message_button.winfo_ismapped()
    assert panel.message_button.cget("text") == "Open Event Viewer"
    panel.message_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("tools_open_windows")) and idle(app))
    assert engine.calls_named("tools_open_windows") == [("event_viewer",)]
    assert app.errors == []


def test_boot_history_shows_what_could_not_be_read(make_app: AppFactory) -> None:
    app, engine = make_app(
        elevated=True,
        health_update_scan_due=False,
        health_boot_notes=[STARTUP_LIST_NOTE],
        health_boot_errors=[START_TYPES_ERROR, SHUTDOWNS_ERROR],
    )
    panel = open_boot(app)
    pump(app, 0.3)
    assert panel.notice_label.winfo_ismapped()
    # Everything the engine reports at most: its note, then both errors.
    assert panel.notice_label.cget("text") == (
        f"⚠ {STARTUP_LIST_NOTE}\n⚠ {START_TYPES_ERROR}\n⚠ {SHUTDOWNS_ERROR}"
    )
    assert panel.notice_label.cget("text_color") == theme.WARNING
    assert panel.chart_card.winfo_ismapped()
    assert_section_fits(app, BOOT_SECTION)
    # A history without them takes the lines away again.
    engine.health_boot_notes = []
    engine.health_boot_errors = []
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("health_boot_history")) == 2 and boot_loaded(app))
    pump(app, 0.2)
    assert not panel.notice_label.winfo_ismapped()
    assert app.errors == []


def test_a_disabled_log_still_shows_what_could_not_be_read(make_app: AppFactory) -> None:
    app, _ = make_app(
        elevated=True,
        health_update_scan_due=False,
        health_boot_access="log_disabled",
        health_boot_errors=[START_TYPES_ERROR],
    )
    panel = open_boot(app)
    pump(app, 0.3)
    assert panel.notice_label.cget("text") == f"⚠ {START_TYPES_ERROR}"
    assert panel.notice_label.winfo_ismapped() and panel.message_button.winfo_ismapped()
    assert_section_fits(app, BOOT_SECTION)


def test_fast_startup_starts_that_are_not_listed_are_counted(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, health_update_scan_due=False, health_boot_notes=[FAST_STARTS_NOTE])
    panel = open_boot(app)
    pump(app, 0.3)
    assert panel.fast_label.winfo_ismapped()
    assert panel.notice_label.winfo_ismapped()
    assert panel.notice_label.cget("text") == f"⚠ {FAST_STARTS_NOTE}"
    assert_section_fits(app, BOOT_SECTION)
    assert app.errors == []


def test_items_that_slowed_shutdowns_are_listed_with_the_shutdowns(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, health_update_scan_due=False, health_boot_slow=SLOW_BOTH_PHASES)
    panel = open_boot(app)
    assert panel.view_selector.get() == "Slows startup (2)"
    starting = [card for card in panel.slow_cards if card.item["phase"] == "startup"]
    stopping = [card for card in panel.slow_cards if card.item["phase"] == "shutdown"]
    assert [card.item["title"] for card in starting] == ["Discord", "Contoso Storage Driver"]
    assert [card.item["title"] for card in stopping] == ["Fabrikam Sync Service"]
    pump(app, 0.2)
    assert all(card.winfo_viewable() for card in starting)
    assert not stopping[0].winfo_viewable()
    panel.select_view("shutdowns")
    pump(app, 0.2)
    assert panel.view_selector.get() == "Shutdowns"
    assert stopping[0].winfo_viewable()
    assert not any(card.winfo_viewable() for card in starting)
    # A service offers Services; only startup items can be turned off at startup.
    assert stopping[0].tool_button is not None and stopping[0].tool_button.cget("text") == "Open Services"
    assert stopping[0].startup_button is None
    # The selector keeps counting the startup items only.
    panel.select_view("slow")
    pump(app, 0.2)
    assert panel.view_selector.get() == "Slows startup (2)"
    assert_section_fits(app, BOOT_SECTION)
    assert app.errors == []


def test_boot_history_errors_and_missing_logs(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, health_boot_access="log_missing")
    panel = open_boot(app)
    assert (
        panel.summary_label.cget("text") == "This copy of Windows does not keep startup performance records."
    )
    assert not panel.chart_card.winfo_ismapped()
    other, _ = make_app(elevated=True, health_boot_failure="the log is corrupt")
    pump(other, 5.0, until=lambda: scanned(other))
    show_section(other, BOOT_SECTION)
    pump(other, 5.0, until=lambda: not other._boot_loading and idle(other))
    assert (
        other.boot_panel.message_label.cget("text") == "Could not read the boot history: the log is corrupt"
    )
    assert app.errors == [] and other.errors == []


def test_a_failed_checkup_is_shown(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, health_failure="WMI is broken")
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, SECURITY_SECTION)
    pump(app, 5.0, until=lambda: not app._checkup_loading and idle(app))
    assert app.security_panel.sub_label.cget("text") == "Could not check Windows security: WMI is broken"
    assert app.errors == []


@pytest.mark.parametrize("section", [SECURITY_SECTION, BOOT_SECTION])
def test_an_outdated_engine_shows_the_out_of_date_text(make_app: AppFactory, section: str) -> None:
    app, engine = make_app(
        elevated=True,
        unsupported=["health_security_checkup", "health_boot_history", "health_update_scan_start"],
    )
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, section)
    pump(app, 0.3)
    if section == SECURITY_SECTION:
        assert app.security_panel.sub_label.cget("text") == f"⚠ {UNSUPPORTED_TEXT}"
    else:
        assert app.boot_panel.summary_label.cget("text") == f"⚠ {UNSUPPORTED_TEXT}"
    assert engine.calls_named("health_security_checkup") == []
    assert engine.calls_named("health_boot_history") == []
    assert app.errors == []


def test_closing_while_a_search_runs_asks_nothing_and_stops_it(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_security(app)
    assert app._update_scan_active
    app._request_close()
    assert engine.calls_named("health_update_scan_cancel") == [()]
    assert not app._running
    assert app.errors == []


def test_polling_slows_down_while_security_is_hidden(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    open_security(app)
    show_section(app, "Dashboard")
    before = len(engine.calls_named("health_update_scan"))
    started = time.perf_counter()
    pump(app, 1.2)
    elapsed = time.perf_counter() - started
    hidden = len(engine.calls_named("health_update_scan")) - before
    # The read that was already due, then one per hidden interval; a busy PC can keep the
    # event loop longer than asked, so the bound follows the time that really passed.
    assert hidden <= 1 + int(elapsed / UPDATE_POLL_HIDDEN_S)
    engine.update_scan_finish([])
    pump(app, 3.0, until=lambda: not app._update_scan_active)
    # Security is hidden: the checkup is read on the next visit, not now.
    assert len(engine.calls_named("health_security_checkup")) == 1
    show_section(app, SECURITY_SECTION)
    pump(
        app,
        5.0,
        until=lambda: len(engine.calls_named("health_security_checkup")) == 2 and checkup_loaded(app),
    )
    assert engine.calls_named("health_update_scan_start") == [(False,)]
    assert app.errors == []


def test_security_fits_the_window(make_app: AppFactory) -> None:
    # A standard user: the header carries the notes and "Restart as administrator".
    app, _ = make_app(elevated=False, health_update_scan_due=False)
    open_security(app)
    assert_section_fits(app, SECURITY_SECTION)


def test_boot_history_needing_administrator_rights_fits_the_window(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=False, health_update_scan_due=False)
    show_section(app, BOOT_SECTION)
    pump(app, 0.3)
    assert_section_fits(app, BOOT_SECTION)


def test_boot_history_fits_the_window(make_app: AppFactory) -> None:
    # An administrator: the chart and the bottom card with its views.
    app, _ = make_app(elevated=True, health_update_scan_due=False)
    open_boot(app)
    assert_section_fits(app, BOOT_SECTION)


def test_an_engine_without_the_search_still_shows_the_checkup(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, unsupported=["health_update_scan_start"])
    panel = open_security(app)
    assert panel.loaded
    assert not app._update_scan_active
    button(panel.rows["pending_updates"], "Check online now").invoke()
    assert status(app) == (UNSUPPORTED_TEXT, theme.WARNING)
    assert job_status(app) == ""
    assert app.errors == []
