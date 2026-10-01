"""Network section: adapters, journaled DNS server changes, the DNS cache, DHCP leases and the
network stack reset.

A DNS change is planned first (a dry run) and confirmed, then the engine records the
adapter's current servers before it writes, so the change reverts from the adapter's row,
History or Revert All. Flushing the cache, renewing a lease and resetting the stack are
irreversible and only logged. Renew and reset are refused while a job that network
maintenance must wait for runs (a maintenance tool or winget); the running job is read only
through `_running_job_title(network_only=True)`.
"""

from __future__ import annotations

import logging
from concurrent.futures import Future
from typing import TYPE_CHECKING, Any

from .. import theme
from ..widgets.dialogs import MessageDialog
from ..widgets.network import (
    CUSTOM_PRESET,
    CustomDnsDialog,
    NetworkPanel,
    dns_done_text,
    failed_step_output_lines,
    nothing_to_change_text,
    plan_lines,
    reset_details,
    reset_result_lines,
    restore_point_sentence,
)

if TYPE_CHECKING:
    import customtkinter as ctk

    from ..bridge.engine import EngineBridge

log = logging.getLogger(__name__)

OUTDATED_TEXT = "The engine is out of date: rebuild and deploy it to use this section."
NO_ENGINE_TEXT = "The engine is not loaded, so network adapters can't be read."
DNS_CONFIRM_MESSAGE = (
    "The current settings are recorded first, so you can undo this from History or with Undo "
    "DNS change on this adapter. It takes effect immediately; no restart is needed."
)
UNDO_DNS_MESSAGE = (
    "The DNS servers recorded before Cairn changed them are restored. It takes effect "
    "immediately; no restart is needed."
)
RENEW_MESSAGE = (
    "Windows asks the network's DHCP server (usually your router) for a fresh IPv4 lease, including its "
    "current DNS servers. The connection can pause for a few seconds; if no server answers, this can take "
    "up to a minute."
)
RESET_MESSAGE = (
    "Resets the Winsock catalog and the TCP/IP settings of every adapter to Windows defaults. This cannot "
    "be undone from Cairn. Manual IP addresses and DNS servers are removed (adapters go back to "
    "automatic), and VPN, firewall or antivirus software that hooks into networking may need to be "
    "repaired or reinstalled. Restart Windows afterwards to finish."
)
RESET_ACKNOWLEDGE = "I understand this cannot be undone and have noted the settings listed above."
FLUSHED_TEXT = "DNS cache flushed. Names are looked up again as they are needed."


