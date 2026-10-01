"""Profiles section: preview, apply, undo and export settings profiles.

Everything a profile applies goes through the existing journaled changes (tweaks, Store
apps, startup entries, DNS servers, Windows Update settings, scheduled maintenance) in one
journal session, so "Undo these changes", History and Revert All undo it; applying needs
administrator rights. Previewing a profile, reading a profile file and exporting this PC's
settings change nothing and write no journal rows.
"""

from __future__ import annotations

import logging
import os
from concurrent.futures import Future
from datetime import datetime
from typing import TYPE_CHECKING, Any

from .. import system, theme
from ..widgets.dialogs import MessageDialog
from ..widgets.profiles import ADMIN_NOTE as ADMIN_NOTE
from ..widgets.profiles import (
    RESTART_TEXT,
    REVERT_RESTART_TEXT,
    STARTER_SOURCE,
    ProfilesPanel,
    apply_details,
    counts_text,
    filter_is_empty,
    plan_counts_text,
    plural,
    result_summary,
    selected_restart,
    suggested_file_name,
)

if TYPE_CHECKING:
    import customtkinter as ctk

    from ..bridge.engine import EngineBridge

log = logging.getLogger(__name__)

UNSUPPORTED_TEXT = "The engine is out of date: rebuild and deploy it to use this section."
OUTDATED_TEXT = UNSUPPORTED_TEXT
NO_ENGINE_TEXT = "The engine is not loaded, so profiles can't be previewed or applied."
PROFILES_SECTION = "Profiles"
PROFILE_FILETYPES = (("Cairn profile", "*.json"), ("All files", "*.*"))
APPLY_MESSAGE = (
    "Cairn records each setting's current value before changing it, so you can undo these changes "
    "here, from History, or with Revert All Changes."
)
APPS_NOTE = (
    "Removed apps can be restored from Apps or History; if Windows has deleted an app's files, "
    "restoring it opens its Microsoft Store page."
)
RESTORE_POINT_NOTE = "Cairn first tries to create a System Restore point."
STALE_TEXT = "Settings changed since this preview, so it was refreshed. Review it and apply again."
INVALID_TITLE = "This file can't be used as a profile"
READ_ERROR_TITLE = "The profile file could not be read"
APPLY_ERROR_TITLE = "The profile could not be applied"
SAVE_ERROR_TITLE = "The profile could not be saved"
UNDO_READ_ERROR_TITLE = "Could not read the journal"


