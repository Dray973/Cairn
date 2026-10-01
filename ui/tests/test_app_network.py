"""Network section integration tests: the real window with the in-memory FakeEngine.

Nothing reaches the system: adapters, DNS servers and the reset live inside the fake.
Administrator dialogs are cancelled, never confirmed.
"""

from __future__ import annotations

from typing import Any

import pytest

from optimizer.app import BUSY_CLOSE_TITLE, CLOSE_IRREVERSIBLE, CLOSE_TEXTS
from optimizer.features.network import (
    DNS_CONFIRM_MESSAGE,
    FLUSHED_TEXT,
    OUTDATED_TEXT,
    RENEW_MESSAGE,
    RESET_ACKNOWLEDGE,
    RESET_MESSAGE,
)
from optimizer.widgets.network import (
    ADMIN_NOTE,
    CUSTOM_HINT,
    POLICY_WARNING,
    RESET_CAVEAT,
    VPN_WARNING,
    CustomDnsDialog,
    NetworkPanel,
)

from .app_support import (
    App,
    AppFactory,
    MessageDialog,
    confirm_dialog,
    confirm_dialog_text,
    ctk,
    dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    scanned,
    show_section,
    theme,
)
from .fake_network import (
    DEFAULT_SWITCH,
    DNS_PRESETS,
    ETHERNET,
    NOTE_PROFILE,
    NOTE_VIRTUAL,
    NOTE_VPN_ADAPTER,
    NOTE_VPN_CONNECTED,
    PROTECTION_OFF,
    PROTON_VPN,
    WIFI,
    WIFI_DIRECT,
)

MENU_VALUES = [p["title"] for p in DNS_PRESETS] + ["Custom…"]
SWITCH_MANUAL = "vEthernet (Default Switch): static IPv4 172.28.64.1/20"


def open_network(app: App) -> NetworkPanel:
    """Waits for the first scan, shows the Network section and waits until the adapters are listed."""
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Network")
    pump(app, 5.0, until=lambda: app.network_panel.loaded and listed(app))
    return app.network_panel


def listed(app: App) -> bool:
    """No adapter read is running or waiting, and nothing else runs."""
    return idle(app) and not app._network_loading


def reads(engine: Any) -> int:
    return len(engine.calls_named("network_list"))


def status(app: App) -> tuple[str, str]:
    return str(app.status_message.cget("text")), str(app.status_message.cget("text_color"))


def record_statuses(app: App) -> list[str]:
    """Records every status bar message from now on; a later rescan replaces the last one."""
    shown: list[str] = []
    set_status = app.set_status

    def record(text: str, color: str = theme.INK_SECONDARY) -> None:
        shown.append(text)
        set_status(text, color)

    app.set_status = record  # type: ignore[method-assign]
    return shown


def cancel(app: App, dialog: MessageDialog | CustomDnsDialog) -> None:
    """Cancels `dialog` once its window has finished appearing."""
    pump(app, 0.3)
    dialog._cancel()
    pump(app, 0.2)


def custom_dialogs(app: App) -> list[CustomDnsDialog]:
    return [w for w in app.winfo_children() if isinstance(w, CustomDnsDialog) and w.winfo_exists()]


def next_custom_dialog(app: App) -> CustomDnsDialog:
    pump(app, 5.0, until=lambda: bool(custom_dialogs(app)))
    return custom_dialogs(app)[-1]


def enter(dialog: CustomDnsDialog, entry: ctk.CTkEntry, text: str) -> None:
    """Replaces the entry's text and runs the validation its key bindings run."""
    entry.delete(0, "end")
    entry.insert(0, text)
    dialog._validate()


def dry_runs(engine: Any) -> list[tuple[Any, ...]]:
    return [c for c in engine.calls_named("network_set_dns") if c[-1] is True]


def changes(engine: Any) -> list[tuple[Any, ...]]:
    return [c for c in engine.calls_named("network_set_dns") if c[-1] is False]