class NetworkFeature:
    """State, widgets and flows of the Network section, mixed into `App`.

    Uses these members of the window: `engine`, `elevated`, `section_visible`, `set_status`,
    `_guard`, `_set_busy`, `_show_error`, `_refresh_journal`, `load_history`, `_revert_done`
    and `_running_job_title`.
    """

    network_panel: NetworkPanel
    _network_presets: list[dict[str, Any]]
    _network_loading: bool
    _network_reload: bool
    _network_supported: bool

    if TYPE_CHECKING:
        engine: EngineBridge | None
        elevated: bool

        def section_visible(self, name: str) -> bool: ...
        def set_status(self, text: str, color: str = ...) -> None: ...
        def _guard(self, *, needs_admin: bool = True) -> bool: ...
        def _set_busy(self, busy: bool, close: str = ..., *, action: str = ..., note: str = ...) -> None: ...
        def _show_error(self, title: str, exc: BaseException | None) -> None: ...
        def _refresh_journal(self) -> None: ...
        def load_history(self) -> None: ...
        def _revert_done(self, future: Future[Any]) -> None: ...
        def _running_job_title(self, *, network_only: bool = False) -> str | None: ...

    def _init_network_state(self) -> None:
        """Sets the section's state; runs before the window is built."""
        self._network_presets = []
        # A list read is in flight; a reload asked for meanwhile runs once it returns.
        self._network_loading = False
        self._network_reload = False
        self._network_supported = True

    def _build_network_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the section's panel into `frame`; handles a missing engine itself."""
        self.network_panel = NetworkPanel(
            frame,
            on_refresh=self.load_network,
            on_flush=self._on_flush_dns,
            on_reset=self._on_network_reset,
            on_dns=self._on_dns,
            on_renew=self._on_renew,
            on_undo_dns=self._on_undo_dns,
        )
        self.network_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None:
            self._network_supported = False
            self.network_panel.set_unsupported(NO_ENGINE_TEXT)
        elif not self.engine.supports("network_list"):
            self._network_supported = False
            self.network_panel.set_unsupported(OUTDATED_TEXT)
        else:
            self.network_panel.set_access(engine_ready=True, elevated=self.elevated)

    def _network_tab_shown(self) -> None:
        """Runs when the Network section is shown while the engine is loaded.

        The list is read on every visit, also while another operation runs: the read waits
        on the engine's worker behind it, and rows that are busy stay disabled.
        """
        self.load_network()

    def _network_after_mutation(self) -> None:
        """Runs after any journaled change or undo, so the adapter list stays current."""
        panel = getattr(self, "network_panel", None)
        if panel is not None and panel.loaded:
            self.load_network()

    # -- listing -------------------------------------------------------------------

    def load_network(self) -> None:
        """Reads the DNS presets (cached by the bridge) and queues a read of the adapters."""
        if self.engine is None or not self._network_supported:
            return
        if self._network_loading:
            self._network_reload = True
            return
        try:
            presets = self.engine.dns_presets()
        except Exception as exc:  # noqa: BLE001 - an engine failure is reported in the panel
            log.exception("DNS presets could not be read")
            self.network_panel.show_error(f"the DNS presets could not be read: {exc}")
            return
        self._network_presets = list(presets)
        self._network_loading = True
        self.network_panel.set_loading()
        self.engine.network_list(callback=self._network_loaded)

    def _network_loaded(self, future: Future[Any]) -> None:
        self._network_loading = False
        if self._network_reload:
            # The list changed while it was read; the newer read replaces this one.
            self._network_reload = False
            self.load_network()
            return
        exc = future.exception()
        if exc is not None:
            self.network_panel.show_error(str(exc))
            return
        self.network_panel.show(
            future.result(), self._network_presets, engine_ready=True, elevated=self.elevated
        )

    def _after_network_change(self) -> None:
        """Refreshes the views a network action can affect."""
        self.load_network()
        self._refresh_journal()
        if self.section_visible("History"):
            self.load_history()

    def _reset_dns_choice(self, adapter_id: str) -> None:
        # Rows are rebuilt on every reload, so the adapter's current row is looked up.
        row = self.network_panel.row(adapter_id)
        if row is not None:
            row.reset_choice()

    # -- DNS servers ---------------------------------------------------------------

    def _on_dns(self, adapter: dict[str, Any], preset_id: str) -> None:
        """Changes the DNS servers of `adapter` to a preset, or to custom servers ("custom")."""
        adapter_id = str(adapter.get("id", ""))
        if preset_id != CUSTOM_PRESET:
            self._plan_dns(adapter, preset_id)
            return
        if not self._guard(needs_admin=True):
            self._reset_dns_choice(adapter_id)
            return
        CustomDnsDialog(
            self,
            adapter=adapter,
            presets=self._network_presets,
            on_submit=lambda ipv4, ipv6: self._plan_dns(adapter, CUSTOM_PRESET, ipv4 or None, ipv6 or None),
            on_cancel=lambda: self._reset_dns_choice(adapter_id),
        )

    def _plan_dns(
        self,
        adapter: dict[str, Any],
        preset_id: str,
        ipv4: list[str] | None = None,
        ipv6: list[str] | None = None,
    ) -> None:
        """Dry-runs the change and asks before applying it; nothing to change needs no dialog."""
        adapter_id = str(adapter.get("id", ""))
        name = str(adapter.get("name") or adapter_id)
        if not self._guard(needs_admin=True):
            self._reset_dns_choice(adapter_id)
            return
        assert self.engine is not None
        # The row shows the check; the status bar keeps its message until there is a result.
        self._set_busy(True, "read")
        self.network_panel.set_busy(adapter_id, "Checking…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            self.network_panel.set_idle()
            exc = future.exception()
            if exc is not None:
                self._show_error("Could not plan the DNS change", exc)
                self._reset_dns_choice(adapter_id)
                return
            report = future.result()
            if not any(c.get("outcome") == "planned" for c in report.get("changes") or []):
                self.set_status(*nothing_to_change_text(report, adapter))
                self._reset_dns_choice(adapter_id)
                return
            MessageDialog(
                self,
                title=f"Change DNS servers of {name}?",
                message=DNS_CONFIRM_MESSAGE,
                details=plan_lines(report),
                confirm_text="Change DNS",
                cancel_text="Cancel",
                on_confirm=lambda: self._run_dns(adapter_id, name, preset_id, ipv4, ipv6),
                on_cancel=lambda: self._reset_dns_choice(adapter_id),
            )

        self.engine.plan_set_dns(adapter_id, preset_id, ipv4, ipv6, callback=planned)

    def _run_dns(
        self, adapter_id: str, name: str, preset_id: str, ipv4: list[str] | None, ipv6: list[str] | None
    ) -> None:
        if self.engine is None:
            return
        self._set_busy(True, "journaled", action=f"The DNS change on {name}")
        self.network_panel.set_busy(adapter_id, "Changing DNS…")
        self.set_status(f"Changing the DNS servers of {name}…")
        self.engine.set_dns(adapter_id, preset_id, ipv4, ipv6, callback=self._dns_done)

    def _dns_done(self, future: Future[Any]) -> None:
        self._set_busy(False)
        self.network_panel.set_idle()
        exc = future.exception()
        if exc is not None:
            self._show_error("Could not change DNS servers", exc)
            self._after_network_change()
            return
        report = future.result()
        name = report.get("adapter_name") or report.get("adapter_id") or "the adapter"
        outcomes = [c.get("outcome") for c in report.get("changes") or []]
        if "failed" in outcomes:
            MessageDialog(
                self,
                title="Some DNS settings were not changed",
                message=f"Cairn could not change every DNS setting of {name}. Settings that were "
                "recorded can be undone from History.",
                details=plan_lines(report),
            )
            self.set_status(f"Some DNS settings of {name} were not changed.", theme.WARNING)
        elif "applied" not in outcomes:
            # The adapter's servers changed between the plan and the write: nothing was
            # written, so nothing was recorded to undo.
            row = self.network_panel.row(str(report.get("adapter_id") or ""))
            self.set_status(*nothing_to_change_text(report, row.adapter if row is not None else {}))
        else:
            row = self.network_panel.row(str(report.get("adapter_id") or ""))
            self.set_status(
                *dns_done_text(report, row.adapter if row is not None else {}, self._network_presets)
            )
        self._after_network_change()

    def _on_undo_dns(self, adapter: dict[str, Any]) -> None:
        """Restores the DNS servers recorded for `adapter`, both families, after confirmation."""
        adapter_id = str(adapter.get("id", ""))
        name = str(adapter.get("name") or adapter_id)
        if not self._guard(needs_admin=True):
            return
        assert self.engine is not None
        self._set_busy(True, "read")
        self.network_panel.set_busy(adapter_id, "Checking…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            self.network_panel.set_idle()
            exc = future.exception()
            if exc is not None:
                self._show_error("Could not read the journal", exc)
                return
            actions = future.result().get("actions") or []
            if not actions:
                self.set_status(f"{name}: nothing recorded to undo. Cairn did not change its DNS servers.")
                self.load_network()
                return
            MessageDialog(
                self,
                title=f"Undo DNS change on {name}?",
                message=UNDO_DNS_MESSAGE,
                details=[f"• {a}" for a in actions],
                confirm_text="Undo",
                cancel_text="Cancel",
                on_confirm=lambda: self._run_undo_dns(adapter_id, name),
            )

        self.engine.revert_targets({"dns": [adapter_id]}, dry_run=True, callback=planned)

    def _run_undo_dns(self, adapter_id: str, name: str) -> None:
        if self.engine is None:
            return
        self._set_busy(True, "journaled", action=f"Undoing the DNS change on {name}")
        self.network_panel.set_busy(adapter_id, "Undoing…")
        self.set_status(f"Undoing the DNS change on {name}…")
        self.engine.revert_targets({"dns": [adapter_id]}, callback=self._undo_dns_done)

    def _undo_dns_done(self, future: Future[Any]) -> None:
        self.network_panel.set_idle()
        # The window's revert handler reports the result and reloads every affected view.
        self._revert_done(future)

    # -- cache, lease and reset ----------------------------------------------------

    def _on_flush_dns(self) -> None:
        """Clears the DNS resolver cache; allowed for standard users, logged only."""
        if not self._guard(needs_admin=False):
            return
        assert self.engine is not None
        self._set_busy(True, "irreversible", action="The DNS cache flush")
        self.network_panel.set_busy(None, "Flushing the DNS cache…")
        self.set_status("Flushing the DNS cache…")

        def done(future: Future[Any]) -> None:
            self._set_busy(False)
            self.network_panel.set_idle()
            exc = future.exception()
            if exc is not None:
                self._show_error("Could not flush the DNS cache", exc)
            else:
                self.set_status(FLUSHED_TEXT, theme.GOOD)
            if self.section_visible("History"):
                self.load_history()

        self.engine.flush_dns(callback=done)

    def _on_renew(self, adapter: dict[str, Any]) -> None:
        """Renews the adapter's IPv4 DHCP lease after confirmation; logged only."""
        adapter_id = str(adapter.get("id", ""))
        name = str(adapter.get("name") or adapter_id)
        tool = self._running_job_title(network_only=True)
        if tool:
            self.set_status(f"Wait for {tool} to finish before renewing the lease.", theme.WARNING)
            return
        if not self._guard(needs_admin=True):
            return
        MessageDialog(
            self,
            title=f"Renew the IP lease of {name}?",
            message=RENEW_MESSAGE,
            confirm_text="Renew lease",
            cancel_text="Cancel",
            on_confirm=lambda: self._run_renew(adapter_id, name),
        )

    def _run_renew(self, adapter_id: str, name: str) -> None:
        if self.engine is None:
            return
        self._set_busy(True, "irreversible", action=f"The IP lease renewal of {name}")
        self.network_panel.set_busy(adapter_id, "Renewing…")
        self.set_status(f"Renewing the IP lease of {name}…")

        def done(future: Future[Any]) -> None:
            self._set_busy(False)
            self.network_panel.set_idle()
            exc = future.exception()
            if exc is not None:
                self._show_error(f"Could not renew the lease of {name}", exc)
            else:
                report = future.result()
                renewed = report.get("adapter_name") or name
                addresses = [str(a) for a in report.get("ipv4") or []]
                if addresses:
                    self.set_status(f"{renewed} renewed its lease: {', '.join(addresses)}.", theme.GOOD)
                else:
                    self.set_status(
                        f"{renewed} renewed its lease but has no IPv4 address yet.", theme.WARNING
                    )
            self.load_network()
            if self.section_visible("History"):
                self.load_history()

        self.engine.renew_lease(adapter_id, callback=done)

    def _on_network_reset(self) -> None:
        """Plans the stack reset, reads System Protection and asks with an acknowledgement."""
        tool = self._running_job_title(network_only=True)
        if tool:
            self.set_status(f"Wait for {tool} to finish before resetting the network stack.", theme.WARNING)
            return
        if not self._guard(needs_admin=True):
            return
        assert self.engine is not None
        self._set_busy(True, "read")
        self.network_panel.set_busy(None, "Preparing the reset…")

        def checked(plan: dict[str, Any], enabled: bool | None) -> None:
            self._set_busy(False)
            self.network_panel.set_idle()
            restore_points = self.engine is not None and self.engine.restore_points
            MessageDialog(
                self,
                title="Reset the network stack?",
                message=RESET_MESSAGE + restore_point_sentence(restore_points, enabled),
                details=reset_details(plan),
                confirm_text="Reset network stack",
                cancel_text="Cancel",
                danger=True,
                acknowledge=RESET_ACKNOWLEDGE,
                on_confirm=lambda: self._run_network_reset(restore_points and enabled is not False),
            )

        def planned(future: Future[Any]) -> None:
            exc = future.exception()
            if exc is not None:
                self._set_busy(False)
                self.network_panel.set_idle()
                self._show_error("Could not prepare the network reset", exc)
                return
            plan = future.result()

            def protection(result: Future[Any]) -> None:
                # A failed read leaves the restore point outcome unknown.
                enabled = None if result.exception() is not None else bool(result.result())
                checked(plan, enabled)

            engine = self.engine
            if engine is None:
                checked(plan, None)
                return
            try:
                engine.system_restore_enabled(callback=protection)
            except (AttributeError, RuntimeError):
                # A module without the query, or a worker that is shutting down.
                log.exception("System Protection state could not be queried")
                checked(plan, None)

        self.engine.plan_network_reset(callback=planned)

    def _run_network_reset(self, restore_point: bool) -> None:
        if self.engine is None:
            return
        text = "Resetting the network stack…"
        if restore_point:
            text = "Creating a restore point and resetting the network stack…"
        self._set_busy(
            True,
            "irreversible",
            action="The network stack reset",
            note="Wait a few minutes for it to finish, then restart Windows to complete the reset.",
        )
        self.network_panel.set_busy(None, text)
        self.set_status(text)
        self.engine.network_reset(callback=self._reset_done)

    def _reset_done(self, future: Future[Any]) -> None:
        self._set_busy(False)
        self.network_panel.set_idle()
        exc = future.exception()
        if exc is not None:
            self._show_error("The network stack was not reset", exc)
            self._after_network_change()
            return
        report = future.result()
        statuses = {s.get("status") for s in report.get("steps") or []}
        details = reset_result_lines(report) + failed_step_output_lines(report)
        if statuses & {"failed", "timed_out"}:
            MessageDialog(
                self,
                title="Network reset partly failed",
                message="Some reset steps failed or did not finish. Restart Windows, then check the "
                "connection; the details are below.",
                details=details,
            )
            self.set_status(
                "The network reset partly failed; restart Windows and check the details.", theme.CRITICAL
            )
        elif "completed_with_errors" in statuses:
            MessageDialog(
                self,
                title="Restart Windows to finish",
                message="Some settings could not be reset; restart Windows to finish. Details are below.",
                details=details,
            )
            self.set_status("Network stack reset with some errors. Restart Windows to finish.", theme.WARNING)
        else:
            MessageDialog(
                self,
                title="Restart Windows to finish",
                message="The network stack was reset. Restart Windows to finish; until then some "
                "connections may not work.",
                details=details,
            )
            self.set_status("Network stack reset. Restart Windows to finish.", theme.WARNING)
        self._after_network_change()