class ProfilesFeature:
    """State, widgets and flows of the Profiles section, mixed into `App`.

    Uses these members of the window: `engine`, `elevated`, `_busy`, `_mutations_enabled`,
    `section_visible`, `set_status`, `_guard`, `_set_busy`, `_show_error`, `_refresh_journal`,
    `_after_mutation`, `_revert_done` and `_restart_explorer`.
    """

    profiles_panel: ProfilesPanel
    _profiles_supported: bool
    _profile_starters: list[dict[str, Any]]
    _profile_text: str | None
    _profile_name: str
    _profile_source: str
    _profile_description: str
    _profile_plan: dict[str, Any] | None
    _profile_loading: bool
    _profile_pending: tuple[str, str, str, str] | None
    _profile_stale: bool
    _profile_stale_notice: bool
    _profile_undo: dict[str, Any] | None
    _profile_keep_unchecked: set[str]
    _profile_keep_status: bool
    _profile_retry: str

    if TYPE_CHECKING:
        engine: EngineBridge | None
        elevated: bool
        _busy: bool

        @property
        def _mutations_enabled(self) -> bool: ...
        def section_visible(self, name: str) -> bool: ...
        def set_status(self, text: str, color: str = ...) -> None: ...
        def _guard(self, *, needs_admin: bool = True) -> bool: ...
        def _set_busy(self, busy: bool, close: str = ..., *, action: str = ..., note: str = ...) -> None: ...
        def _show_error(self, title: str, exc: BaseException | None) -> None: ...
        def _refresh_journal(self) -> None: ...
        def _after_mutation(self) -> None: ...
        def _revert_done(self, future: Future[Any]) -> None: ...
        def _restart_explorer(self) -> None: ...

    def _init_profiles_state(self) -> None:
        """Sets the section's state; runs before the window is built."""
        self._profiles_supported = True
        self._profile_starters = []
        # The profile shown (canonical text, name, where it came from, its description) and
        # its last plan.
        self._profile_text = None
        self._profile_name = ""
        self._profile_source = ""
        self._profile_description = ""
        self._profile_plan = None
        # A plan read is in flight; the newest preview asked for meanwhile runs once it returns.
        self._profile_loading = False
        self._profile_pending = None
        # A change happened since the plan was read, so Apply reads it again first.
        self._profile_stale = False
        self._profile_stale_notice = False
        # The last apply's undo filter, name, count and time.
        self._profile_undo = None
        # Change rows the user unchecked, kept when the same profile is planned again.
        self._profile_keep_unchecked = set()
        # The plan being read leaves the status bar as it is: it follows an undo, whose own
        # status stays. Only set while that plan is read.
        self._profile_keep_status = False
        # What "Try again" repeats: "plan" or "export".
        self._profile_retry = "plan"

    # -- building and hooks ----------------------------------------------------------

    def _build_profiles_tab(self, frame: ctk.CTkFrame) -> None:
        """Builds the Profiles section into `frame`; handles a missing or outdated engine itself."""
        self.profiles_panel = ProfilesPanel(
            frame,
            starters=[],
            on_preview_starter=self._on_preview_starter,
            on_open_file=self._on_open_profile,
            on_export=self._on_export_profile,
            on_apply=self._on_apply_profile,
            on_save_export=self._on_save_profile,
            on_cancel_export=self._on_cancel_export,
            on_undo=self._on_undo_profile,
            on_retry=self._on_profile_retry,
        )
        self.profiles_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)
        if self.engine is None:
            self._profiles_supported = False
            self.profiles_panel.set_unsupported(NO_ENGINE_TEXT)
            return
        if not self.engine.supports("profile_apply"):
            self._profiles_supported = False
            self.profiles_panel.set_unsupported(OUTDATED_TEXT)
            return
        try:
            self._profile_starters = [dict(s) for s in self.engine.profile_starters()]
        except Exception as exc:  # noqa: BLE001 - the section stays usable for files and exports
            log.exception("starter profiles could not be read")
            self.profiles_panel.show_error(f"The starter profiles could not be read: {exc}", retry=False)
        self.profiles_panel.set_starters(self._profile_starters)
        self.profiles_panel.set_access(engine_ready=True, elevated=self.elevated)

    def _profiles_tab_shown(self) -> None:
        """Runs when the Profiles section is shown while the engine is loaded: a plan shown
        since before a change is read again."""
        if self._profiles_supported and self._profile_stale and self._profile_replannable():
            self._profile_replan()

    def _refresh_profile_actions(self) -> None:
        """Runs whenever the window's busy state changes."""
        panel = getattr(self, "profiles_panel", None)
        if panel is None or not self._profiles_supported:
            return
        panel.set_actions_enabled(
            reads=not self._busy,
            apply=not self._busy and self._mutations_enabled and not self._profile_loading,
        )

    def _profiles_after_mutation(self) -> None:
        """Runs after any journaled change or undo: the plan no longer describes this PC, so it
        is read again when it is on screen, or before the next apply."""
        self._profile_stale = True
        if (
            self._profiles_supported
            and self.section_visible(PROFILES_SECTION)
            and self._profile_replannable()
        ):
            self._profile_replan()

    def _profile_replannable(self) -> bool:
        """A plan is shown (not a result, an export or a message) and none is being read."""
        panel = getattr(self, "profiles_panel", None)
        return (
            panel is not None
            and panel.mode == "plan"
            and self._profile_text is not None
            and not self._profile_loading
        )

    def _profile_shown_starter(self) -> str | None:
        """Id of the starter whose profile the sheet holds; None for a profile file."""
        if self._profile_source != STARTER_SOURCE:
            return None
        return next(
            (str(s.get("id") or "") for s in self._profile_starters if s.get("text") == self._profile_text),
            None,
        )

    # -- previewing ------------------------------------------------------------------

    def plan_profile(self, text: str, name: str, source: str, description: str = "") -> None:
        """Reads what applying the profile in `text` would change on this PC (a dry run on the
        engine's worker). A preview asked for while one is read replaces any earlier waiting one
        and runs once the read in flight returns."""
        if self.engine is None or not self._profiles_supported:
            return
        if self._profile_loading:
            self._profile_pending = (text, name, source, description)
            return
        if text != self._profile_text:
            self._profile_keep_unchecked = set()
        self._profile_text = text
        self._profile_name = name
        self._profile_source = source
        self._profile_description = description
        self._profile_retry = "plan"
        self._profile_loading = True
        self.profiles_panel.set_loading(f"Checking what “{name}” would change on this PC…")
        self._refresh_profile_actions()
        self.engine.plan_profile(text, callback=self._profile_planned)

    def _profile_replan(self) -> None:
        """Plans the shown profile again, keeping the rows the user unchecked."""
        if self._profile_text is None:
            return
        panel = self.profiles_panel
        if panel.mode == "plan":
            self._profile_keep_unchecked = panel.unchecked_keys()
        self.plan_profile(
            self._profile_text, self._profile_name, self._profile_source, self._profile_description
        )

    def _profile_planned(self, future: Future[Any]) -> None:
        exc = future.exception()
        self._profile_loading = False
        keep_status, self._profile_keep_status = self._profile_keep_status, False
        pending, self._profile_pending = self._profile_pending, None
        if pending is not None:
            self._profile_stale_notice = False
            self.plan_profile(*pending)
            return
        notice, self._profile_stale_notice = self._profile_stale_notice, False
        if exc is not None:
            self._profile_plan = None
            self.profiles_panel.show_error(str(exc))
            self.set_status(f"“{self._profile_name}” could not be previewed: {exc}", theme.CRITICAL)
            self._refresh_profile_actions()
            return
        plan = future.result()
        self._profile_plan = plan
        self._profile_stale = False
        self.profiles_panel.show_plan(
            plan,
            source=self._profile_source,
            keep_unchecked=self._profile_keep_unchecked,
            description=self._profile_description,
        )
        if notice:
            self.profiles_panel.sheet.add_warning(STALE_TEXT)
            self.set_status(STALE_TEXT, theme.WARNING)
        elif not keep_status:
            self.set_status(
                f"“{self._profile_name}”: {plan_counts_text(plan, sep=', ')}. Nothing has changed yet."
            )
        self._refresh_profile_actions()

    def _on_preview_starter(self, starter: dict[str, Any]) -> None:
        if self.engine is None:
            return
        self.profiles_panel.mark_current_starter(str(starter.get("id") or ""))
        self.plan_profile(
            str(starter["text"]),
            str(starter.get("name") or ""),
            STARTER_SOURCE,
            str(starter.get("description") or ""),
        )

    def _on_open_profile(self) -> None:
        if self.engine is None or not self._profiles_supported:
            return
        path = system.ask_open_path(self, title="Open a Cairn profile", filetypes=PROFILE_FILETYPES)
        if not path:
            return
        path = os.path.normpath(path)
        self.set_status(f"Reading {os.path.basename(path)}…")
        self.engine.read_profile(path, callback=lambda future: self._profile_read_done(future, path))

    def _profile_read_done(self, future: Future[Any], path: str) -> None:
        exc = future.exception()
        name = os.path.basename(path)
        if isinstance(exc, ValueError):
            self.set_status(f"{name} can't be used as a profile.", theme.WARNING)
            MessageDialog(self, title=INVALID_TITLE, message=f"{name}: {exc}")
            return
        if exc is not None:
            self._show_error(READ_ERROR_TITLE, exc)
            return
        summary = future.result()
        self.profiles_panel.mark_current_starter(None)
        self.plan_profile(
            str(summary["text"]),
            str(summary.get("name") or name),
            f"File: {name}",
            str(summary.get("description") or ""),
        )

    def _on_profile_retry(self) -> None:
        """ "Try again" and "Preview again": repeats the read that failed, or plans the shown
        profile again."""
        if self._profile_retry == "export":
            self._on_export_profile()
        else:
            self._profile_replan()

    # -- applying --------------------------------------------------------------------

    def _on_apply_profile(self, keys: list[str]) -> None:
        if not keys or not self._guard(needs_admin=True):
            return
        assert self.engine is not None
        plan = self._profile_plan
        if plan is None or self._profile_text is None:
            return
        if self._profile_stale:
            # The plan predates a change: show the current one instead of applying the old.
            self._profile_keep_unchecked = self.profiles_panel.unchecked_keys()
            self._profile_stale_notice = True
            self.set_status(STALE_TEXT, theme.WARNING)
            self._profile_replan()
            return
        rows = [r for r in plan["rows"] if r["key"] in set(keys)]
        unchecked = self.profiles_panel.unchecked_keys()
        name = self._profile_name
        text = self._profile_text
        parts = [APPLY_MESSAGE]
        if any(r.get("section") == "apps" for r in rows):
            parts.append(APPS_NOTE)
        if self.engine.next_mutation_creates_restore_point:
            parts.append(RESTORE_POINT_NOTE)
        restart = RESTART_TEXT.get(selected_restart(rows, keys), "")
        if restart:
            parts.append(restart)

        def run() -> None:
            assert self.engine is not None
            # The result has no checkboxes, so a plan read after it needs the selection from here.
            self._profile_keep_unchecked = unchecked
            self._set_busy(True, "journaled", action="Applying the profile")
            self.set_status(f"Applying “{name}”…")
            self.engine.apply_profile(text, list(keys), callback=self._profile_applied)

        MessageDialog(
            self,
            title=f"Apply {plural(len(rows), 'change')} from “{name}”?",
            message=" ".join(parts),
            details=apply_details(rows),
            confirm_text="Apply",
            cancel_text="Cancel",
            on_confirm=run,
        )

    def _profile_applied(self, future: Future[Any]) -> None:
        self._set_busy(False)
        exc = future.exception()
        if exc is not None:
            self._show_error(APPLY_ERROR_TITLE, exc)
            self._after_mutation()
            return
        report = future.result()
        name = str(report.get("name") or self._profile_name)
        applied = int(report.get("applied") or 0)
        if not filter_is_empty(report.get("undo")):
            when = datetime.now().strftime("%H:%M")
            self._profile_undo = {"name": name, "filter": report["undo"], "applied": applied, "time": when}
            self.profiles_panel.set_last_applied(name, when, applied)
        if self._profile_plan is not None:
            self.profiles_panel.show_result(self._profile_plan, report)
        results = report.get("results") or []
        failed = [r for r in results if r.get("outcome") == "failed"]
        skipped = [r for r in results if r.get("outcome") == "skipped"]
        restart = RESTART_TEXT.get(str(report.get("restart")), "") if applied else ""
        rp = report.get("restore_point")
        notes = [f"Restore point #{rp['sequence']} created."] if rp else []
        notes += [str(w) for w in report.get("warnings") or []]
        summary = result_summary(report)
        if failed or skipped or restart or notes:
            explorer = report.get("restart") == "explorer" and applied > 0
            MessageDialog(
                self,
                title="Some changes failed" if failed else "Profile applied",
                message=" ".join([summary + "."] + notes + ([restart] if restart else [])),
                details=[
                    f"• {r.get('title') or r.get('key')} ({r.get('outcome')}): "
                    + "; ".join(str(d) for d in r.get("details") or [])
                    for r in failed + skipped
                ],
                confirm_text="Restart Explorer now" if explorer else "OK",
                cancel_text="Later" if explorer else None,
                on_confirm=self._restart_explorer if explorer else None,
            )
        self.set_status(f"Done: {summary}. {restart}".strip(), theme.WARNING if failed else theme.GOOD)
        self._refresh_journal()
        self._after_mutation()

    # -- undoing ---------------------------------------------------------------------

    def _on_undo_profile(self) -> None:
        undo = self._profile_undo
        if undo is None or not self._guard(needs_admin=True):
            return
        assert self.engine is not None
        name = str(undo["name"])
        target_filter = dict(undo["filter"])
        self._set_busy(True, "read")
        self.set_status(f"Checking what “{name}” changed…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            exc = future.exception()
            if exc is not None:
                self._show_error(UNDO_READ_ERROR_TITLE, exc)
                return
            plan = future.result()
            actions = plan.get("actions") or []
            if not actions:
                self._profile_undo_gone()
                self.set_status(f"Nothing from “{name}” is still recorded; it may have been undone already.")
                return
            restart = REVERT_RESTART_TEXT.get(str(plan.get("restart") or "none"), "")
            message = (
                f"{plural(len(actions), 'recorded change')} will be restored to the values they had "
                "before Cairn changed them."
            )
            if restart:
                message += " " + restart

            def run() -> None:
                assert self.engine is not None
                self._set_busy(True, "journaled", action="Undoing the profile")
                self.set_status(f"Undoing “{name}”…")
                self.engine.revert_targets(target_filter, dry_run=False, callback=self._profile_undone)

            self.set_status("")
            MessageDialog(
                self,
                title=f"Undo the changes from “{name}”?",
                message=message,
                details=[f"• {a}" for a in actions],
                confirm_text="Undo changes",
                cancel_text="Cancel",
                on_confirm=run,
            )

        self.engine.revert_targets(target_filter, dry_run=True, callback=planned)

    def _profile_undone(self, future: Future[Any]) -> None:
        if future.exception() is None:
            self._profile_undo_gone()
        self._revert_done(future)

    def _profile_undo_gone(self) -> None:
        """Forgets the last apply once its changes are undone or no longer recorded: hides its
        card and, when the sheet still shows an apply's result, reads the profile again, so the
        sheet offers no undo that is gone and no row says "Applied" any more."""
        self._profile_undo = None
        panel = self.profiles_panel
        panel.clear_last_applied()
        if panel.mode != "result":
            return
        self._profile_keep_status = True
        self._profile_replan()
        if not self._profile_loading:
            # No plan is being read, so no later one may inherit the flag.
            self._profile_keep_status = False

    # -- exporting -------------------------------------------------------------------

    def _on_export_profile(self) -> None:
        if not self._profiles_supported or not self._guard(needs_admin=False):
            return
        assert self.engine is not None
        self._profile_retry = "export"
        self._set_busy(True, "read")
        self.profiles_panel.mark_current_starter(None)
        self.profiles_panel.set_loading("Reading this PC's settings…")
        self.engine.profile_candidates(callback=self._profile_candidates_loaded)

    def _profile_candidates_loaded(self, future: Future[Any]) -> None:
        self._set_busy(False)
        exc = future.exception()
        if exc is not None:
            self.profiles_panel.show_error(f"This PC's settings could not be read: {exc}")
            self.set_status(f"This PC's settings could not be read: {exc}", theme.CRITICAL)
            return
        self.profiles_panel.show_export(future.result())
        self.set_status("Choose what to include and name the profile.")

    def _on_save_profile(self, name: str, description: str, keys: list[str]) -> None:
        name = name.strip()
        if not name:
            self.profiles_panel.show_name_error("Give the profile a name.")
            return
        if not keys or not self._guard(needs_admin=False):
            return
        assert self.engine is not None
        path = system.ask_save_path(
            self,
            title="Save the profile",
            filetypes=PROFILE_FILETYPES,
            initialfile=suggested_file_name(name),
            defaultextension=".json",
        )
        if not path:
            return
        path = os.path.normpath(path)
        self._set_busy(True, "read", action="Saving the profile")
        self.set_status(f"Saving “{name}”…")
        self.engine.export_profile(
            path, name, description, list(keys), callback=lambda future: self._profile_saved(future, path)
        )

    def _profile_saved(self, future: Future[Any], path: str) -> None:
        self._set_busy(False)
        exc = future.exception()
        if exc is not None:
            self._show_error(SAVE_ERROR_TITLE, exc)
            return
        report = future.result()
        missing = list(report.get("missing") or [])
        text = (
            f"Saved “{report.get('name')}” ({counts_text(report.get('counts') or {})}) "
            f"to {os.path.basename(path)}."
        )
        if missing:
            text += (
                f" {plural(len(missing), 'setting')} changed since the list was read and "
                f"{'was' if len(missing) == 1 else 'were'} left out."
            )
        self._profile_leave_export()
        self.set_status(text, theme.WARNING if missing else theme.GOOD)

    def _on_cancel_export(self) -> None:
        self._profile_leave_export()
        self.set_status("")

    def _profile_leave_export(self) -> None:
        """Shows the plan the sheet held before the export, read again if it is out of date,
        with its starter's card marked again."""
        self._profile_retry = "plan"
        plan = self._profile_plan
        if plan is None or self._profile_text is None:
            self.profiles_panel.set_empty()
            return
        self.profiles_panel.mark_current_starter(self._profile_shown_starter())
        if self._profile_stale:
            self.profiles_panel.show_plan(
                plan,
                source=self._profile_source,
                keep_unchecked=self._profile_keep_unchecked,
                description=self._profile_description,
            )
            self._profile_replan()
            return
        self.profiles_panel.show_plan(
            plan,
            source=self._profile_source,
            keep_unchecked=self._profile_keep_unchecked,
            description=self._profile_description,
        )