def test_network_tab_loads_on_visit(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    assert reads(engine) == 0, "the adapters are read only when the section is shown"
    panel = open_network(app)
    assert engine.calls_named("network_dns_presets") == [()]
    assert panel.row_count == 4, "the idle Wi-Fi Direct adapter is hidden"
    assert [r.adapter_id for r in panel.rows] == [WIFI, ETHERNET, DEFAULT_SWITCH, PROTON_VPN]
    assert panel.summary.cget("text") == (
        "5 adapters  ·  2 connected  ·  1 idle virtual adapter hidden  ·  read in 18 ms"
    )
    assert panel.warnings == [] and not panel.warning_label.winfo_manager()
    assert not panel.admin_label.winfo_manager()
    assert panel.caveat_label.cget("text") == RESET_CAVEAT
    assert panel.reset_button.cget("text") == "Reset network stack…"
    assert panel.reset_button.cget("fg_color") == theme.CRITICAL
    for button in (panel.refresh_button, panel.flush_button, panel.reset_button):
        assert button.cget("state") == "normal"

    wifi = panel.row(WIFI.upper().strip("{}"))
    assert wifi is not None and wifi is panel.row(WIFI), "rows are found by any spelling of the GUID"
    assert wifi.primary_label is not None and wifi.primary_label.cget("text") == "Primary connection"
    assert wifi.status_label.cget("text") == "✓ Connected"
    assert wifi.dns_menu.cget("values") == MENU_VALUES
    assert wifi.dns_menu.get() == "Automatic (DHCP)"
    assert wifi.dns_menu.cget("state") == "normal"
    assert wifi.dns_label.cget("text") == "IPv4 DNS: automatic (192.168.0.1)"
    assert wifi.renew_shown and wifi.renew_button.cget("state") == "normal"
    assert not wifi.undo_shown
    assert not wifi.hint_label.winfo_manager()
    ethernet = panel.row(ETHERNET)
    assert ethernet is not None and not ethernet.renew_shown and ethernet.primary_label is None
    assert ethernet.status_label.cget("text") == "○ Disconnected"

    panel.show_all_switch.toggle()
    assert panel.row_count == 5 and panel.row(WIFI_DIRECT) is not None
    assert panel.summary.cget("text") == "5 adapters  ·  2 connected  ·  read in 18 ms"
    assert reads(engine) == 1, "Show all filters the last read"

    # Every visit reads the adapters again; the preset table is read once.
    show_section(app, "Dashboard")
    show_section(app, "Network")
    pump(app, 5.0, until=lambda: reads(engine) == 2 and listed(app))
    assert panel.row_count == 5, "Show all stays on"
    panel.refresh_button.invoke()
    assert panel.refresh_button.cget("state") == "disabled", "a read is running"
    pump(app, 5.0, until=lambda: reads(engine) == 3 and listed(app))
    assert panel.refresh_button.cget("state") == "normal"
    assert engine.calls_named("network_dns_presets") == [()]
    assert app.errors == []


def test_tab_opened_while_busy_loads_once_the_worker_is_free(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, delay=0.4)
    # The automatic scan runs; the first visit to the section happens meanwhile.
    pump(app, 5.0, until=lambda: app._busy)
    show_section(app, "Network")
    panel = app.network_panel
    assert panel.summary.cget("text") == "Reading network adapters…"
    pump(app, 10.0, until=lambda: scanned(app) and panel.loaded and listed(app))
    assert reads(engine) == 1
    assert panel.row_count == 4
    assert all(r.dns_menu.cget("state") == "normal" for r in panel.rows if r.adapter.get("can_change_dns"))
    assert app.errors == []


def test_reload_requested_during_a_read_runs_once_after_it(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, delay=0.2)
    panel = open_network(app)
    assert reads(engine) == 1
    app.load_network()
    app.load_network()
    app.load_network()
    pump(app, 5.0, until=lambda: reads(engine) == 2)
    pump(app, 5.0, until=lambda: listed(app))
    pump(app, 0.5)
    assert reads(engine) == 3, "the reloads asked for during a read are merged into one"
    assert panel.row_count == 4
    assert app.errors == []


def test_dns_preset_confirms_then_applies(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    pump(app, 5.0, until=lambda: app.status_journal.cget("text") == "Journal: no changes")
    statuses = record_statuses(app)
    row = panel.row(WIFI)
    assert row is not None
    row.dns_menu._dropdown_callback("Cloudflare")
    dialog = next_dialog(app)
    assert dialog.title_text == "Change DNS servers of Wi-Fi?"
    assert dialog.confirm_button.cget("text") == "Change DNS"
    text = dialog_text(dialog)
    assert DNS_CONFIRM_MESSAGE in text
    assert "• IPv4: automatic (192.168.0.1) → 1.1.1.1, 1.0.0.1" in text
    assert "• IPv6: not changed: IPv6 is turned off on this adapter" in text
    assert row.dns_menu.get() == "Cloudflare"
    assert engine.calls_named("network_set_dns") == [(WIFI, "cloudflare", None, None, "skip", True)]
    assert changes(engine) == []

    dialog.confirm_button.invoke()
    # Until the engine answers, the row says what runs and every action waits.
    assert row.busy_label.cget("text") == "Changing DNS…"
    assert row.busy_label.winfo_manager() == "grid"
    assert all(r.dns_menu.cget("state") == "disabled" for r in panel.rows)
    assert row.renew_button.cget("state") == "disabled"
    for button in (panel.refresh_button, panel.flush_button, panel.reset_button):
        assert button.cget("state") == "disabled"
    pump(app, 5.0, until=lambda: bool(changes(engine)))
    assert changes(engine) == [(WIFI, "cloudflare", None, None, "skip", False)]
    assert "Changing the DNS servers of Wi-Fi…" in statuses
    pump(app, 5.0, until=lambda: reads(engine) == 2 and listed(app))
    assert status(app) == ("Done: Wi-Fi now uses Cloudflare DNS. Undo it from History.", theme.GOOD)
    wifi = panel.row(WIFI)
    assert wifi is not None and wifi is not row, "the list was read again"
    assert wifi.dns_menu.get() == "Cloudflare"
    assert wifi.dns_label.cget("text") == "IPv4 DNS: 1.1.1.1, 1.0.0.1 (Cloudflare)"
    assert wifi.undo_shown and wifi.undo_button.cget("state") == "normal"
    pump(app, 5.0, until=lambda: app.status_journal.cget("text") == "Journal: 1 revertible change")

    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 1 and idle(app))
    change = app.history_panel.rows[0].change
    assert change.title == "DNS servers: Wi-Fi"
    assert change.kind == "dns"
    assert change.details == ["IPv4 DNS  ·  was automatic"]
    assert change.filter["dns"] == [WIFI]
    assert dialogs(app) == []
    assert app.errors == []


def test_dns_change_cancel_changes_nothing_and_resets_menu(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    row = panel.row(WIFI)
    assert row is not None
    row.choose("google")
    dialog = next_dialog(app)
    assert "• IPv4: automatic (192.168.0.1) → 8.8.8.8, 8.8.4.4" in dialog_text(dialog)
    assert row.dns_menu.get() == "Google Public DNS"
    cancel(app, dialog)
    assert row.dns_menu.get() == "Automatic (DHCP)"
    assert dry_runs(engine) == [(WIFI, "google", None, None, "skip", True)]
    assert changes(engine) == []
    assert engine._dns_active_count() == 0
    assert reads(engine) == 1, "nothing changed, so nothing is read again"
    assert idle(app)
    assert row.dns_menu.cget("state") == "normal"
    assert panel.flush_button.cget("state") == "normal"
    assert app.errors == []


def test_custom_dns_dialog_validates_input(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)

    # Wi-Fi has IPv6 turned off, so only IPv4 servers can be entered; cancelling changes nothing.
    wifi = panel.row(WIFI)
    assert wifi is not None
    wifi.dns_menu._dropdown_callback("Custom…")
    dialog = next_custom_dialog(app)
    assert dialog.title_text == "Custom DNS servers for Wi-Fi"
    assert dialog.ipv4_entry.cget("state") == "normal"
    assert dialog.ipv6_entry.cget("state") == "disabled"
    assert dialog.ipv6_entry._entry.get() == "IPv6 is turned off on this adapter", "the field says why"
    assert dialog.ipv6_entry.get() == ""
    assert wifi.dns_menu.get() == "Custom…"
    cancel(app, dialog)
    assert wifi.dns_menu.get() == "Automatic (DHCP)"
    assert engine.calls_named("network_set_dns") == []

    ethernet = panel.row(ETHERNET)
    assert ethernet is not None
    ethernet.choose("custom")
    dialog = next_custom_dialog(app)
    labels = [str(w.cget("text")) for w in dialog.winfo_children() if isinstance(w, ctk.CTkLabel)]
    assert CUSTOM_HINT in labels
    assert "Current settings: IPv4 DNS: automatic  ·  IPv6 DNS: automatic" in labels
    for entry in (dialog.ipv4_entry, dialog.ipv6_entry):
        assert "<KeyRelease>" in entry._entry.bind(), "input is checked as it is typed"
    assert dialog.continue_button.cget("state") == "disabled", "both fields are empty"
    assert dialog.error_label.cget("text") == ""

    enter(dialog, dialog.ipv4_entry, "1.1.1")
    assert dialog.error_label.cget("text") == "⚠ 1.1.1 is not a valid IPv4 address"
    assert dialog.continue_button.cget("state") == "disabled"
    enter(dialog, dialog.ipv4_entry, "192.168.1.53, 192.168.1.53 1.1.1.1")
    assert dialog.error_label.cget("text") == ""
    assert dialog.continue_button.cget("state") == "normal"
    enter(dialog, dialog.ipv6_entry, "fe80::1")
    assert "link-local" in dialog.error_label.cget("text")
    assert dialog.continue_button.cget("state") == "disabled"
    dialog.continue_button.invoke()
    assert dialog.winfo_exists(), "invalid input is never submitted"
    enter(dialog, dialog.ipv6_entry, "1.1.1.1")
    assert dialog.error_label.cget("text") == "⚠ 1.1.1.1 is not a valid IPv6 address"
    enter(dialog, dialog.ipv4_entry, "1.1.1.1 1.0.0.1 8.8.8.8 8.8.4.4 9.9.9.9")
    assert dialog.error_label.cget("text") == "⚠ at most 4 IPv4 DNS servers"
    enter(dialog, dialog.ipv4_entry, "192.168.1.53, 1.1.1.1")
    enter(dialog, dialog.ipv6_entry, "2001:DB8::53")
    assert dialog.error_label.cget("text") == ""
    assert dialog.continue_button.cget("state") == "normal"
    assert engine.calls_named("network_set_dns") == []

    pump(app, 0.3)
    dialog.continue_button.invoke()
    confirm = next_dialog(app)
    servers = (ETHERNET, "custom", ["192.168.1.53", "1.1.1.1"], ["2001:db8::53"], "skip")
    assert dry_runs(engine) == [(*servers, True)]
    text = dialog_text(confirm)
    assert confirm.title_text == "Change DNS servers of Ethernet?"
    assert "• IPv4: automatic → 192.168.1.53, 1.1.1.1" in text
    assert "• IPv6: automatic → 2001:db8::53" in text
    confirm.confirm_button.invoke()
    pump(app, 5.0, until=lambda: bool(changes(engine)) and reads(engine) == 2 and listed(app))
    assert changes(engine) == [(*servers, False)]
    assert status(app) == ("Done: Ethernet now uses Custom DNS. Undo it from History.", theme.GOOD)
    ethernet = panel.row(ETHERNET)
    assert ethernet is not None and ethernet.dns_menu.get() == "Custom"
    assert ethernet.undo_shown
    assert app.errors == []


def test_already_set_shows_status_without_dialog(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    wifi = panel.row(WIFI)
    assert wifi is not None
    wifi.choose("automatic")
    pump(app, 5.0, until=lambda: len(dry_runs(engine)) == 1 and idle(app))
    pump(app, 0.2)
    assert dialogs(app) == []
    assert status(app) == ("Nothing to change: Wi-Fi already uses these DNS servers.", theme.INK_SECONDARY)
    assert wifi.dns_menu.get() == "Automatic (DHCP)"

    # Automatic IPv4 DNS is skipped on an adapter with a manual address; the reason is named.
    switch = panel.row(DEFAULT_SWITCH)
    assert switch is not None
    switch.choose("automatic")
    pump(app, 5.0, until=lambda: len(dry_runs(engine)) == 2 and idle(app))
    pump(app, 0.2)
    assert dialogs(app) == []
    assert status(app) == (
        "Nothing to change on vEthernet (Default Switch): this adapter has a manually set IPv4 "
        "address, so it would get no IPv4 DNS servers automatically.",
        theme.INK_SECONDARY,
    )
    assert changes(engine) == []
    assert reads(engine) == 1
    assert app.errors == []


def test_a_skipped_family_is_named_in_the_result(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    switch = panel.row(DEFAULT_SWITCH)
    assert switch is not None
    switch.choose("cloudflare")
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: len(changes(engine)) == 1 and reads(engine) == 2 and listed(app))
    assert status(app) == (
        "Done: vEthernet (Default Switch) now uses Cloudflare DNS. Undo it from History.",
        theme.GOOD,
    )

    # Automatic DNS can reach only IPv6: IPv4 keeps Cloudflare on this static address.
    switch = panel.row(DEFAULT_SWITCH)
    assert switch is not None
    switch.choose("automatic")
    dialog = next_dialog(app)
    assert "• IPv4: not changed: this adapter has a manually set IPv4 address" in dialog_text(dialog)
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: len(changes(engine)) == 2 and reads(engine) == 3 and listed(app))
    assert status(app) == (
        "Done: vEthernet (Default Switch) now uses Cloudflare (IPv4 only) DNS; its IPv4 DNS servers were "
        "not changed. Undo it from History.",
        theme.WARNING,
    )
    switch = panel.row(DEFAULT_SWITCH)
    assert switch is not None and switch.dns_menu.get() == "Cloudflare (IPv4 only)"
    pump(app, 0.2)
    assert dialogs(app) == []
    assert app.errors == []


def test_servers_set_elsewhere_before_the_write_report_nothing_to_change(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    wifi = panel.row(WIFI)
    assert wifi is not None
    wifi.choose("cloudflare")
    dialog = next_dialog(app)
    assert "• IPv4: automatic (192.168.0.1) → 1.1.1.1, 1.0.0.1" in dialog_text(dialog)
    # The same servers are set outside Cairn while the confirmation is open.
    engine._static_dns[(WIFI, "ipv4")] = ["1.1.1.1", "1.0.0.1"]
    pump(app, 0.3)
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: bool(changes(engine)) and reads(engine) == 2 and listed(app))
    assert status(app) == ("Nothing to change: Wi-Fi already uses these DNS servers.", theme.INK_SECONDARY)
    assert engine._dns_active_count() == 0, "nothing was written, so nothing was recorded"
    wifi = panel.row(WIFI)
    assert wifi is not None and not wifi.undo_shown
    assert wifi.dns_menu.get() == "Cloudflare"
    pump(app, 0.2)
    assert dialogs(app) == []
    assert app.errors == []


def test_vpn_adapter_menu_is_disabled_with_note(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    vpn = panel.row(PROTON_VPN)
    assert vpn is not None
    assert vpn.dns_menu.cget("state") == "disabled"
    assert vpn.hint_label.winfo_manager() == "grid"
    assert vpn.hint_label.cget("text") == NOTE_VPN_ADAPTER
    assert not vpn.renew_shown
    switch = panel.row(DEFAULT_SWITCH)
    assert switch is not None
    assert switch.dns_menu.cget("state") == "normal", "virtual adapters can be changed"
    assert switch.hint_label.cget("text") == NOTE_VIRTUAL
    assert panel.warnings == [], "a disconnected VPN pauses nothing"

    # The engine refuses the adapter as well.
    vpn.choose("cloudflare")
    dialog = next_dialog(app)
    assert dialog.title_text == "Could not plan the DNS change"
    assert NOTE_VPN_ADAPTER in dialog_text(dialog)
    assert status(app) == (f"Could not plan the DNS change: {NOTE_VPN_ADAPTER}", theme.CRITICAL)
    confirm_dialog(app)
    assert vpn.dns_menu.get() == "Automatic (DHCP)"
    assert changes(engine) == []
    assert app.errors == []


def test_connected_vpn_disables_every_dns_menu(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, vpn_connected=True, dns_policy=["10.0.0.53"])
    panel = open_network(app)
    assert panel.warning_label.winfo_manager() == "grid"
    assert panel.warning_label.cget("text") == "\n".join(
        [POLICY_WARNING.format(servers="10.0.0.53"), VPN_WARNING]
    )
    assert panel.warning_label.cget("text_color") == theme.WARNING
    assert all(row.dns_menu.cget("state") == "disabled" for row in panel.rows)
    wifi, vpn = panel.row(WIFI), panel.row(PROTON_VPN)
    assert wifi is not None and vpn is not None
    assert wifi.hint_label.cget("text") == NOTE_VPN_CONNECTED
    assert vpn.hint_label.cget("text") == NOTE_VPN_ADAPTER
    assert vpn.status_label.cget("text") == "✓ Connected"
    # Only DNS changes pause: the cache, the lease and the reset stay available.
    assert wifi.renew_button.cget("state") == "normal"
    assert panel.flush_button.cget("state") == "normal"
    assert panel.reset_button.cget("state") == "normal"
    assert app.errors == []


def test_profile_dns_adapter_is_not_changeable(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, profile_dns={WIFI: ["9.9.9.9"]})
    panel = open_network(app)
    wifi = panel.row(WIFI)
    assert wifi is not None
    assert wifi.dns_menu.cget("state") == "disabled"
    assert wifi.hint_label.cget("text") == NOTE_PROFILE
    assert wifi.dns_label.cget("text") == "IPv4 DNS: Set for this Wi-Fi network: 9.9.9.9"
    assert wifi.dns_menu.get() == "Custom"
    ethernet = panel.row(ETHERNET)
    assert ethernet is not None and ethernet.dns_menu.cget("state") == "normal"
    assert panel.warnings == []
    assert app.errors == []


def test_standard_user_can_flush_but_not_change(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    # A change recorded earlier from an elevated window.
    engine.elevated = True
    engine.network_set_dns(WIFI, "cloudflare")
    engine.elevated = False
    panel = open_network(app)
    assert panel.admin_label.winfo_manager() == "grid"
    assert panel.admin_label.cget("text") == ADMIN_NOTE
    assert all(row.dns_menu.cget("state") == "disabled" for row in panel.rows)
    wifi = panel.row(WIFI)
    assert wifi is not None
    assert wifi.renew_shown and wifi.renew_button.cget("state") == "disabled"
    assert wifi.undo_shown and wifi.undo_button.cget("state") == "disabled"
    assert not wifi.hint_label.winfo_manager(), "the administrator note is shown once, above the list"
    assert panel.reset_button.cget("state") == "disabled"
    assert panel.flush_button.cget("state") == "normal"
    assert panel.refresh_button.cget("state") == "normal"

    statuses = record_statuses(app)
    panel.flush_button.invoke()
    assert panel.flush_button.cget("state") == "disabled", "the flush is running"
    pump(app, 5.0, until=lambda: bool(engine.calls_named("network_flush_dns")) and idle(app))
    pump(app, 0.2)
    assert engine.calls_named("network_flush_dns") == [()]
    assert "Flushing the DNS cache…" in statuses
    assert status(app) == (FLUSHED_TEXT, theme.GOOD)
    assert dialogs(app) == []
    assert panel.flush_button.cget("state") == "normal"

    # Changes stop at the administrator dialog, before any engine call.
    calls = len(engine.calls)
    assert wifi.dns_menu.get() == "Cloudflare"
    wifi.choose("google")
    dialog = next_dialog(app)
    assert dialog.title_text == "Administrator rights needed"
    cancel(app, dialog)
    assert wifi.dns_menu.get() == "Cloudflare", "the menu shows the current servers again"
    wifi.choose("custom")
    dialog = next_dialog(app)
    assert dialog.title_text == "Administrator rights needed"
    cancel(app, dialog)
    assert custom_dialogs(app) == []
    app._on_network_reset()
    cancel(app, next_dialog(app))
    app._on_undo_dns(wifi.adapter)
    cancel(app, next_dialog(app))
    assert engine.calls[calls:] == []
    assert app.errors == []


def test_renew_confirms_and_reloads(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    wifi = panel.row(WIFI)
    assert wifi is not None
    wifi.renew_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Renew the IP lease of Wi-Fi?"
    assert RENEW_MESSAGE in dialog_text(dialog)
    assert dialog.confirm_button.cget("text") == "Renew lease"
    cancel(app, dialog)
    assert engine.calls_named("network_renew_dhcp") == []

    statuses = record_statuses(app)
    wifi.renew_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("network_renew_dhcp")))
    pump(app, 5.0, until=lambda: reads(engine) == 2 and listed(app))
    assert engine.calls_named("network_renew_dhcp") == [(WIFI, False)]
    assert "Renewing the IP lease of Wi-Fi…" in statuses
    assert status(app) == ("Wi-Fi renewed its lease: 192.168.0.23.", theme.GOOD)
    assert panel.row(WIFI) is not wifi, "the list was read again"
    assert dialogs(app) == []
    assert app.errors == []


@pytest.mark.parametrize(
    ("options", "title", "message", "final_status", "step_line"),
    [
        (
            {},
            "Restart Windows to finish",
            "The network stack was reset.",
            ("Network stack reset. Restart Windows to finish.", theme.WARNING),
            "• Reset TCP/IP for IPv6: done",
        ),
        (
            {"reset_partial": {"ipv4"}},
            "Restart Windows to finish",
            "Some settings could not be reset; restart Windows to finish. Details are below.",
            ("Network stack reset with some errors. Restart Windows to finish.", theme.WARNING),
            "• Reset TCP/IP for IPv4: finished with errors (exit code 1)",
        ),
        (
            {"reset_fail": {"ipv6"}},
            "Network reset partly failed",
            "Some reset steps failed or did not finish.",
            ("The network reset partly failed; restart Windows and check the details.", theme.CRITICAL),
            "• Reset TCP/IP for IPv6: failed (exit code 1)",
        ),
    ],
    ids=["succeeded", "completed_with_errors", "failed"],
)
def test_network_reset_requires_acknowledgement(
    make_app: AppFactory,
    options: dict[str, Any],
    title: str,
    message: str,
    final_status: tuple[str, str],
    step_line: str,
) -> None:
    app, engine = make_app(elevated=True, **options)
    panel = open_network(app)
    statuses = record_statuses(app)
    panel.reset_button.invoke()
    dialog = next_dialog(app)
    assert engine.calls_named("network_reset") == [("skip", True)]
    assert engine.calls_named("system_restore_enabled") == [()]
    assert dialog.title_text == "Reset the network stack?"
    text = dialog_text(dialog)
    assert RESET_MESSAGE + " A restore point is created first." in text
    assert "• Reset the Winsock catalog  (netsh winsock reset)" in text
    assert "• Reset TCP/IP for IPv6  (netsh int ipv6 reset)" in text
    assert f"• {SWITCH_MANUAL}" in text, "the manual settings that are lost are listed"
    assert dialog.acknowledge_box is not None
    assert dialog.acknowledge_box.cget("text") == RESET_ACKNOWLEDGE
    assert dialog.confirm_button.cget("text") == "Reset network stack"
    assert dialog.confirm_button.cget("fg_color") == theme.CRITICAL
    assert dialog.confirm_button.cget("state") == "disabled"

    pump(app, 0.3)
    dialog.confirm_button.invoke()
    dialog._confirm()
    pump(app, 0.2)
    assert dialog.winfo_exists(), "nothing runs until the acknowledgement is ticked"
    assert engine.calls_named("network_reset") == [("skip", True)]
    dialog.acknowledge_box.toggle()
    assert dialog.confirm_button.cget("state") == "normal"
    dialog.confirm_button.invoke()

    result = next_dialog(app)
    assert engine.calls_named("network_reset") == [("skip", True), ("try", False)]
    assert "Creating a restore point and resetting the network stack…" in statuses
    assert result.title_text == title
    text = dialog_text(result)
    assert message in text
    assert "Restore point #9 was created before the reset." in text
    assert step_line in text
    assert f"Re-enter: {SWITCH_MANUAL}" in text
    if options:
        assert "Output of " in text, "the output of a step with problems is repeated"
    else:
        assert "Output of " not in text
    pump(app, 5.0, until=lambda: reads(engine) == 2 and listed(app))
    assert status(app) == final_status
    confirm_dialog(app)
    assert app.errors == []


@pytest.mark.parametrize(
    ("restore_enabled", "restore_points", "sentence", "running", "outcome"),
    [
        (
            False,
            True,
            " No restore point can be created because System Protection is off.",
            "Resetting the network stack…",
            "No restore point was created.",
        ),
        (
            None,
            True,
            " Cairn couldn't check System Protection, so a restore point may not be created.",
            "Creating a restore point and resetting the network stack…",
            "Restore point #9 was created before the reset.",
        ),
        (
            True,
            False,
            " No restore point is created.",
            "Resetting the network stack…",
            "No restore point was created.",
        ),
    ],
    ids=["protection_off", "protection_unknown", "restore_points_not_wanted"],
)
def test_reset_dialog_says_when_system_protection_is_off(
    make_app: AppFactory,
    restore_enabled: bool | None,
    restore_points: bool,
    sentence: str,
    running: str,
    outcome: str,
) -> None:
    app, engine = make_app(elevated=True, restore_enabled=restore_enabled)
    assert app.engine is not None
    app.engine.restore_points = restore_points
    panel = open_network(app)
    statuses = record_statuses(app)
    panel.reset_button.invoke()
    dialog = next_dialog(app)
    assert RESET_MESSAGE + sentence in dialog_text(dialog)
    assert dialog.acknowledge_box is not None
    pump(app, 0.3)
    dialog.acknowledge_box.toggle()
    dialog.confirm_button.invoke()
    result = next_dialog(app)
    assert running in statuses
    policy = "try" if restore_points else "skip"
    assert engine.calls_named("network_reset")[-1] == (policy, False)
    text = dialog_text(result)
    assert outcome in text
    if restore_enabled is False:
        assert f"⚠ restore point unavailable: {PROTECTION_OFF}" in text
    pump(app, 5.0, until=lambda: reads(engine) == 2 and listed(app))
    confirm_dialog(app)
    assert app.errors == []


def test_network_list_error_is_shown_in_panel(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, network_error="cannot list the network adapters: access denied")
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Network")
    pump(app, 5.0, until=lambda: reads(engine) == 1 and listed(app))
    panel = app.network_panel
    assert panel.summary.cget("text") == (
        "Could not read the network adapters: cannot list the network adapters: access denied"
    )
    assert panel.summary.cget("text_color") == theme.CRITICAL
    assert panel.row_count == 0 and not panel.loaded
    assert panel.refresh_button.cget("state") == "normal"
    assert panel.flush_button.cget("state") == "normal"
    pump(app, 0.2)
    assert dialogs(app) == [], "a read error is shown in the panel, not in a dialog"

    engine.network_error = None
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: panel.loaded and listed(app))
    assert panel.row_count == 4
    assert panel.summary.cget("text_color") == theme.INK_MUTED
    assert app.errors == []


def test_preset_table_error_is_shown_in_panel(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)

    def unreadable() -> list[dict[str, Any]]:
        raise RuntimeError("the preset table is damaged")

    engine.network_dns_presets = unreadable  # type: ignore[method-assign]
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Network")
    pump(app, 0.3)
    panel = app.network_panel
    assert panel.summary.cget("text") == (
        "Could not read the network adapters: the DNS presets could not be read: the preset table is damaged"
    )
    assert panel.summary.cget("text_color") == theme.CRITICAL
    assert reads(engine) == 0 and not panel.loaded
    assert dialogs(app) == []
    assert panel.refresh_button.cget("state") == "normal"

    del engine.network_dns_presets
    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: panel.loaded and listed(app))
    assert panel.row_count == 4
    assert app.errors == []


def test_malformed_report_is_shown_in_panel(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    presets = list(DNS_PRESETS)
    panel.show({"warnings": []}, presets, engine_ready=True, elevated=True)
    assert panel.summary.cget("text") == (
        "Could not read the network adapters: the engine's report has no adapter list"
    )
    assert panel.summary.cget("text_color") == theme.CRITICAL
    assert panel.row_count == 0 and panel.list.winfo_children() == []

    # Adapter data of the wrong type is reported as well, and no half-built row is left.
    report = engine.network_list()
    report["adapters"][1]["ipv4"] = 5
    panel.show(report, presets, engine_ready=True, elevated=True)
    assert panel.summary.cget("text").startswith(
        "Could not read the network adapters: unexpected adapter data ("
    )
    assert panel.row_count == 0 and panel.list.winfo_children() == []
    assert not panel.warning_label.winfo_manager()

    # An adapter that is only listed with Show all is checked when it is shown.
    report = engine.network_list()
    report["adapters"][-1]["gateways"] = 5
    panel.show(report, presets, engine_ready=True, elevated=True)
    assert panel.row_count == 4
    panel.show_all_switch.toggle()
    assert panel.summary.cget("text").startswith(
        "Could not read the network adapters: unexpected adapter data ("
    )
    assert panel.row_count == 0 and panel.list.winfo_children() == []

    panel.refresh_button.invoke()
    pump(app, 5.0, until=lambda: panel.row_count == 5 and listed(app))
    assert panel.summary.cget("text_color") == theme.INK_MUTED
    assert app.errors == []


def test_dns_set_failure_reports_details(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, dns_fail={"ipv4"})
    panel = open_network(app)
    wifi = panel.row(WIFI)
    assert wifi is not None
    wifi.choose("quad9")
    confirm_dialog(app)
    result = next_dialog(app)
    assert result.title_text == "Some DNS settings were not changed"
    failure = "• IPv4: failed: cannot set the IPv4 DNS servers of Wi-Fi: Access is denied."
    assert failure in dialog_text(result)
    pump(app, 5.0, until=lambda: reads(engine) == 2 and listed(app))
    assert status(app) == ("Some DNS settings of Wi-Fi were not changed.", theme.WARNING)
    # The servers recorded before the failed write stay recorded, so the attempt can be undone.
    wifi = panel.row(WIFI)
    assert wifi is not None and wifi.undo_shown
    assert wifi.dns_menu.get() == "Automatic (DHCP)"
    confirm_dialog(app)

    # An engine error is reported, and the list is read again.
    engine.elevated = False
    wifi.choose("google")
    confirm_dialog(app)
    error = next_dialog(app)
    assert error.title_text == "Could not change DNS servers"
    assert "requires an elevated (Administrator) process" in dialog_text(error)
    pump(app, 5.0, until=lambda: reads(engine) == 3 and listed(app))
    assert status(app)[1] == theme.CRITICAL
    confirm_dialog(app)
    assert app.errors == []


def test_history_undo_restores_dns_and_reloads_network(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    engine.network_set_dns(WIFI, "cloudflare")
    panel = open_network(app)
    wifi = panel.row(WIFI)
    assert wifi is not None and wifi.undo_shown
    pump(app, 5.0, until=lambda: app.status_journal.cget("text") == "Journal: 1 revertible change")

    show_section(app, "History")
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 1 and idle(app))
    row = app.history_panel.rows[0]
    assert row.change.title == "DNS servers: Wi-Fi"
    assert row.undo_button.cget("state") == "normal"
    statuses = record_statuses(app)
    before = reads(engine)
    row.undo_button.invoke()
    text = confirm_dialog_text(app)
    assert "IPv4 DNS  ·  was automatic" in text
    pump(app, 5.0, until=lambda: engine._dns_active_count() == 0 and idle(app))
    sent, dry_run = engine.calls_named("revert_targets")[-1]
    assert sent["dns"] == [WIFI] and dry_run is False
    assert "Done: 1 change restored." in statuses, "the DNS record counts as restored"
    assert "Nothing was recorded to undo, so nothing changed." not in statuses

    # The Network section was loaded, so it is read again although History is showing.
    pump(app, 5.0, until=lambda: reads(engine) > before and listed(app))
    wifi = panel.row(WIFI)
    assert wifi is not None and not wifi.undo_shown
    assert wifi.dns_menu.get() == "Automatic (DHCP)"
    pump(app, 5.0, until=lambda: app.history_panel.row_count == 0 and idle(app))
    pump(app, 5.0, until=lambda: app.status_journal.cget("text") == "Journal: no changes")
    assert app.errors == []


def test_revert_all_lists_dns_actions(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    engine.network_set_dns(WIFI, "cloudflare")
    engine.network_set_dns(ETHERNET, "google")
    panel = open_network(app)
    statuses = record_statuses(app)
    app.revert_button.invoke()
    dialog = next_dialog(app)
    text = dialog_text(dialog)
    assert "DNS servers" in text
    assert "• restore IPv4 DNS servers of Wi-Fi to automatic" in text
    assert "• restore IPv4 DNS servers of Ethernet to automatic" in text
    assert "• restore IPv6 DNS servers of Ethernet to automatic" in text
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: engine._dns_active_count() == 0 and idle(app))
    assert engine.calls_named("revert_all")[-1] == (False,)
    assert "Done: 3 changes restored." in statuses
    pump(app, 5.0, until=lambda: listed(app) and not any(r.undo_shown for r in panel.rows))
    assert app.errors == []


def test_undo_button_on_adapter_row_reverts_only_that_adapter(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    engine.network_set_dns(WIFI, "cloudflare")
    engine.network_set_dns(ETHERNET, "google")
    panel = open_network(app)
    wifi, ethernet, switch = panel.row(WIFI), panel.row(ETHERNET), panel.row(DEFAULT_SWITCH)
    assert wifi is not None and ethernet is not None and switch is not None
    assert wifi.undo_shown and ethernet.undo_shown and not switch.undo_shown
    assert engine._dns_active_count() == 3

    wifi.undo_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Undo DNS change on Wi-Fi?"
    assert dialog.confirm_button.cget("text") == "Undo"
    text = dialog_text(dialog)
    assert "• restore IPv4 DNS servers of Wi-Fi to automatic" in text
    assert "Ethernet" not in text
    assert engine.calls_named("revert_targets") == [({"dns": [WIFI]}, True)]
    cancel(app, dialog)
    assert engine._dns_active_count() == 3

    statuses = record_statuses(app)
    wifi.undo_button.invoke()
    confirm_dialog(app)
    pump(app, 5.0, until=lambda: len(engine.calls_named("revert_targets")) == 3 and idle(app))
    assert engine.calls_named("revert_targets")[-1] == ({"dns": [WIFI]}, False)
    assert "Undoing the DNS change on Wi-Fi…" in statuses
    assert "Done: 1 change restored." in statuses
    assert engine._dns_active_count() == 2
    pump(app, 5.0, until=lambda: listed(app) and not panel.row(WIFI).undo_shown)  # type: ignore[union-attr]
    ethernet = panel.row(ETHERNET)
    assert ethernet is not None and ethernet.undo_shown
    assert ethernet.dns_menu.get() == "Google Public DNS"
    assert app.errors == []


def test_undo_without_a_record_changes_nothing(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    wifi = panel.row(WIFI)
    assert wifi is not None and not wifi.undo_shown
    app._on_undo_dns(wifi.adapter)
    pump(app, 5.0, until=lambda: reads(engine) == 2 and listed(app))
    assert engine.calls_named("revert_targets") == [({"dns": [WIFI]}, True)]
    assert status(app) == (
        "Wi-Fi: nothing recorded to undo. Cairn did not change its DNS servers.",
        theme.INK_SECONDARY,
    )
    pump(app, 0.2)
    assert dialogs(app) == []
    assert app.errors == []


def test_engine_errors_are_reported_for_flush_renew_and_reset(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)

    def no_flush() -> dict[str, Any]:
        engine._record("network_flush_dns")
        raise RuntimeError("the DNS Client service did not flush its cache")

    engine.network_flush_dns = no_flush  # type: ignore[method-assign]
    panel.flush_button.invoke()
    error = next_dialog(app)
    assert error.title_text == "Could not flush the DNS cache"
    assert "the DNS Client service did not flush its cache" in dialog_text(error)
    assert status(app)[1] == theme.CRITICAL
    confirm_dialog(app)
    assert idle(app) and panel.flush_button.cget("state") == "normal"

    # The engine refuses a lease renewal on an adapter that is not connected.
    ethernet = panel.row(ETHERNET)
    assert ethernet is not None
    app._on_renew(ethernet.adapter)
    assert confirm_dialog(app).title_text == "Renew the IP lease of Ethernet?"
    error = next_dialog(app)
    assert error.title_text == "Could not renew the lease of Ethernet"
    assert "this adapter is not connected" in dialog_text(error)
    pump(app, 5.0, until=lambda: reads(engine) == 2 and listed(app))
    assert status(app)[1] == theme.CRITICAL
    confirm_dialog(app)

    # A reset that cannot be planned asks nothing and reads nothing else.
    fake_reset = engine.network_reset
    plan_error: list[str] = ["cannot list the network adapters: access denied"]

    def reset(restore_point: str = "try", dry_run: bool = False) -> dict[str, Any]:
        if dry_run and plan_error:
            engine._record("network_reset", restore_point, dry_run)
            raise RuntimeError(plan_error[0])
        if not dry_run:
            engine._record("network_reset", restore_point, dry_run)
            raise RuntimeError("cannot open the journal: the file is locked")
        return fake_reset(restore_point=restore_point, dry_run=dry_run)

    engine.network_reset = reset  # type: ignore[method-assign]
    panel.reset_button.invoke()
    error = next_dialog(app)
    assert error.title_text == "Could not prepare the network reset"
    assert "cannot list the network adapters: access denied" in dialog_text(error)
    assert engine.calls_named("system_restore_enabled") == []
    confirm_dialog(app)
    assert idle(app) and panel.reset_button.cget("state") == "normal"

    # A reset that fails before its first step is reported, and the list is read again.
    plan_error.clear()
    panel.reset_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == "Reset the network stack?"
    assert dialog.acknowledge_box is not None
    pump(app, 0.3)
    dialog.acknowledge_box.toggle()
    dialog.confirm_button.invoke()
    error = next_dialog(app)
    assert error.title_text == "The network stack was not reset"
    assert "cannot open the journal: the file is locked" in dialog_text(error)
    assert engine.calls_named("network_reset")[-1] == ("try", False)
    pump(app, 5.0, until=lambda: reads(engine) == 3 and listed(app))
    assert status(app)[1] == theme.CRITICAL
    confirm_dialog(app)
    assert app.errors == []


def test_outdated_engine_disables_network_tab(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, unsupported=["network_list"])
    panel = app.network_panel
    assert panel.summary.cget("text") == f"⚠ {OUTDATED_TEXT}"
    assert panel.summary.cget("text_color") == theme.WARNING
    for widget in (panel.refresh_button, panel.flush_button, panel.reset_button, panel.show_all_switch):
        assert widget.cget("state") == "disabled"
    assert not panel.admin_label.winfo_manager()
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Network")
    pump(app, 0.3)
    app.load_network()
    pump(app, 0.2)
    assert not panel.loaded and panel.row_count == 0
    assert engine.calls_named("network_dns_presets") == []
    assert panel.summary.cget("text") == f"⚠ {OUTDATED_TEXT}"
    assert app.errors == []


def test_reset_and_renew_are_refused_while_a_tool_runs(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    panel = open_network(app)
    app._running_tool_title = lambda: "Check system files"  # type: ignore[method-assign]
    panel.reset_button.invoke()
    assert status(app) == (
        "Wait for Check system files to finish before resetting the network stack.",
        theme.WARNING,
    )
    wifi = panel.row(WIFI)
    assert wifi is not None
    wifi.renew_button.invoke()
    assert status(app) == ("Wait for Check system files to finish before renewing the lease.", theme.WARNING)
    pump(app, 0.2)
    assert dialogs(app) == []
    assert engine.calls_named("network_reset") == []
    assert engine.calls_named("network_renew_dhcp") == []
    assert engine.calls_named("system_restore_enabled") == []

    # Flushing the cache and changing DNS servers are still allowed.
    panel.flush_button.invoke()
    pump(app, 5.0, until=lambda: bool(engine.calls_named("network_flush_dns")) and idle(app))
    assert status(app) == (FLUSHED_TEXT, theme.GOOD)
    wifi.choose("cloudflare")
    dialog = next_dialog(app)
    assert dialog.title_text == "Change DNS servers of Wi-Fi?"
    cancel(app, dialog)
    assert dry_runs(engine) == [(WIFI, "cloudflare", None, None, "skip", True)]
    assert app.errors == []


def test_closing_during_a_network_reset_says_it_cannot_be_undone(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"network_reset": 1.5})
    panel = open_network(app)
    panel.reset_button.invoke()
    dialog = next_dialog(app)
    pump(app, 0.3)
    dialog.acknowledge_box.toggle()
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: len(engine.calls_named("network_reset")) == 2)

    app._request_close()
    closing = next_dialog(app)
    assert closing.title_text == BUSY_CLOSE_TITLE
    text = dialog_text(closing)
    assert f"The network stack reset is still running. {CLOSE_TEXTS[CLOSE_IRREVERSIBLE]}" in text
    assert "restart Windows" in text
    assert closing.confirm_button.cget("fg_color") == theme.CRITICAL
    cancel(app, closing)
    result = next_dialog(app)
    assert result.title_text == "Restart Windows to finish"
    cancel(app, result)
    assert app._running
    assert app.errors == []


def test_closing_while_a_dns_change_is_planned_is_a_read(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, slow={"network_set_dns": 1.5})
    panel = open_network(app)
    panel.row(WIFI).choose("cloudflare")
    pump(app, 5.0, until=lambda: bool(engine.calls_named("network_set_dns")))

    app._request_close()
    closing = next_dialog(app)
    assert closing.title_text == BUSY_CLOSE_TITLE
    assert CLOSE_TEXTS["read"] in dialog_text(closing)
    assert closing.confirm_button.cget("fg_color") != theme.CRITICAL
    cancel(app, closing)
    planned = next_dialog(app)
    assert planned.title_text == "Change DNS servers of Wi-Fi?"
    cancel(app, planned)
    assert app._running
    assert app.errors == []
