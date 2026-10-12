"""Main window: a sidebar of sections, a top bar with the section title and Revert All
Changes, the shown section, and a status bar.

Sections (`optimizer.sections`): the live monitor (Dashboard), system information, the
security checkup, boot history, optimizations, apps, startup apps, app permissions, network,
cleanup, storage, updates, maintenance tools, scheduled maintenance, history and profiles.
Dashboard, Optimize, Apps, Cleanup, Startup and History are built here; every other section
comes from a feature mixin (`optimizer.features`): `SystemInfoFeature`, `NetworkFeature`,
`ToolsFeature`, `HealthFeature`, `PermissionsFeature`, `StorageFeature`, `UpdatesFeature`,
`MaintenanceFeature` and `ProfilesFeature`. The window calls their hooks by name from the
registries below while it builds, when a section is shown, when the busy state changes,
after a change, every frame and when it closes.

Threading model: Tk runs on the main thread and redraws at 60 Hz. The telemetry DLL
samples on its own native thread and is read with a sub-microsecond snapshot copy each
frame. Every engine call (scan, apply, revert, cleanup, startup) runs on the EngineBridge
worker thread; its callbacks are dispatched back onto the Tk thread from the frame loop.
Background jobs (maintenance tools, winget, storage jobs, the Windows Update search, the
scheduled maintenance monitor) run inside the engine, outside the worker, and each job lane
is polled from the frame loop.
"""

from __future__ import annotations

import logging
import os
import platform
import time
import tkinter as tk
import tkinter.font as tkfont
import traceback
from collections import deque
from collections.abc import Callable, Iterable
from concurrent.futures import Future
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any, NamedTuple

import customtkinter as ctk

from . import APP_NAME, PACKAGE_DIR, PROJECT_URL, __version__, paths, system, theme
from .bridge.engine import CATEGORY_MODES, EngineBridge, EngineUnavailable
from .bridge.telemetry import Telemetry, TelemetryError
from .features import build_placeholder
from .features.health import HealthFeature
from .features.maintenance import MaintenanceFeature
from .features.network import NetworkFeature
from .features.permissions import PermissionsFeature
from .features.profiles import ProfilesFeature
from .features.storage import StorageFeature
from .features.system_info import SystemInfoFeature
from .features.tools import ToolsFeature
from .features.updates import UpdatesFeature
from .sections import FIRST, SECTIONS, section
from .widgets.about import (
    ABOUT_TITLE,
    DEVELOPMENT,
    INSTALLED,
    NO_DATA_YET_TEXT,
    NOT_INSTALLED,
    OPEN_DATA_TEXT,
    PROJECT_PAGE_TEXT,
    UNKNOWN_COPY,
    AboutInfo,
    about_lines,
    about_message,
)
from .widgets.brand import set_window_icon
from .widgets.cleanup import CleanupPanel, fmt_size
from .widgets.controls import CategoryToggle, ItemListPanel, recorded_recommended
from .widgets.dialogs import MessageDialog
from .widgets.history import (
    KEEP_FORMER,
    ChangeGroup,
    HistoryPanel,
    drift_lines,
    holds,
    packaged_task_names,
    selects_every_record,
)
from .widgets.monitor import HardwareMonitor, set_text
from .widgets.sidebar import Sidebar, badge_fit, sidebar_mode
from .widgets.startup import StartupPanel

if TYPE_CHECKING:
    from .instance import InstanceLock

log = logging.getLogger(__name__)

FRAME_HZ = 60.0
TWEAK_CATEGORIES = ("privacy", "performance", "gaming", "interface", "security")
RESTART_TEXT = {
    "explorer": "File Explorer has to restart for some of these changes.",
    "sign_out": "Sign out and back in to finish applying these changes.",
    "restart": "Restart Windows to finish applying these changes.",
}
REVERT_RESTART_TEXT = {
    "explorer": "File Explorer has to restart for some of the restored settings.",
    "sign_out": "Sign out and back in to finish undoing these changes.",
    "restart": "Restart Windows to finish undoing these changes.",
}
# Counters of a revert report that add up to the number of restored records.
REVERT_COUNTS = (
    "registry_restored",
    "registry_deleted",
    "services_restored",
    "scheduled_tasks_restored",
    "appx_restored",
    "power_restored",
    "dns_restored",
    "task_definitions_deleted",
)
# Counters of the journal summary that add up to the number of revertible records.
JOURNAL_ACTIVE_COUNTS = (
    "registry_active",
    "services_active",
    "scheduled_tasks_active",
    "appx_active",
    "power_active",
    "dns_active",
    "task_definitions_active",
)
# Longest status bar message, in characters.
STATUS_LIMIT = 200

# An apply's values are read once more this long after it ended (the settled check); while
# another operation runs, the check waits the retry delay, up to the retry count.
SETTLED_CHECK_MS = 5000
SETTLED_RETRY_MS = 1000
SETTLED_RETRIES = 5
SETTLED_CHANGED_TEXT = "⚠ Checked again 5 seconds later: {settings} changed back ({titles}). See Optimize."
SETTLED_KEPT_TEXT = "✓ Checked again 5 seconds later: every value Cairn just set is still set."
# Apply results whose values did not hold when the engine read them back.
NOT_KEPT_TITLE = "{changes} not kept"
NOT_KEPT_TEXT = (
    "Cairn set these values and read them back half a second later, but Windows or another program "
    "had already changed them. Their originals stay recorded, so Undo restores them. Applying them "
    "again may be undone the same way."
)
NOT_KEPT_LINE = "• {title}: {label} is {found}, not {expected}"
# Added to an apply's result when it applied a tweak that requires Edge.
EDGE_REQUIREMENT = "edge"
EDGE_POLICY_TEXT = "Open edge://policy in Edge to see which policies it uses."
# Undo results whose restored values did not hold when the engine read them back.
NOT_RESTORED_TITLE = "Changes restored with issues"
NOT_RESTORED_ONE = (
    "Windows or another program changed 1 restored setting again right after Cairn put back the "
    "original. Cairn does not write it again."
)
NOT_RESTORED_MANY = (
    "Windows or another program changed {n} restored settings again right after Cairn put back the "
    "original. Cairn does not write them again."
)
NOT_RESTORED_LINE = "• {label} is {found}, not the original {expected}"
# A scan that found recorded values that no longer hold.
SCAN_DRIFT_TEXT = "Scan complete: {items} items checked in {ms} ms. ⚠ {count} {holds}; see History."
# A mode's confirmation when one of its changes cannot be fully undone.
UNDO_LIMIT_LINE = "⚠ {title}: {limit}"
# An Apply confirmation's line on a value an earlier version of Cairn set, which Apply restores.
FORMER_RESTORE_LINE = "• restore {target} to {original} (no longer part of this tweak)"
# History: Apply again and Keep as is.
APPLY_AGAIN_TITLE = "Apply {title} again?"
APPLY_AGAIN_TEXT = (
    "Cairn sets these values again. The originals from before Cairn's first change stay recorded, so "
    "Undo still restores them. If they change back again, Windows or another program may be managing "
    "this setting."
)
KEEP_TITLE = "Keep {title} as it is?"
KEEP_BACK_TEXT = (
    "These settings are back to the values they had before Cairn changed them. Cairn stops listing "
    "this change in History; nothing on this PC is changed."
)
KEEP_LOSES_TEXT = (
    "The current values stay as they are, and Cairn stops tracking this change. Undo can then no "
    "longer restore the values from before Cairn's change:"
)
# The same confirmations when the change has records the selection leaves out (Keep as is of
# the values an earlier version set, or of the records that changed): Cairn keeps tracking those.
KEEP_BACK_SOME_TEXT = (
    "These settings are back to the values they had before Cairn changed them. Cairn stops tracking "
    "them but still tracks the rest of this change, so History still lists it; nothing on this PC is "
    "changed."
)
KEEP_LOSES_SOME_TEXT = (
    "The current values stay as they are, and Cairn stops tracking them but still tracks the rest of "
    "this change. Undo can then no longer restore the values they had before Cairn's change:"
)
KEEP_LOSES_LINE = "• {label}: original {original}, now {found}"
KEEP_FORMER_LINE = "• {target}: original {original}"
UNKNOWN_ORIGINAL = "unknown"
KEEP_DONE_TEXT = "✓ {title}: no longer tracked. Nothing on this PC was changed."
# Keep as is of only some records of a change: the values an earlier version set, or the ones
# that changed.
KEEP_DONE_SOME_TEXT = (
    "✓ {title}: Cairn no longer tracks the values you kept and still tracks the rest of this change. "
    "Nothing on this PC was changed."
)
KEEP_REFUSED_TITLE = "Some changes are still tracked"
KEEP_REFUSED_TEXT = "Cairn still tracks these changes, so Undo can restore them:"
KEEP_PARTLY_TEXT = "⚠ {title}: some changes are still tracked. Nothing on this PC was changed."
# Sections whose search box Ctrl+F focuses, with the App method that focuses it.
FIND_HANDLERS = {"Optimize": "_focus_tweak_search"}

# What closing the window means for the operation that holds it busy (`App._set_busy`).
# Closing never stops an engine call that is already running: it finishes in the background
# and its result is not shown. A read or dry run changes nothing; a journaled change can be
# undone from History; an irreversible action (cleanup, network maintenance) cannot; "other"
# promises neither. The feature mixins pass these values as plain strings, since they cannot
# import this module.
CLOSE_READ = "read"
CLOSE_JOURNALED = "journaled"
CLOSE_IRREVERSIBLE = "irreversible"
CLOSE_OTHER = "other"
BUSY_CLOSE_TITLE = "An operation is still running"
CLOSE_IN_BACKGROUND = (
    "Closing the window doesn't stop it: it finishes in the background, and its result isn't shown."
)
CLOSE_TEXTS = {
    CLOSE_READ: "Cairn is reading information or checking what a change would do; no setting "
    "is being changed, so closing now is safe.",
    CLOSE_JOURNALED: f"{CLOSE_IN_BACKGROUND} Every change is recorded in the journal: when Cairn "
    "opens again, History lists what is still changed and can undo it.",
    CLOSE_IRREVERSIBLE: f"{CLOSE_IN_BACKGROUND} It can't be undone.",
    CLOSE_OTHER: CLOSE_IN_BACKGROUND,
}
VERSION_MISMATCH_TEXT = "⚠ The engine ({engine}) doesn't match this app ({app}); rebuild and deploy it."

# Whose account the window runs as (`App._account`), from the engine's check at start.
ACCOUNT_PENDING = "pending"
ACCOUNT_SAME = "same"
ACCOUNT_OTHER = "other"
ACCOUNT_UNKNOWN = "unknown"
# Start notes the launcher passes (`--start-note`) after asking for administrator rights.
NOTE_DECLINED = "elevation-declined"
NOTE_FAILED_PREFIX = "elevation-failed:"
OTHER_ACCOUNT_TITLE = "Running as another account"
OTHER_ACCOUNT_TEXT = (
    "Cairn was opened with another account's administrator rights, so it runs as that account, not "
    "as you. Changes to Windows as a whole work as usual, but your own settings (per-user tweaks, "
    "startup apps and Store apps) can't be changed from here: they would change the other account "
    "instead. To change your own settings, open Cairn from your own account; where administrator "
    "rights are needed, your account must be an administrator and approve the prompt itself."
)
UNCONFIRMED_ACCOUNT_TITLE = "Account not confirmed"
UNCONFIRMED_ACCOUNT_TEXT = (
    "Cairn couldn't confirm that it runs as the signed-in user, so it doesn't change per-user "
    "settings (per-user tweaks, startup apps and Store apps). Changes to Windows as a whole work "
    "as usual."
)
ELEVATION_FAILED_TITLE = "Couldn't get administrator rights"
ELEVATION_FAILED_TEXT = "Windows reported error {code} when Cairn asked for administrator rights."
NOT_INSTALLED_TITLE = "This copy isn't installed"
NOT_INSTALLED_TEXT = (
    "This copy of Cairn runs from a folder that programs without administrator rights can change. "
    "Code in that folder could be replaced, so this copy never asks for administrator rights and "
    "can't change Windows settings. Install Cairn with its setup program to make changes."
)
ADMIN_UNAVAILABLE_TITLE = "Administrator rights not available"
ADMIN_UNAVAILABLE_TEXT = (
    "Changing Windows settings, services, apps and files needs administrator rights. This copy of "
    "Cairn isn't installed, so it runs without them. Install Cairn with its setup program to make "
    "changes."
)
# Seconds between checks for a second start asking this window to come to the front.
ACTIVATION_POLL_SECONDS = 0.25
# Top bar spacing (logical px): around the badge, a margin inside it, and between the buttons.
BADGE_PADX = 12
BADGE_MARGIN = 8
RESTART_GAP = 10

# Hooks the window calls by name, looked up on every call so a test can replace one per
# window. A hook that raises is reported in `errors` and the others still run.
# Run before the window is built.
INIT_HOOKS = (
    "_init_system_info_state",
    "_init_network_state",
    "_init_tools_state",
    "_init_health_state",
    "_init_permissions_state",
    "_init_storage_state",
    "_init_updates_state",
    "_init_maintenance_state",
    "_init_profiles_state",
)
# Run whenever the busy state changes (`_set_busy`, and the start and end of a scan), to
# enable or disable each section's actions.
BUSY_HOOKS = (
    "_refresh_tool_actions",
    "_refresh_health_actions",
    "_refresh_permission_actions",
    "_refresh_storage_actions",
    "_refresh_updates_actions",
    "_refresh_maintenance_actions",
    "_refresh_profile_actions",
)
# Run after any journaled change or undo, once the window's own views are refreshed.
AFTER_MUTATION_HOOKS = (
    "_network_after_mutation",
    "_health_after_mutation",
    "_permissions_after_mutation",
    "_updates_after_mutation",
    "_maintenance_after_mutation",
    "_profiles_after_mutation",
)
# Each returns {journal target: title}, merged into the History titles when the catalog loads.
TITLE_PROVIDERS = ("_updates_history_titles",)
# Run once after the catalog is queued, only with an engine.
STARTUP_HOOKS = ("_maintenance_start_watch",)


class JobLane(NamedTuple):
    """Engine-owned background jobs of one feature, by the names of the App methods that
    follow them: `poll(now)` every frame, `poll_failed()` once after a poll raised,
    `allow_close() -> bool` before the window closes (False when it showed a dialog, whose
    confirmation calls `_continue_close(name, …)`), `before_shutdown()` before the engine
    shuts down and `running_title() -> str | None`. Network maintenance is refused while a
    lane with `blocks_network` runs a job."""

    name: str
    poll: str
    poll_failed: str
    allow_close: str
    before_shutdown: str
    running_title: str
    blocks_network: bool


# In the order of the status bar's job texts, of the close checks and of shutdown.
JOB_LANES = (
    JobLane(
        "tools",
        "_poll_tools",
        "_tools_poll_failed",
        "_tools_allow_close",
        "_tools_before_shutdown",
        "_running_tool_title",
        True,
    ),
    JobLane(
        "updates",
        "_poll_updates",
        "_updates_poll_failed",
        "_updates_allow_close",
        "_updates_before_shutdown",
        "_updates_running_title",
        True,
    ),
    JobLane(
        "storage",
        "_poll_storage",
        "_storage_poll_failed",
        "_storage_allow_close",
        "_storage_before_shutdown",
        "_running_storage_title",
        False,
    ),
    JobLane(
        "maintenance",
        "_poll_maintenance",
        "_maintenance_poll_failed",
        "_maintenance_allow_close",
        "_maintenance_before_shutdown",
        "_maintenance_running_title",
        False,
    ),
    JobLane(
        "security",
        "_poll_health",
        "_health_poll_failed",
        "_health_allow_close",
        "_health_before_shutdown",
        "_health_running_title",
        False,
    ),
)


class BusyClose(NamedTuple):
    """What closing the window means for the running operation: a `CLOSE_*` kind, the
    operation's name for the dialog ("The network stack reset") and an extra sentence."""

    kind: str = CLOSE_OTHER
    action: str = ""
    note: str = ""


def busy_close_text(close: BusyClose) -> str:
    """Message of the dialog shown when the window closes while `close` describes the
    running operation."""
    parts = [
        f"{close.action} is still running." if close.action else "",
        CLOSE_TEXTS.get(close.kind, CLOSE_IN_BACKGROUND),
        close.note,
    ]
    return " ".join(p for p in parts if p)


class BadgeState(NamedTuple):
    """The top bar's account badge: a long and a short text (the one that fits shows), its
    colour and, when clicking it explains something, the explanation's title and text."""

    long: str
    short: str
    color: str
    explanation_title: str = ""
    explanation: str = ""


def badge_state(elevated: bool, account: str, note: str, can_elevate: bool) -> BadgeState:
    """The account badge for the window's rights: `account` is one of the `ACCOUNT_*` values
    (pending counts as the signed-in user), `note` the launcher's start note ("" for none) and
    `can_elevate` False for a copy that may not ask for administrator rights."""
    if elevated:
        if account == ACCOUNT_OTHER:
            return BadgeState(
                "⚠ Administrator  ·  another account",
                "⚠ Another account",
                theme.WARNING,
                OTHER_ACCOUNT_TITLE,
                OTHER_ACCOUNT_TEXT,
            )
        if account == ACCOUNT_UNKNOWN:
            return BadgeState(
                "⚠ Administrator  ·  account not confirmed",
                "⚠ Account not confirmed",
                theme.WARNING,
                UNCONFIRMED_ACCOUNT_TITLE,
                UNCONFIRMED_ACCOUNT_TEXT,
            )
        return BadgeState("✓ Running as administrator", "✓ Administrator", theme.GOOD)
    short = "⚠ Standard user"
    if not can_elevate:
        return BadgeState(
            f"{short}  ·  this copy isn't installed",
            short,
            theme.WARNING,
            NOT_INSTALLED_TITLE,
            NOT_INSTALLED_TEXT,
        )
    if note == NOTE_DECLINED:
        return BadgeState(f"{short}  ·  administrator rights weren't granted", short, theme.WARNING)
    if note.startswith(NOTE_FAILED_PREFIX):
        return BadgeState(
            f"{short}  ·  couldn't get administrator rights",
            short,
            theme.WARNING,
            ELEVATION_FAILED_TITLE,
            ELEVATION_FAILED_TEXT.format(code=note[len(NOTE_FAILED_PREFIX) :]),
        )
    return BadgeState(f"{short}  ·  most changes need administrator rights", short, theme.WARNING)


def plural(count: int, noun: str) -> str:
    return f"{count} {noun}{'' if count == 1 else 's'}"


def status_line(text: str, limit: int = STATUS_LIMIT) -> str:
    """The first non-empty line of `text`, cut to `limit` characters, for the one-line status bar.

    Engine messages can span several lines (a PowerShell error record, for example); a
    trailing "…" marks text that was left out.
    """
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    if not lines:
        return ""
    first = lines[0]
    if len(first) > limit:
        return first[: limit - 1].rstrip() + "…"
    return first + " …" if len(lines) > 1 else first


def apply_summary(kept: int, not_kept: int, skipped: int, failed: int) -> str:
    """ "3 applied, 1 not kept, 1 skipped, 1 failed": the applied count always, the others when
    they are not zero."""
    summary = f"{kept} applied"
    for count, text in ((not_kept, "not kept"), (skipped, "skipped"), (failed, "failed")):
        if count:
            summary += f", {count} {text}"
    return summary


def not_restored_text(count: int) -> str:
    """The sentence on `count` restored settings that did not hold when read back."""
    return NOT_RESTORED_ONE if count == 1 else NOT_RESTORED_MANY.format(n=count)


def former_values(record: dict[str, Any]) -> dict[str, str]:
    """The target and original of a scan item's `former` entry, for `FORMER_RESTORE_LINE`."""
    return {"target": str(record.get("target", "")), "original": str(record.get("original", ""))}


def apply_outcomes(
    results: Iterable[dict[str, Any]],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    """An apply report's results as (kept, not kept, skipped, failed).

    A result counts as applied only when every value it wrote held when the engine read it
    back; one whose values did not hold (`not_kept`, an applied or already-applied result) is
    counted as not kept."""
    kept, not_kept, skipped, failed = [], [], [], []
    for result in results:
        outcome = result.get("outcome")
        if outcome in ("applied", "already_applied") and result.get("not_kept"):
            not_kept.append(result)
        elif outcome == "applied":
            kept.append(result)
        elif outcome == "skipped":
            skipped.append(result)
        elif outcome == "failed":
            failed.append(result)
    return kept, not_kept, skipped, failed


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


class App(
    SystemInfoFeature,
    NetworkFeature,
    ToolsFeature,
    HealthFeature,
    PermissionsFeature,
    StorageFeature,
    UpdatesFeature,
    MaintenanceFeature,
    ProfilesFeature,
    ctk.CTk,
):
    def __init__(
        self,
        *,
        telemetry_factory: Callable[[], Telemetry] = Telemetry,
        engine: EngineBridge | None = None,
        elevated: bool | None = None,
        autoscan: bool = True,
        instance: InstanceLock | None = None,
        can_elevate: bool = True,
        start_note: str = "",
    ) -> None:
        """`instance` is the single-instance lock `__main__` holds for this window (activation
        requests bring it to the front); `can_elevate` is False for a copy that may not ask
        for administrator rights (it runs from a folder that is not admin-only), and
        `start_note` says why the launcher's request for them did not succeed."""
        ctk.set_appearance_mode("dark")
        super().__init__(fg_color=theme.PAGE)
        self.title(APP_NAME)
        # Also the default of every window opened later; Tk frees it with the window.
        set_window_icon(self, default=True)
        self._fit_to_screen(preferred=(1440, 900), minimum=(1120, 700))

        self.elevated = system.is_admin() if elevated is None else elevated
        self.errors: list[str] = []
        self._frame_times: deque[float] = deque(maxlen=240)
        self._next_frame = 0.0
        self._running = True
        self._busy = False
        # A cleanup measurement is queued or running; opening the section again does not queue another.
        self._cleanup_scanning = False
        # What closing the window means for the operation holding `_busy`.
        self._busy_close = BusyClose()
        # The last scan while it still describes the system; a change clears it.
        self._last_scan: dict[str, Any] | None = None
        self._scan_failed = False
        self._startup_names: dict[str, str] = {}
        self._last_status_update = 0.0
        self._telemetry: Telemetry | None = None
        self._telemetry_error: str | None = None
        self._last_sequence = 0
        self._sample_times: deque[float] = deque(maxlen=240)
        self._titles: dict[str, str] = {}
        # The single-instance lock `__main__` holds for this window, if any.
        self.instance: InstanceLock | None = instance
        self.can_elevate = can_elevate
        self.start_note = start_note
        # Whose account the window runs as; the engine's answer arrives after the start.
        self._account = ACCOUNT_PENDING
        # The installed copy, for the About dialog of a development copy.
        self._install_info: Any = UNKNOWN_COPY
        self._next_activation_poll = 0.0
        # Sections whose builder only shows that the section is not available yet.
        self._stub_sections: set[str] = set()
        self._current_section = FIRST
        self._section_frames: dict[str, ctk.CTkFrame] = {}
        # The job lanes this window follows (`JOB_LANES`; a test may add a lane per window).
        self._job_lanes: tuple[JobLane, ...] = JOB_LANES
        # Job lanes whose poll raised: each reports once and is not polled again.
        self._lane_failed: set[str] = set()
        # Status bar text of each job lane.
        self._job_texts: dict[str, str] = {}
        # Job lanes whose close dialog was confirmed during the current attempt to close.
        self._close_confirmed: set[str] = set()
        # From the catalog: titles by tweak id, the tweaks that require Edge, retired tweaks,
        # what Undo cannot bring back by tweak id, and the targets (lower case) of values
        # earlier versions of Cairn set, with the tweaks they belong to.
        self._item_titles: dict[str, str] = {}
        self._edge_ids: set[str] = set()
        self._retired_ids: set[str] = set()
        self._catalog_undo_limits: dict[str, str] = {}
        self._former_targets: set[str] = set()
        self._former_ids: set[str] = set()
        # From the last scan: the tweaks this PC cannot use (retired ones included), which an
        # apply skips.
        self._unavailable_ids: set[str] = set()
        # Counts the status bar's messages, so a later message can tell whether an earlier
        # one is still shown.
        self._status_serial = 0
        # A scan that refreshes the views after a change keeps the change's status message.
        self._scan_refresh = False
        # The settled check: its timer, the tweaks it reads, the busy retries so far, which
        # apply it belongs to, the status message of that apply, and whether that apply
        # restored values an earlier version of Cairn set, which the check cannot read.
        self._settled_job: str | None = None
        self._settled_ids: list[str] = []
        self._settled_tries = 0
        self._settled_serial = 0
        self._settled_status = 0
        self._settled_former = False
        # Which History load a journal export or drift report belongs to.
        self._history_serial = 0

        try:
            self.engine: EngineBridge | None = engine if engine is not None else EngineBridge()
        except EngineUnavailable as exc:
            self.engine = None
            self.errors.append(str(exc))

        self._run_hooks(INIT_HOOKS)
        self._build()
        self._start_telemetry(telemetry_factory)
        self.protocol("WM_DELETE_WINDOW", self._request_close)
        self._frame_job: str | None = self.after(1, self._frame)
        if self.engine is not None:
            self.engine.catalog(callback=self._catalog_loaded)
            # Queued: the account lookup can wait on the network (a domain controller).
            if self.elevated and self.engine.supports("elevated_as_other_user"):
                self.engine.elevated_as_other_user(callback=self._account_checked)
            if system.installed_launcher() is None and self.engine.supports("install_info"):
                self.engine.install_info(callback=self._install_info_loaded)
            self._run_hooks(STARTUP_HOOKS)
            if autoscan:
                self.after(300, self.start_scan)

    def _run_hooks(self, names: Iterable[str]) -> None:
        """Calls the App methods named `names` in order; one that raises is reported in
        `errors` and the others still run."""
        for name in names:
            try:
                getattr(self, name)()
            except Exception:  # noqa: BLE001 - one feature's failure must not stop the others
                self.errors.append(traceback.format_exc())
                log.exception("%s failed", name)

    # -- layout --------------------------------------------------------------------

    def _fit_to_screen(self, preferred: tuple[int, int], minimum: tuple[int, int]) -> None:
        """Sizes the window to the preferred size or the work area, whichever is smaller.

        CustomTkinter scales geometry by the display's DPI factor, so screen pixels are
        converted to its logical units first. The taskbar is allowed for with a margin.
        """
        scale = ctk.ScalingTracker.get_window_scaling(self) or 1.0
        screen_w = self.winfo_screenwidth() / scale
        screen_h = self.winfo_screenheight() / scale
        width = int(min(preferred[0], screen_w - 40))
        height = int(min(preferred[1], screen_h - 90))
        x = int(max((screen_w - width) / 2, 0))
        y = int(max((screen_h - 48 - height) / 2, 0))
        self.geometry(f"{width}x{height}+{x}+{y}")
        self.minsize(min(minimum[0], width), min(minimum[1], height))

    def _build(self) -> None:
        """Sidebar in column 0 over the top bar's and the sections' rows; the status bar spans
        both columns. Every section is built once, into its own frame; only the shown one is
        gridded."""
        self.grid_columnconfigure(0, weight=0)
        self.grid_columnconfigure(1, weight=1)
        self.grid_rowconfigure(1, weight=1)
        self._build_top_bar()
        # The status bar exists before any section is built; section builders still report
        # only through their own panels.
        self._build_status_bar()
        self.nav = Sidebar(
            self,
            sections=SECTIONS,
            version=__version__,
            on_select=self.show_section,
            on_about=self.show_about,
        )
        self.nav.grid(row=0, column=0, rowspan=2, sticky="nsw")
        self.brand = self.nav.brand

        self.section_host = ctk.CTkFrame(self, fg_color=theme.PAGE, corner_radius=0)
        self.section_host.grid(row=1, column=1, sticky="nsew", padx=(8, 12), pady=(0, 6))
        self.section_host.grid_columnconfigure(0, weight=1)
        self.section_host.grid_rowconfigure(0, weight=1)
        for s in SECTIONS:
            frame = ctk.CTkFrame(self.section_host, fg_color=theme.PAGE, corner_radius=0)
            frame.grid_columnconfigure(0, weight=1)
            frame.grid_rowconfigure(0, weight=1)
            self._section_frames[s.name] = frame
        # Each builder disables its own actions when the engine is missing.
        for s in SECTIONS:
            frame = self._section_frames[s.name]
            builder = getattr(self, s.build, None)
            if callable(builder):
                builder(frame)
            else:
                log.warning("section %s has no builder %s", s.name, s.build)
                self._build_placeholder(frame)
        self._section_frames[self._current_section].grid(row=0, column=0, sticky="nsew")
        self.nav.select(self._current_section)

        if self.engine is None:
            for panel in (self.optimize_list, self.apps_list):
                panel.set_scan_enabled(False)
            self.cleanup_panel.scan_button.configure(state="disabled")
            self.startup_panel.refresh_button.configure(state="disabled")
            self.history_panel.refresh_button.configure(state="disabled")

        mismatch = self.engine is not None and self.engine.version != __version__
        self.nav.set_version_warning(mismatch)
        if self.errors:
            self.set_status("⚠ " + "; ".join(self.errors), theme.WARNING)
        elif mismatch and self.engine is not None:
            self.set_status(
                VERSION_MISMATCH_TEXT.format(engine=self.engine.version, app=__version__), theme.WARNING
            )

        for sequence, step in (
            ("<Control-Tab>", 1),
            ("<Control-Next>", 1),
            ("<Control-Shift-Tab>", -1),
            ("<Control-Prior>", -1),
        ):
            self.bind(sequence, lambda _event, step=step: self._on_step_key(step))
        for sequence in ("<Control-f>", "<Control-F>"):
            self.bind(sequence, self._on_find_key)
        self.bind("<Configure>", self._on_window_configure, add="+")

    def _build_status_bar(self) -> None:
        """Message on the left, then the running jobs, the journal state and the frame rate."""
        status = ctk.CTkFrame(self, fg_color=theme.SURFACE, corner_radius=0, height=28)
        status.grid(row=2, column=0, columnspan=2, sticky="ew")
        status.grid_columnconfigure(0, weight=1)
        self.status_message = ctk.CTkLabel(
            status, text="Starting…", font=_font(11), text_color=theme.INK_SECONDARY, anchor="w"
        )
        self.status_message.grid(row=0, column=0, sticky="w", padx=18, pady=4)
        self.status_tool = ctk.CTkLabel(status, text="", font=_font(11), text_color=theme.INK_SECONDARY)
        self.status_tool.grid(row=0, column=1, sticky="e", padx=12)
        self.status_journal = ctk.CTkLabel(status, text="", font=_font(11), text_color=theme.INK_MUTED)
        self.status_journal.grid(row=0, column=2, sticky="e", padx=12)
        self.status_rate = ctk.CTkLabel(status, text="", font=_font(11), text_color=theme.INK_MUTED)
        self.status_rate.grid(row=0, column=3, sticky="e", padx=18)

    def _build_top_bar(self) -> None:
        """The shown section's title, the account badge, "Restart as administrator" for a
        standard user whose copy may ask for administrator rights, and Revert All Changes.
        F1 opens About."""
        bar = ctk.CTkFrame(self, fg_color="transparent")
        bar.grid(row=0, column=1, sticky="ew", padx=16, pady=(12, 6))
        bar.grid_columnconfigure(1, weight=1)
        self.top_bar = bar
        self.section_title = ctk.CTkLabel(
            bar, text=self._current_section, font=_font(18, "bold"), text_color=theme.INK, anchor="w"
        )
        self.section_title.grid(row=0, column=0, sticky="w")
        self._badge = badge_state(self.elevated, self._account, self.start_note, self.can_elevate)
        self._badge_shown = ""
        self._badge_fonts: dict[float, tuple[tkfont.Font, tkfont.Font]] = {}
        self.account_badge = ctk.CTkLabel(bar, text="", font=_font(12, "bold"), text_color=self._badge.color)
        self.account_badge.bind("<Button-1>", self._badge_clicked)
        column = 3
        self.restart_button: ctk.CTkButton | None = None
        if not self.elevated and self.can_elevate:
            self.restart_button = ctk.CTkButton(
                bar,
                text="Restart as administrator",
                width=190,
                height=32,
                font=_font(12, "bold"),
                fg_color=theme.ACCENT,
                hover_color=theme.ACCENT_HOVER,
                command=self._relaunch_elevated,
            )
            self.restart_button.grid(row=0, column=column, sticky="e", padx=(0, RESTART_GAP))
            column += 1
        self.revert_button = ctk.CTkButton(
            bar,
            text="Revert All Changes",
            width=190,
            height=32,
            font=_font(13, "bold"),
            fg_color=theme.CRITICAL,
            hover_color=theme.CRITICAL_HOVER,
            text_color=theme.INK,
            state="normal" if self._mutations_enabled else "disabled",
            command=self._on_revert_all,
        )
        self.revert_button.grid(row=0, column=column, sticky="e")
        tk.Frame.bind(bar, "<Configure>", self._fit_badge, add="+")
        self._refresh_badge()
        self.bind("<F1>", self._on_about_key)

    # -- account badge -------------------------------------------------------------

    def _refresh_badge(self) -> None:
        """Recomputes the badge from the window's rights and account, then fits it."""
        self._badge = badge_state(self.elevated, self._account, self.start_note, self.can_elevate)
        self.account_badge.configure(
            text_color=self._badge.color, cursor="hand2" if self._badge.explanation else ""
        )
        self._badge_shown = ""
        self._fit_badge()

    def _badge_fonts_for(self, scale: float) -> tuple[tkfont.Font, tkfont.Font]:
        """The badge's and the section title's fonts at display scale `scale`, for measuring."""
        fonts = self._badge_fonts.get(scale)
        if fonts is None:
            fonts = (
                tkfont.Font(root=self, family=theme.FONT_FAMILY, size=-round(12 * scale), weight="bold"),
                tkfont.Font(root=self, family=theme.FONT_FAMILY, size=-round(18 * scale), weight="bold"),
            )
            self._badge_fonts[scale] = fonts
        return fonts

    def _fit_badge(self, _event: Any = None) -> None:
        """Shows the long badge text, the short one or none, whichever fits between the
        longest section title and the buttons."""
        bar = self.top_bar
        scale = ctk.ScalingTracker.get_widget_scaling(bar) or 1.0
        badge_font, title_font = self._badge_fonts_for(scale)
        title = max(title_font.measure(s.name) for s in SECTIONS)
        buttons = [w for w in (self.restart_button, self.revert_button) if w is not None]
        gaps = BADGE_PADX * 2 + BADGE_MARGIN + (RESTART_GAP if self.restart_button is not None else 0)
        available = bar.winfo_width() - title - sum(w.winfo_reqwidth() for w in buttons) - round(gaps * scale)
        fit = badge_fit(
            available, badge_font.measure(self._badge.long), badge_font.measure(self._badge.short)
        )
        text = {"long": self._badge.long, "short": self._badge.short}.get(fit, "")
        if text == self._badge_shown:
            return
        self._badge_shown = text
        if text:
            self.account_badge.configure(text=text)
            self.account_badge.grid(row=0, column=2, sticky="e", padx=BADGE_PADX)
        else:
            self.account_badge.grid_forget()

    def _badge_clicked(self, _event: Any = None) -> None:
        if self._badge.explanation:
            MessageDialog(
                self,
                title=self._badge.explanation_title,
                message=self._badge.explanation,
                confirm_text="Close",
            )

    def _account_checked(self, future: Future[Any]) -> None:
        if future.exception() is not None:
            log.warning("the account check failed: %s", future.exception())
            self._account = ACCOUNT_UNKNOWN
        else:
            self._account = ACCOUNT_OTHER if future.result() else ACCOUNT_SAME
        self._refresh_badge()

    def _install_info_loaded(self, future: Future[Any]) -> None:
        if future.exception() is not None:
            log.warning("the installed copy could not be read: %s", future.exception())
            return
        self._install_info = future.result()

    def _build_placeholder(self, frame: ctk.CTkFrame) -> None:
        """Fills the frame of a section whose builder is missing."""
        build_placeholder(frame)

    def _build_dashboard_tab(self, frame: ctk.CTkFrame) -> None:
        self.monitor = HardwareMonitor(frame)
        self.monitor.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)

    def _build_optimize_tab(self, frame: ctk.CTkFrame) -> None:
        frame.grid_columnconfigure(0, weight=0, minsize=430)
        frame.grid_columnconfigure(1, weight=1)
        modes = ctk.CTkScrollableFrame(
            frame,
            fg_color=theme.SURFACE,
            corner_radius=10,
            border_width=1,
            border_color=theme.BORDER,
            width=410,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        modes.grid(row=0, column=0, sticky="nsew", padx=(4, 5), pady=4)
        modes.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            modes,
            text="Optimization modes",
            font=_font(13, "bold"),
            text_color=theme.INK_SECONDARY,
            anchor="w",
        ).grid(row=0, column=0, sticky="w", padx=8, pady=(6, 2))
        ctk.CTkLabel(
            modes,
            text="Each mode applies its recommended changes. Turning a mode off restores the originals "
            "of its recommended changes, however they were applied; other tweaks applied from the "
            "list are kept.",
            font=_font(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=380,
        ).grid(row=1, column=0, sticky="w", padx=8, pady=(0, 6))
        self.toggles: dict[str, CategoryToggle] = {}
        for row, (category, title) in enumerate(CATEGORY_MODES.items(), start=2):
            toggle = CategoryToggle(
                modes, category, title, self._on_toggle, on_apply_remaining=self._on_apply_remaining
            )
            toggle.grid(row=row, column=0, sticky="ew", padx=4, pady=(0, 8))
            toggle.set_enabled(self._mutations_enabled)
            self.toggles[category] = toggle

        self.optimize_list = ItemListPanel(
            frame,
            title="All tweaks",
            categories=TWEAK_CATEGORIES,
            on_scan=self.start_scan,
            on_action=self._on_item_action,
            search=True,
        )
        self.optimize_list.grid(row=0, column=1, sticky="nsew", padx=(5, 4), pady=4)

    def _build_apps_tab(self, frame: ctk.CTkFrame) -> None:
        self.apps_list = ItemListPanel(
            frame,
            title="Preinstalled apps",
            categories=("bloatware",),
            on_scan=self.start_scan,
            on_action=self._on_item_action,
            bulk_label="Remove recommended",
            on_bulk=self._on_bulk_apply,
            empty_text="No known bloatware apps are installed.",
        )
        self.apps_list.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)

    def _build_cleanup_tab(self, frame: ctk.CTkFrame) -> None:
        self.cleanup_panel = CleanupPanel(frame, on_scan=self.start_cleanup_scan, on_clean=self._on_clean)
        self.cleanup_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)

    def _build_startup_tab(self, frame: ctk.CTkFrame) -> None:
        self.startup_panel = StartupPanel(
            frame, on_refresh=self.load_startup, on_toggle=self._on_startup_toggle
        )
        self.startup_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)

    def _build_history_tab(self, frame: ctk.CTkFrame) -> None:
        self.history_panel = HistoryPanel(
            frame,
            on_refresh=self.load_history,
            on_undo=self._on_history_undo,
            on_apply_again=self._on_history_apply_again,
            on_keep=self._on_history_keep,
        )
        self.history_panel.grid(row=0, column=0, sticky="nsew", padx=4, pady=4)

    @property
    def _mutations_enabled(self) -> bool:
        return self.elevated and self.engine is not None

    def set_status(self, text: str, color: str = theme.INK_SECONDARY) -> None:
        self._status_serial += 1
        self.status_message.configure(text=status_line(text), text_color=color)

    # -- navigation ------------------------------------------------------------------

    @property
    def current_section(self) -> str:
        """Name of the section that is shown."""
        return self._current_section

    def section_visible(self, name: str) -> bool:
        """Whether section `name` is the one shown."""
        return self._current_section == name

    def section_frame(self, name: str) -> ctk.CTkFrame:
        """The frame section `name` is built into; KeyError for an unknown name."""
        section(name)
        return self._section_frames[name]

    def show_section(self, name: str, *, run_hook: bool = True) -> None:
        """Shows section `name` (KeyError for an unknown one) and, with `run_hook`, runs its
        shown hook, also when it is already shown, so showing History again reloads it.

        The new frame is gridded before the old one is forgotten, so the window never
        shows an empty section. `run_hook=False` lays a section out without loading it.
        """
        section(name)
        if name != self._current_section:
            self._section_frames[name].grid(row=0, column=0, sticky="nsew")
            self._section_frames[self._current_section].grid_forget()
            self._current_section = name
            self.nav.select(name)
            self.section_title.configure(text=name)
        if run_hook:
            self._on_section_shown(name)

    def _on_section_shown(self, name: str) -> None:
        """Runs the section's shown hook; nothing loads without an engine."""
        if self.engine is None:
            return
        hook = section(name).shown
        if hook:
            getattr(self, hook)()

    def _step_section(self, step: int) -> None:
        """Shows the section `step` rows below (negative: above) the shown one, wrapping around."""
        names = [s.name for s in SECTIONS]
        index = names.index(self._current_section)
        self.show_section(names[(index + step) % len(names)])

    def _on_step_key(self, step: int) -> str:
        self._step_section(step)
        return "break"

    def _on_find_key(self, _event: Any = None) -> str | None:
        """Ctrl+F: focuses the search box of the shown section, when it has one (see
        `FIND_HANDLERS`); in every other section the key does nothing here."""
        handler = FIND_HANDLERS.get(self._current_section)
        if handler is None:
            return None
        getattr(self, handler)()
        return "break"

    def _focus_tweak_search(self) -> None:
        self.optimize_list.focus_search()

    def _on_window_configure(self, event: Any) -> None:
        # The root's bindings also see every child's events.
        if event.widget is not self:
            return
        scale = ctk.ScalingTracker.get_window_scaling(self) or 1.0
        self.nav.set_mode(sidebar_mode(self.winfo_width() / scale))

    def show_about(self) -> None:
        """Shows the versions of every part, where this copy runs from and where its data is,
        with buttons that open the data folder and the project page."""
        launcher = system.installed_launcher()
        if launcher is not None:
            install = INSTALLED if self.can_elevate else NOT_INSTALLED
            location = str(launcher.parent)
        else:
            install, location = DEVELOPMENT, str(PACKAGE_DIR.parent)
        data_dir = self._data_dir()
        telemetry = self._telemetry
        info = AboutInfo(
            app=__version__,
            engine=self.engine.version if self.engine is not None else None,
            telemetry=(telemetry.version or None) if telemetry is not None else None,
            python=platform.python_version(),
            tk=str(self.tk.call("info", "patchlevel")),
            ctk=str(ctk.__version__),
            install=install,
            location=location,
            data_dir=str(data_dir),
            installed_copy=self._install_info,
        )
        MessageDialog(
            self,
            title=ABOUT_TITLE,
            message=about_message(__version__),
            details=about_lines(info),
            confirm_text="Close",
            links=[
                (OPEN_DATA_TEXT, lambda: self._open_data_folder(data_dir)),
                (PROJECT_PAGE_TEXT, self._open_project_page),
            ],
        )

    def _on_about_key(self, _event: Any = None) -> str:
        self.show_about()
        return "break"

    def _data_dir(self) -> Path:
        """The folder of the journal the engine uses, else the default data folder."""
        journal = self.engine.journal_path() if self.engine is not None else None
        return Path(journal).parent if journal else paths.data_dir()

    def _open_data_folder(self, folder: Path) -> None:
        if not folder.is_dir():
            self.set_status(NO_DATA_YET_TEXT, theme.INK_SECONDARY)
            return
        try:
            system.open_folder(folder)
        except OSError as exc:
            self.set_status(f"The data folder could not be opened: {exc}", theme.WARNING)

    def _open_project_page(self) -> None:
        # The link opener keeps the browser unelevated when Cairn runs as administrator.
        self._open_link(PROJECT_URL)

    def _cleanup_tab_shown(self) -> None:
        # The measurement is read-only, so it queues behind a running operation on the worker.
        if not self.cleanup_panel.scanned and not self._cleanup_scanning:
            self.start_cleanup_scan()

    def _startup_tab_shown(self) -> None:
        if not self.startup_panel.loaded:
            self.load_startup()

    def _history_tab_shown(self) -> None:
        self.load_history()

    def _catalog_loaded(self, future: Future[Any]) -> None:
        if future.exception() is not None:
            return
        titles: dict[str, str] = {}
        for entry in future.result():
            title = entry["title"]
            for target in entry["targets"]:
                titles[target] = title
            # Values earlier versions of Cairn set for the tweak; their records are its own.
            for target in entry.get("former_targets") or []:
                titles[target] = title
                self._former_targets.add(str(target).lower())
                self._former_ids.add(entry["id"])
            self._item_titles[entry["id"]] = title
            if entry.get("requires") == EDGE_REQUIREMENT:
                self._edge_ids.add(entry["id"])
            if entry.get("retired"):
                self._retired_ids.add(entry["id"])
            if entry.get("undo_limit"):
                self._catalog_undo_limits[entry["id"]] = str(entry["undo_limit"])
        for name in TITLE_PROVIDERS:
            try:
                titles.update(getattr(self, name)())
            except Exception:  # noqa: BLE001 - the catalog's titles are kept
                self.errors.append(traceback.format_exc())
                log.exception("%s failed", name)
        self._titles = titles

    # -- background job lanes ------------------------------------------------------

    @property
    def _tool_poll_failed(self) -> bool:
        """Whether polling the tools lane stopped after a failure."""
        return "tools" in self._lane_failed

    @_tool_poll_failed.setter
    def _tool_poll_failed(self, failed: bool) -> None:
        if failed:
            self._lane_failed.add("tools")
        else:
            self._lane_failed.discard("tools")

    def set_job_status(self, lane: str, text: str) -> None:
        """Sets the status bar text of job lane `lane` ("" clears it); the texts of every lane
        are shown together in lane order."""
        self._job_texts[lane] = text
        order = [lane.name for lane in self._job_lanes]
        order += [name for name in self._job_texts if name not in order]
        joined = "  ·  ".join(self._job_texts[name] for name in order if self._job_texts.get(name))
        set_text(self.status_tool, joined)

    def _running_job_title(self, *, network_only: bool = False) -> str | None:
        """Title of the first running job in lane order, or None; with `network_only`, only
        lanes whose jobs network maintenance must wait for."""
        for lane in self._job_lanes:
            if network_only and not lane.blocks_network:
                continue
            try:
                title = getattr(self, lane.running_title)()
            except Exception:  # noqa: BLE001 - a broken lane must not block every other check
                self.errors.append(traceback.format_exc())
                log.exception("%s failed", lane.running_title)
                continue
            if title is not None:
                return title
        return None

    def _poll_lanes(self, now: float) -> None:
        """Polls every job lane that has not failed; a lane whose poll raises is reported once
        and stops being polled."""
        for lane in self._job_lanes:
            if lane.name in self._lane_failed:
                continue
            try:
                getattr(self, lane.poll)(now)
            except Exception:  # noqa: BLE001 - a failing poll must not stop the frame loop
                self.errors.append(traceback.format_exc())
                log.exception("%s poll failed", lane.name)
                self._lane_failed.add(lane.name)
                try:
                    getattr(self, lane.poll_failed)()
                except Exception:  # noqa: BLE001 - the failure is already reported
                    self.errors.append(traceback.format_exc())
                    log.exception("%s failed", lane.poll_failed)

    # -- telemetry and frame loop --------------------------------------------------

    def _start_telemetry(self, factory: Callable[[], Telemetry]) -> None:
        """Starts the sampler on the Tk thread before the window is shown.

        tel_init takes one synchronous sample (about 0.1 s, most of it the first
        performance-counter query in the process). Starting it here keeps ownership of the
        process-global sampler with the window: a helper thread could outlive a window
        that closes early, and a Tk thread waiting on such a helper can deadlock when that
        helper runs garbage collection that finalizes Tk objects, because those finalizers
        wait for the Tk thread.
        """
        try:
            tel = factory()
            tel.start()
        except (TelemetryError, OSError) as exc:
            self._telemetry_error = str(exc)
            self.monitor.show_message(f"Telemetry unavailable: {exc}")
            return
        self._telemetry = tel

    def _frame(self) -> None:
        if not self._running:
            return
        now = time.perf_counter()
        self._frame_times.append(now)
        try:
            self._render(now)
            # A minimized window keeps its last pose; nothing renders frames it never shows.
            if self.state() != "iconic":
                self.brand.tick(now)
        except Exception:  # noqa: BLE001 - one bad frame must not stop the loop
            self.errors.append(traceback.format_exc())
            log.exception("frame render failed")
        try:
            self._poll_activation(now)
        except Exception:  # noqa: BLE001 - a failing activation check must not stop the loop
            self.errors.append(traceback.format_exc())
            log.exception("activation check failed")
        if self.engine is not None:
            try:
                self.engine.dispatch_completed()
            except Exception:  # noqa: BLE001 - a failing callback must not stop the frame loop
                self.errors.append(traceback.format_exc())
                log.exception("engine callback failed")
            if not self._running:
                # A callback closed the window.
                return
            self._poll_lanes(now)
        # Fixed-rate schedule with resynchronisation after a stall. The timer is set once the
        # redraws this frame queued have run: set here, a redraw slower than the frame period
        # would find the next frame already due, and `update()` would never run out of events.
        self._next_frame = max(self._next_frame + 1.0 / FRAME_HZ, now)
        self._frame_job = self.after_idle(self._arm_frame)

    def _arm_frame(self) -> None:
        if not self._running:
            return
        delay_ms = int((self._next_frame - time.perf_counter()) * 1000)
        if delay_ms < 1:
            self._next_frame = time.perf_counter()
            delay_ms = 1
        self._frame_job = self.after(delay_ms, self._frame)

    def _poll_activation(self, now: float) -> None:
        """Brings the window to the front when a second start asked for it."""
        if now < self._next_activation_poll:
            return
        self._next_activation_poll = now + ACTIVATION_POLL_SECONDS
        # Closing needs only `stop_activation` of the lock; one without an activation check
        # never asks for the window.
        requested = getattr(self.instance, "activation_requested", None)
        if requested is not None and requested():
            self._bring_to_front()

    def _bring_to_front(self) -> None:
        self.deiconify()
        self.lift()
        system.bring_window_to_front(int(self.wm_frame(), 16))

    def _render(self, now: float) -> None:
        tel = self._telemetry
        if tel is None:
            if self._telemetry_error:
                self.monitor.show_message(f"Telemetry unavailable: {self._telemetry_error}")
                self._telemetry_error = None
            return
        snapshot = tel.snapshot()
        if snapshot.sequence != self._last_sequence:
            self._last_sequence = snapshot.sequence
            self._sample_times.append(now)
        self.monitor.update_frame(snapshot, now, visible=self.section_visible("Dashboard"))
        if now - self._last_status_update >= 0.5:
            self._last_status_update = now
            self.status_rate.configure(
                text=f"UI {self.fps:.0f} fps  ·  telemetry {self._rate(self._sample_times):.0f} Hz"
            )

    @staticmethod
    def _rate(times: deque[float]) -> float:
        if len(times) < 2:
            return 0.0
        span = times[-1] - times[0]
        return (len(times) - 1) / span if span > 0 else 0.0

    @property
    def fps(self) -> float:
        recent = (
            [t for t in self._frame_times if self._frame_times[-1] - t <= 1.0] if self._frame_times else []
        )
        if len(recent) < 2:
            return 0.0
        return (len(recent) - 1) / (recent[-1] - recent[0])

    # -- scan ----------------------------------------------------------------------

    def start_scan(self, *, refresh: bool = False) -> None:
        """Scans every tweak and Store app. A `refresh` (the rescan after a change or a check)
        leaves the status bar's message as it is unless the scan fails or warns."""
        if self.engine is None or self._busy:
            return
        self._busy = True
        self._busy_close = BusyClose(CLOSE_READ)
        self._scan_refresh = refresh
        for panel in (self.optimize_list, self.apps_list):
            panel.set_scanning()
        for toggle in self.toggles.values():
            toggle.set_enabled(False)
        self._run_hooks(BUSY_HOOKS)
        if not refresh:
            self.set_status("Scanning…")
        self.engine.scan(callback=self._scan_done)

    def _scan_done(self, future: Future[Any]) -> None:
        self._busy = False
        self._busy_close = BusyClose()
        refresh, self._scan_refresh = self._scan_refresh, False
        for toggle in self.toggles.values():
            toggle.set_enabled(self._mutations_enabled)
        self._run_hooks(BUSY_HOOKS)
        exc = future.exception()
        if exc is not None:
            self._scan_failed = True
            for panel in (self.optimize_list, self.apps_list):
                panel.show_error(str(exc))
            # Replaces any "Applying…" label a mode change left behind: with the state of
            # the last scan when nothing changed since, else with "status unknown".
            for category in self.toggles:
                self._refresh_toggle(category)
            self.set_status(f"Scan failed: {exc}", theme.CRITICAL)
            return
        report = future.result()
        self._last_scan = report
        self._unavailable_ids = {str(i["id"]) for i in report["items"] if i.get("unavailable")}
        self._scan_failed = False
        when = datetime.now().strftime("Scanned at %H:%M:%S")
        for panel in (self.optimize_list, self.apps_list):
            panel.show(report, self._mutations_enabled, when)
        for category in self.toggles:
            self._refresh_toggle(category)
        warnings = report.get("warnings") or []
        if warnings:
            for warning in warnings:
                log.warning("scan: %s", warning)
            # The status bar keeps the first line; the lists show the full text.
            self.set_status(
                f"Scan complete with {plural(len(warnings), 'warning')}: {warnings[0]}", theme.WARNING
            )
        elif not refresh:
            drifted = sum(len(i.get("drift") or []) for i in report["items"])
            if drifted:
                self.set_status(
                    SCAN_DRIFT_TEXT.format(
                        items=len(report["items"]),
                        ms=report["duration_ms"],
                        count=drifted,
                        holds=holds(drifted),
                    ),
                    theme.WARNING,
                )
            else:
                self.set_status(
                    f"Scan complete: {len(report['items'])} items checked in {report['duration_ms']} ms."
                )
        self._refresh_journal()

    def _refresh_journal(self) -> None:
        if self.engine is None:
            return

        def done(future: Future[Any]) -> None:
            if future.exception() is not None:
                return
            s = future.result()
            pending = sum(int(s.get(key, 0)) for key in JOURNAL_ACTIVE_COUNTS)
            self.status_journal.configure(
                text=f"Journal: {plural(pending, 'revertible change')}" if pending else "Journal: no changes"
            )
            self.nav.set_badge("History", str(pending) if pending else "", theme.INK_SECONDARY)

        self.engine.journal_summary(callback=done)

    # -- shared guards -------------------------------------------------------------

    def _guard(self, *, needs_admin: bool = True) -> bool:
        """Checks that a change may start: the engine is loaded, nothing else is running
        and, when `needs_admin`, the process is elevated (otherwise it offers a relaunch, or
        explains that this copy can't get administrator rights)."""
        if self.engine is None:
            return False
        if needs_admin and not self.elevated and not self.can_elevate:
            MessageDialog(
                self, title=ADMIN_UNAVAILABLE_TITLE, message=ADMIN_UNAVAILABLE_TEXT, confirm_text="OK"
            )
            return False
        if needs_admin and not self.elevated:
            MessageDialog(
                self,
                title="Administrator rights needed",
                message="Changing Windows settings, services, apps and files needs administrator rights. "
                "Restart Cairn as administrator to make changes.",
                confirm_text="Restart as administrator",
                cancel_text="Not now",
                on_confirm=self._relaunch_elevated,
            )
            return False
        if self._busy or self.engine.busy:
            self.set_status("Another operation is still running; wait for it to finish.", theme.WARNING)
            return False
        return True

    def _set_busy(self, busy: bool, close: str = CLOSE_OTHER, *, action: str = "", note: str = "") -> None:
        """Marks an operation as running (or finished) and disables the actions that would
        start another one.

        `close` says what closing the window means for the operation that starts: one of
        `CLOSE_READ` (reads and dry runs), `CLOSE_JOURNALED` (recorded changes and their
        undo), `CLOSE_IRREVERSIBLE` (logged-only actions) or `CLOSE_OTHER`. `action` names the
        operation in the close dialog ("The network stack reset") and `note` adds a sentence
        to it. Ending the busy state forgets them.
        """
        self._busy = busy
        self._busy_close = BusyClose(close, action, note) if busy else BusyClose()
        for panel in (self.optimize_list, self.apps_list):
            panel.set_scan_enabled(not busy and self.engine is not None)
        self.revert_button.configure(state="disabled" if busy or not self._mutations_enabled else "normal")
        for toggle in self.toggles.values():
            toggle.set_enabled(not busy and self._mutations_enabled)
        self._run_hooks(BUSY_HOOKS)

    # -- optimization modes and items ---------------------------------------------

    def _on_toggle(self, category: str, turn_on: bool) -> None:
        if turn_on:
            self._plan_mode_on(category, remaining=False)
        else:
            self._plan_mode_off(category)

    def _on_apply_remaining(self, category: str) -> None:
        self._plan_mode_on(category, remaining=True)

    def _plan_mode_on(self, category: str, *, remaining: bool) -> None:
        """Dry-runs the mode's recommended changes and asks before applying them."""
        if not self._guard():
            return
        assert self.engine is not None
        mode = CATEGORY_MODES[category]
        self._set_busy(True, CLOSE_READ)
        self.toggles[category].set_busy("Preparing…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            self._refresh_toggle(category)
            if future.exception() is not None:
                self._show_error(f"Could not prepare {mode}", future.exception())
                return
            planned_results = [r for r in future.result()["results"] if r["outcome"] == "planned"]
            details = [f"• {r['id']}: " + "; ".join(r["details"]) for r in planned_results]
            if not details:
                self.set_status(f"{mode}: every recommended change is already applied.")
                return
            message = (
                f"{plural(len(details), 'change')} will be applied. Each one is recorded first, "
                "so it can be undone individually or with Revert All Changes."
            )
            if self.engine is not None and self.engine.next_mutation_creates_restore_point:
                message += " A System Restore point is created first; this can take up to a minute."
            # What Undo cannot bring back is said before anything is applied, as for one tweak.
            limits = self._undo_limits(r["id"] for r in planned_results)
            if limits:
                message += "\n\n" + "\n".join(limits)
            MessageDialog(
                self,
                title=f"Apply the rest of {mode}?" if remaining else f"Turn on {mode}?",
                message=message,
                details=details,
                confirm_text="Apply remaining" if remaining else f"Turn on {mode}",
                cancel_text="Cancel",
                danger=bool(limits),
                on_confirm=lambda: self._run_mode(category, f"Applying {mode}…", apply=True),
            )

        self.engine.plan_category(category, callback=planned)

    def _plan_mode_off(self, category: str) -> None:
        """Dry-runs the revert of the mode's recommended items and asks before running it.

        Every recommended item of the category in the last scan is reverted, the same set
        turning the mode on applies, whether the mode or an Apply from the list changed it.
        Tweaks of the category that are not recommended stay in place. Items whose values
        were already set before Cairn ran have no journal records and are left as is.
        """
        if not self._guard():
            return
        assert self.engine is not None
        mode = CATEGORY_MODES[category]
        ids = self._recommended_ids(category)
        if not ids:
            self.set_status(f"{mode}: nothing recorded to undo for this mode.")
            return
        self._set_busy(True, CLOSE_READ)
        self.toggles[category].set_busy("Preparing…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            self._refresh_toggle(category)
            if future.exception() is not None:
                self._show_error(f"Could not prepare {mode}", future.exception())
                return
            plan = future.result()
            actions = plan.get("actions", [])
            if not actions:
                self.set_status(
                    f"{mode}: nothing recorded to undo. These settings were already in place before "
                    "Cairn changed anything."
                )
                return
            MessageDialog(
                self,
                title=f"Turn off {mode}?",
                message=f"{plural(len(actions), 'recorded change')} will be restored to the values "
                "they had before Cairn changed them." + self._planned_restart(plan),
                details=[f"• {a}" for a in actions],
                confirm_text=f"Turn off {mode}",
                cancel_text="Cancel",
                on_confirm=lambda: self._run_mode(category, f"Restoring {mode}…", apply=False, ids=ids),
            )

        self.engine.plan_revert(ids, callback=planned)

    def _recommended_ids(self, category: str) -> list[str]:
        """The items turning `category`'s mode off restores: its recommended items and the ones
        the engine marks for it (`mode_undo`: tweaks earlier versions recommended)."""
        items = self._last_scan["items"] if self._last_scan else []
        return [i["id"] for i in items if i["category"] == category and i.get("mode_undo", i["recommended"])]

    def _scan_item(self, item_id: str) -> dict[str, Any] | None:
        """The last scan's item `item_id`, when a scan describes the system."""
        items = self._last_scan["items"] if self._last_scan else []
        return next((i for i in items if i["id"] == item_id), None)

    def _undo_limits(self, ids: Iterable[str]) -> list[str]:
        """One line per item of `ids` whose changes Undo cannot fully bring back, as the last scan
        says or, for an item no scan describes (none ran, it failed, or a change dropped it),
        as the catalog says."""
        lines = []
        for item_id in ids:
            item = self._scan_item(item_id)
            limit = item.get("undo_limit") if item is not None else self._catalog_undo_limits.get(item_id)
            if limit:
                lines.append(UNDO_LIMIT_LINE.format(title=self._item_title(item_id), limit=limit))
        return lines

    def _item_title(self, item_id: str) -> str:
        """An item's title: from the catalog, else from the last scan, else its id."""
        if item_id in self._item_titles:
            return self._item_titles[item_id]
        item = self._scan_item(item_id)
        return str(item["title"]) if item is not None else item_id

    @staticmethod
    def _planned_restart(plan: dict[str, Any]) -> str:
        text = REVERT_RESTART_TEXT.get(plan.get("restart") or "none", "")
        return f" {text}" if text else ""

    def _category_status(self, category: str) -> dict[str, Any] | None:
        if not self._last_scan:
            return None
        return next((c for c in self._last_scan["categories"] if c["category"] == category), None)

    def _refresh_toggle(self, category: str) -> None:
        """Shows the mode's state from the last scan, or that it is unknown after a failed scan."""
        items = self._last_scan["items"] if self._last_scan else []
        self.toggles[category].set_status(
            self._category_status(category),
            recorded_recommended(items, category),
            unknown=self._scan_failed,
        )

    def _run_mode(self, category: str, busy_text: str, *, apply: bool, ids: list[str] | None = None) -> None:
        assert self.engine is not None
        self._set_busy(True, CLOSE_JOURNALED)
        self.toggles[category].set_busy(busy_text)
        self.set_status(busy_text)
        if apply:
            self.engine.apply_category(category, callback=self._apply_done)
        else:
            self.engine.revert(ids or [], callback=self._revert_done)

    def _on_item_action(self, item: dict[str, Any], kind: str) -> None:
        if not self._guard():
            return
        assert self.engine is not None
        apply = kind == "apply"
        appx = item["kind"] == "appx"
        verb = ("Remove" if appx else "Apply") if apply else ("Restore" if appx else "Undo")

        def run() -> None:
            assert self.engine is not None
            self._set_busy(True, CLOSE_JOURNALED)
            self.set_status(f"{verb}: {item['title']}…")
            if apply:
                self.engine.apply([item["id"]], callback=self._apply_done)
            else:
                self.engine.revert([item["id"]], callback=self._revert_done)

        def confirm(message: str, details: list[str], danger: bool = False) -> None:
            MessageDialog(
                self,
                title=f"{verb} {item['title']}?",
                message=message,
                details=details,
                confirm_text=verb,
                cancel_text="Cancel",
                danger=danger,
                on_confirm=run,
            )

        if apply:
            details = [FORMER_RESTORE_LINE.format(**former_values(r)) for r in item.get("former") or []]
            details += [f"• {a['detail']}" for a in item["actions"]]
            limit = item.get("undo_limit")
            if limit:
                # What Undo cannot bring back follows the description, in the danger style.
                confirm(f"{item['description']}\n\n⚠ {limit}", details, danger=True)
            else:
                confirm(item["description"], details)
            return

        # Undo restores journal records, so the dialog lists those, and an item whose value
        # was already in place (nothing recorded) gets no dialog at all.
        previous = (self.status_message.cget("text"), self.status_message.cget("text_color"))
        self._set_busy(True, CLOSE_READ)
        self.set_status(f"Checking what Cairn changed for {item['title']}…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            if future.exception() is not None:
                self._show_error("Could not read the journal", future.exception())
                return
            plan = future.result()
            actions = plan.get("actions", [])
            if not actions:
                self.set_status(f"{item['title']}: nothing recorded to undo. Cairn did not change this item.")
                return
            self.set_status(*previous)
            confirm(
                f"{plural(len(actions), 'recorded change')} will be restored to the values they had "
                "before Cairn changed them." + self._planned_restart(plan),
                [f"• {a}" for a in actions],
            )

        self.engine.plan_revert([item["id"]], callback=planned)

    def _on_bulk_apply(self, items: list[dict[str, Any]]) -> None:
        if not items or not self._guard():
            return

        def run() -> None:
            assert self.engine is not None
            self._set_busy(True, CLOSE_JOURNALED)
            self.set_status(f"Removing {plural(len(items), 'app')}…")
            self.engine.apply([i["id"] for i in items], callback=self._apply_done)

        MessageDialog(
            self,
            title=f"Remove {plural(len(items), 'app')}?",
            message="These preinstalled apps are removed for your account. Each one can be restored "
            "here or from History.",
            details=[f"• {i['title']}: {i['description']}" for i in items],
            confirm_text="Remove",
            cancel_text="Cancel",
            on_confirm=run,
        )

    def _on_revert_all(self) -> None:
        if not self._guard():
            return
        assert self.engine is not None
        self._set_busy(True, CLOSE_READ)
        self.set_status("Reading the journal…")

        def planned(future: Future[Any]) -> None:
            self._set_busy(False)
            if future.exception() is not None:
                self._show_error("Could not read the journal", future.exception())
                return
            plan = future.result()
            actions = plan["actions"]
            if not actions:
                MessageDialog(
                    self, title="Nothing to revert", message="The journal has no recorded changes to undo."
                )
                self.set_status("Nothing to revert.")
                return

            def run() -> None:
                assert self.engine is not None
                self._set_busy(True, CLOSE_JOURNALED)
                self.set_status("Reverting all changes…")
                self.engine.revert_all(callback=self._revert_done)

            MessageDialog(
                self,
                title="Revert all changes?",
                message=f"Every change Cairn recorded ({len(actions)} in total) is restored to its "
                "original state: registry values, services, scheduled tasks, startup apps, Store apps, "
                "DNS servers, the power plan and the scheduled maintenance task. Deleted cleanup files "
                "are not part of the journal and cannot come back, and repairs made by the maintenance "
                "tools cannot be undone." + self._planned_restart(plan),
                details=[f"• {a}" for a in actions],
                confirm_text="Revert All Changes",
                cancel_text="Cancel",
                danger=True,
                on_confirm=run,
            )

        self.engine.plan_revert_all(callback=planned)

    def _apply_done(self, future: Future[Any]) -> None:
        self._set_busy(False)
        exc = future.exception()
        if exc is not None:
            self._show_error("The changes could not be applied", exc)
            self._after_mutation()
            return
        report = future.result()
        kept, not_kept, skipped, failed = apply_outcomes(report["results"])
        applied = kept + [r for r in not_kept if r["outcome"] == "applied"]
        summary = apply_summary(len(kept), len(not_kept), len(skipped), len(failed))
        restart = RESTART_TEXT.get(report["restart"], "")
        rp = report.get("restore_point")
        notes = [f"Restore point #{rp['sequence']} created."] if rp else []
        notes += report.get("warnings", [])
        # A read-back cannot see which Edge policies Edge uses; Edge's own page can.
        if any(r["id"] in self._edge_ids for r in applied):
            notes.append(EDGE_POLICY_TEXT)
        if not_kept or failed or skipped or restart or notes:
            details = [
                NOT_KEPT_LINE.format(
                    title=self._item_title(r["id"]),
                    label=mismatch.get("label", ""),
                    found=mismatch.get("found", ""),
                    expected=mismatch.get("expected", ""),
                )
                for r in not_kept
                for mismatch in r["not_kept"]
            ]
            details += [f"• {r['id']} ({r['outcome']}): " + "; ".join(r["details"]) for r in failed + skipped]
            explorer = report["restart"] == "explorer" and bool(applied)
            if failed:
                title = "Some changes failed"
            elif not_kept:
                title = NOT_KEPT_TITLE.format(changes=plural(len(not_kept), "change"))
            else:
                title = "Changes applied"
            MessageDialog(
                self,
                title=title,
                message=" ".join(
                    [summary + "."]
                    + ([NOT_KEPT_TEXT] if not_kept else [])
                    + notes
                    + ([restart] if restart else [])
                ),
                details=details,
                confirm_text="Restart Explorer now" if explorer else "OK",
                cancel_text="Later" if explorer else None,
                on_confirm=self._restart_explorer if explorer else None,
            )
        if not_kept:
            self.set_status(f"⚠ Done: {summary}. {restart}".strip(), theme.WARNING)
        else:
            self.set_status(
                f"Done: {summary}. {restart}".strip(), theme.GOOD if not failed else theme.WARNING
            )
        self._schedule_settled_check([r["id"] for r in kept + not_kept])
        self._after_mutation()

    # -- the settled check ----------------------------------------------------------

    def _schedule_settled_check(self, ids: list[str]) -> None:
        """Reads the tweaks `ids` an apply changed again `SETTLED_CHECK_MS` later, replacing a
        check that is still pending. Store apps are not read back, and an engine without the
        check gets none. The check reads the tweaks' recorded values only, so whether the
        apply restored values an earlier version of Cairn set, whose records the restore ended,
        is noted (`_restores_former`)."""
        self._cancel_settled_check()
        tweaks = [i for i in ids if not i.startswith("appx.")]
        if not tweaks or self.engine is None or not self.engine.supports("verify"):
            return
        self._settled_ids = tweaks
        self._settled_former = any(self._restores_former(i) for i in tweaks)
        self._settled_tries = 0
        self._settled_status = self._status_serial
        self._settled_job = self.after(SETTLED_CHECK_MS, self._settled_check)

    def _restores_former(self, item_id: str) -> bool:
        """Whether applying `item_id` restored values an earlier version of Cairn set: the last
        scan lists them for it or, when no scan describes it, the catalog names such values."""
        item = self._scan_item(item_id)
        if item is not None:
            return bool(item.get("former"))
        return item_id in self._former_ids

    def _cancel_settled_check(self) -> None:
        """Drops a pending settled check; a read already queued is ignored when it returns."""
        self._settled_serial += 1
        self._settled_ids = []
        job, self._settled_job = self._settled_job, None
        if job is not None:
            try:
                self.after_cancel(job)
            except tk.TclError:
                pass

    def _settled_check(self) -> None:
        """Reads the applied tweaks again once nothing else runs; while something does, retries
        every `SETTLED_RETRY_MS`, at most `SETTLED_RETRIES` times, then gives up."""
        self._settled_job = None
        if not self._running or self.engine is None or not self._settled_ids:
            return
        if self._busy or self.engine.busy:
            self._settled_tries += 1
            if self._settled_tries > SETTLED_RETRIES:
                log.info("the settled check was given up: another operation kept running")
                self._settled_ids = []
                return
            self._settled_job = self.after(SETTLED_RETRY_MS, self._settled_check)
            return
        ids, self._settled_ids = self._settled_ids, []
        serial = self._settled_serial
        self.engine.verify(ids, callback=lambda future: self._settled_checked(future, serial))

    def _settled_checked(self, future: Future[Any], serial: int) -> None:
        if future.exception() is not None:
            log.warning("the settled check failed: %s", future.exception())
            return
        if serial != self._settled_serial:
            # A later change replaced or cancelled this check.
            return
        report = future.result()
        entries = report.get("entries") or []
        unread = report.get("warnings") or []
        for warning in unread:
            log.warning("the settled check: %s", warning)
        # The tick needs every value the apply wrote read back: the check read some, left none
        # unread, and the apply restored no value an earlier version set, which it cannot read.
        read_all = (
            bool(report.get("checked"))
            and not unread
            and not report.get("unchecked")
            and not self._settled_former
        )
        if entries:
            titles: list[str] = []
            for entry in entries:
                title = self._item_title(str(entry.get("item") or ""))
                if title not in titles:
                    titles.append(title)
            self.set_status(
                SETTLED_CHANGED_TEXT.format(
                    settings=plural(len(entries), "setting"), titles=", ".join(titles)
                ),
                theme.WARNING,
            )
            # The rescan shows what changed back on the rows and keeps this message; while
            # another operation runs, the rescan after it does the same.
            self.start_scan(refresh=True)
            if self.section_visible("History"):
                self.load_history()
        elif read_all and self._status_serial == self._settled_status:
            # Only while the status bar still shows the apply's result; a value that was not
            # read back is not said to be still set.
            self.set_status(SETTLED_KEPT_TEXT, theme.GOOD)

    def _revert_done(self, future: Future[Any]) -> None:
        self._set_busy(False)
        exc = future.exception()
        if exc is not None:
            self._show_error("The changes could not be reverted", exc)
            self._after_mutation()
            return
        report = future.result()
        # The values an apply just set are being undone, so its settled check is dropped.
        self._cancel_settled_check()
        restored = sum(report.get(key, 0) for key in REVERT_COUNTS)
        failures = report.get("failures", [])
        store = report.get("appx_store_required", [])
        not_restored = report.get("not_restored") or []
        warnings = report.get("warnings") or []
        if not restored and not failures and not store:
            self.set_status("Nothing was recorded to undo, so nothing changed.", theme.INK_SECONDARY)
            self._after_mutation()
            return
        need = (report.get("restart") or "none") if restored else "none"
        restart = REVERT_RESTART_TEXT.get(need, "")
        explorer = need == "explorer"
        summary = f"{plural(restored, 'change')} restored"
        if not_restored:
            summary += f", {len(not_restored)} changed again"
        if failures:
            summary += f", {len(failures)} failed"
        if failures or store or restart or not_restored or warnings:
            links = [
                (
                    f"Open Store page: {s['package_family'].split('_')[0]}",
                    lambda link=s["store_link"]: system.open_uri(link),
                )
                for s in store
            ]
            message = summary + "."
            if not_restored:
                message += " " + not_restored_text(len(not_restored))
            if store:
                message += (
                    " Some removed apps no longer have their files on this PC; "
                    "reinstall them from the Microsoft Store."
                )
            if restart:
                message += " " + restart
            if failures or store:
                title = "Revert finished with issues"
            elif not_restored:
                title = NOT_RESTORED_TITLE
            else:
                title = "Changes restored"
            details = [f"• {f['target']}: {f['error']}" for f in failures]
            details += [
                NOT_RESTORED_LINE.format(
                    label=m.get("label", ""), found=m.get("found", ""), expected=m.get("expected", "")
                )
                for m in not_restored
            ]
            details += [f"⚠ {w}" for w in warnings]
            MessageDialog(
                self,
                title=title,
                message=message,
                details=details,
                links=links,
                confirm_text="Restart Explorer now" if explorer else "OK",
                cancel_text="Later" if explorer else None,
                on_confirm=self._restart_explorer if explorer else None,
            )
        if not_restored:
            self.set_status(f"⚠ Done: {summary}. {restart}".strip(), theme.WARNING)
        else:
            self.set_status(
                f"Done: {summary}. {restart}".strip(), theme.GOOD if not failures else theme.WARNING
            )
        self._after_mutation()

    def _after_mutation(self) -> None:
        """Refreshes every view a change can affect.

        The last scan no longer describes the system, so it is dropped; if the rescan
        fails, the modes show their status as unknown instead of the state before the change.
        The rescan keeps the change's status message unless it fails or warns.
        """
        self._last_scan = None
        self.start_scan(refresh=True)
        if self.startup_panel.loaded:
            self.load_startup()
        if self.section_visible("History"):
            self.load_history()
        self._run_hooks(AFTER_MUTATION_HOOKS)

    def _restart_explorer(self) -> None:
        if self.engine is None:
            return
        self.set_status("Restarting File Explorer…")

        def done(future: Future[Any]) -> None:
            if future.exception() is not None:
                self._show_error("File Explorer could not be restarted", future.exception())
            else:
                self.set_status("File Explorer restarted.", theme.GOOD)

        self.engine.restart_explorer(callback=done)

    # -- cleanup -------------------------------------------------------------------

    def start_cleanup_scan(self) -> None:
        if self.engine is None:
            return
        self._cleanup_scanning = True
        self.cleanup_panel.set_scanning()

        def done(future: Future[Any]) -> None:
            self._cleanup_scanning = False
            if future.exception() is not None:
                self.cleanup_panel.show_error(str(future.exception()))
                return
            self.cleanup_panel.show(future.result(), self.elevated, self._mutations_enabled)

        self.engine.cleanup_scan(callback=done)

    def _on_clean(self, targets: list[dict[str, Any]]) -> None:
        blocked = self._maintenance_blocks_cleanup()
        if isinstance(blocked, str):
            self.set_status(blocked, theme.WARNING)
            return
        if not self._guard():
            return
        total = sum(t["bytes"] for t in targets)

        def run() -> None:
            assert self.engine is not None
            self._set_busy(True, CLOSE_IRREVERSIBLE, action="The cleanup")
            self.cleanup_panel.set_busy("Cleaning…")
            self.set_status(f"Cleaning {fmt_size(total)}…")
            self.engine.cleanup_run([t["id"] for t in targets], callback=self._clean_done)

        MessageDialog(
            self,
            title=f"Delete {fmt_size(total)}?",
            message="Files in the locations listed below are deleted permanently; this cannot be "
            "undone. Files in use are skipped. Running apps that own a cache may need to be closed "
            "for it to be cleaned.",
            details=self._cleanup_details(targets),
            confirm_text="Delete permanently",
            cancel_text="Cancel",
            danger=True,
            on_confirm=run,
        )

    @staticmethod
    def _cleanup_details(targets: list[dict[str, Any]]) -> list[str]:
        """One line per target with its size and age rule, followed by the folders it empties."""
        lines: list[str] = []
        for t in targets:
            line = f"• {t['title']}: {fmt_size(t['bytes'])}"
            kept = t.get("recent_files_kept")
            if kept is True:
                line += "  (files changed in the last 24 hours are kept)"
            elif kept is False:
                line += "  (all files, including recent ones)"
            lines.append(line)
            lines += [f"      {path}" for path in t.get("paths") or []]
        return lines

    def _clean_done(self, future: Future[Any]) -> None:
        self._set_busy(False)
        if future.exception() is not None:
            self._show_error("Cleanup failed", future.exception())
            self.start_cleanup_scan()
            return
        report = future.result()
        skipped = sum(r["skipped_files"] for r in report["results"])
        notes = [f"• {r['id']}: {r['skipped_reason']}" for r in report["results"] if r.get("skipped_reason")]
        notes += [f"• {r['id']}: {e}" for r in report["results"] for e in r.get("errors", [])]
        text = f"Freed {fmt_size(report['freed_bytes'])}."
        if skipped:
            text += f" {plural(skipped, 'file')} in use or too recent were left in place."
        if notes:
            MessageDialog(self, title="Cleanup finished", message=text, details=notes)
        self.set_status(text, theme.GOOD)
        self.start_cleanup_scan()

    # -- startup -------------------------------------------------------------------

    def load_startup(self) -> None:
        if self.engine is None:
            return
        self.startup_panel.set_loading()

        def done(future: Future[Any]) -> None:
            if future.exception() is not None:
                self.startup_panel.show_error(str(future.exception()))
                return
            entries = future.result()
            self._startup_names = packaged_task_names(entries)
            self.startup_panel.show(entries, self.engine is not None, self.elevated)

        self.engine.startup_list(callback=done)

    def _on_startup_toggle(
        self,
        entry: dict[str, Any],
        enabled: bool,
        *,
        on_done: Callable[[bool], None] | None = None,
    ) -> None:
        """Enables or disables a startup entry (journaled). `on_done(success)` runs once the
        startup list and the journal state are reloaded, or at once when the change is refused."""
        # Per-user entries are HKCU writes the engine allows without elevation.
        if not self._guard(needs_admin=bool(entry.get("requires_admin", True))):
            self.load_startup()
            if on_done is not None:
                on_done(False)
            return
        assert self.engine is not None
        verb = "Enabling" if enabled else "Disabling"
        self._set_busy(True, CLOSE_JOURNALED)
        self.set_status(f"{verb} {entry['name']} at startup…")

        def done(future: Future[Any]) -> None:
            self._set_busy(False)
            if future.exception() is not None:
                self._show_error(f"Could not change {entry['name']}", future.exception())
            else:
                state = "enabled" if enabled else "disabled"
                self.set_status(f"{entry['name']} {state} at startup. Undo it from History.", theme.GOOD)
            self.load_startup()
            self._refresh_journal()
            if on_done is not None:
                on_done(future.exception() is None)

        self.engine.startup_set_enabled(entry["id"], enabled, callback=done)

    # -- history -------------------------------------------------------------------

    def load_history(self) -> None:
        """Reads the journal export and, from an engine that has the check, which recorded
        changes no longer hold, then shows both. A newer load replaces an older one."""
        if self.engine is None:
            return
        self.history_panel.set_loading()
        # Packaged-app startup records are titled with the task's display name, which only
        # the startup list has. Engine calls run and call back in order, so the list is
        # read before the journal export is shown, and the export before the drift report.
        if not self.startup_panel.loaded:
            self.load_startup()
        self._history_serial += 1
        serial = self._history_serial
        with_drift = self.engine.supports("drift")
        loaded: dict[str, Any] = {}

        def show(drift: dict[str, Any] | None = None, drift_error: str | None = None) -> None:
            self.history_panel.show(
                loaded["export"],
                self._titles,
                self.engine is not None,
                self.elevated,
                self._startup_names,
                drift=drift,
                drift_error=drift_error,
                former_targets=self._former_targets,
                unavailable_ids=self._skipped_here(),
            )

        def exported(future: Future[Any]) -> None:
            if serial != self._history_serial:
                return
            if future.exception() is not None:
                self.history_panel.show_error(str(future.exception()))
                return
            loaded["export"] = future.result()
            if not with_drift:
                show()

        def drifted(future: Future[Any]) -> None:
            if serial != self._history_serial or "export" not in loaded:
                return
            exc = future.exception()
            if exc is not None:
                log.warning("the drift check failed: %s", exc)
                show(drift_error=str(exc))
                return
            report = future.result()
            # Records it could not read are counted in the report; why goes to the log.
            for warning in report.get("warnings") or []:
                log.warning("the drift check: %s", warning)
            show(drift=report)

        self.engine.journal_export(callback=exported)
        if with_drift:
            self.engine.drift(callback=drifted)

    def _skipped_here(self) -> set[str]:
        """The tweaks an apply skips on this PC: retired ones and those the last scan found this
        PC cannot use."""
        return self._retired_ids | self._unavailable_ids

    def _on_history_apply_again(self, change: ChangeGroup) -> None:
        """Applies the tweaks of a change that no longer holds again, through the usual apply,
        its read-back and its settled check; tweaks an apply skips on this PC are left out.
        When Undo cannot fully bring back what one of them changes, the dialog says so first,
        in the danger style."""
        skipped = self._skipped_here()
        ids = [i for i in change.item_ids if i not in skipped]
        if not ids or not self._guard():
            return
        # What Undo cannot bring back is said before anything is applied, as for one tweak.
        limits = self._undo_limits(ids)
        message = APPLY_AGAIN_TEXT + ("\n\n" + "\n".join(limits) if limits else "")

        def run() -> None:
            assert self.engine is not None
            self._set_busy(True, CLOSE_JOURNALED)
            self.set_status(f"Applying {change.title} again…")
            self.engine.apply(ids, callback=self._apply_done)

        MessageDialog(
            self,
            title=APPLY_AGAIN_TITLE.format(title=change.title),
            message=message,
            details=drift_lines(change),
            confirm_text="Apply again",
            cancel_text="Cancel",
            danger=bool(limits),
            on_confirm=run,
        )

    def _on_history_keep(self, change: ChangeGroup, kind: str) -> None:
        """Keep as is: stops tracking the records of `change` that no longer hold, or with
        `kind` KEEP_FORMER its values an earlier version of Cairn set. Nothing on this PC is
        written, so no administrator rights are needed. When an original would be lost, the
        dialog says which and uses the danger style. The confirmation and the result say
        whether Cairn still tracks other records of the change."""
        if not self._guard(needs_admin=False):
            return
        selection = change.former_filter if kind == KEEP_FORMER else change.drift_filter
        partial = not selects_every_record(change.filter, selection)
        loses = KEEP_LOSES_SOME_TEXT if partial else KEEP_LOSES_TEXT
        if kind == KEEP_FORMER:
            message, danger = loses, True
            details = [KEEP_FORMER_LINE.format(target=t, original=o) for t, o in change.former]
        else:
            danger = not all(m.get("back_to_original") for m in change.drift)
            message = loses if danger else (KEEP_BACK_SOME_TEXT if partial else KEEP_BACK_TEXT)
            details = []
            if danger:
                details = [
                    KEEP_LOSES_LINE.format(
                        label=m.get("label", ""),
                        original=UNKNOWN_ORIGINAL if m.get("original") is None else m["original"],
                        found=m.get("found", ""),
                    )
                    for m in change.drift
                ]
        target_filter = {
            key: (list(value) if isinstance(value, list) else value) for key, value in selection.items()
        }

        def done(future: Future[Any]) -> None:
            self._set_busy(False)
            exc = future.exception()
            if exc is not None:
                self._show_error("The change could not be kept as it is", exc)
            else:
                report = future.result()
                refused = report.get("refused") or []
                if refused:
                    lines = []
                    for r in refused:
                        target, error = str(r.get("target", "")), str(r.get("error", ""))
                        lines.append(f"• {error}" if error.startswith(target) else f"• {target}: {error}")
                    MessageDialog(self, title=KEEP_REFUSED_TITLE, message=KEEP_REFUSED_TEXT, details=lines)
                    self.set_status(KEEP_PARTLY_TEXT.format(title=change.title), theme.WARNING)
                else:
                    # One record per detail line; those the selection left out are still tracked.
                    rest = len(report.get("forgotten") or []) < len(change.details)
                    done_text = KEEP_DONE_SOME_TEXT if rest else KEEP_DONE_TEXT
                    self.set_status(done_text.format(title=change.title), theme.GOOD)
            self._after_mutation()

        def run() -> None:
            assert self.engine is not None
            self._set_busy(True, CLOSE_OTHER)
            self.set_status(f"Keeping {change.title} as it is…")
            self.engine.forget_targets(target_filter, callback=done)

        MessageDialog(
            self,
            title=KEEP_TITLE.format(title=change.title),
            message=message,
            details=details,
            confirm_text="Keep as is",
            cancel_text="Cancel",
            danger=danger,
            on_confirm=run,
        )

    def _on_history_undo(self, change: ChangeGroup) -> None:
        if not self._guard(needs_admin=change.needs_admin):
            return

        def run() -> None:
            assert self.engine is not None
            self._set_busy(True, CLOSE_JOURNALED)
            self.set_status(f"Undoing {change.title}…")
            self.engine.revert_targets(change.filter, callback=self._revert_done)

        MessageDialog(
            self,
            title=f"Undo {change.title}?",
            message="The values recorded before this change are restored.",
            details=[f"• {d}" for d in change.details],
            confirm_text="Undo",
            cancel_text="Cancel",
            on_confirm=run,
        )

    # -- errors, elevation and shutdown -------------------------------------------

    def _show_error(self, title: str, exc: BaseException | None) -> None:
        self.set_status(f"{title}: {exc}", theme.CRITICAL)
        MessageDialog(self, title=title, message=str(exc))

    def _relaunch_elevated(self) -> None:
        # The elevated copy waits for this process to end before it takes the instance lock.
        if system.relaunch_as_admin(os.getpid()):
            self._request_close(force=True)
        else:
            self.set_status("Restart as administrator was cancelled.", theme.WARNING)

    def _running_close(self) -> BusyClose:
        """What closing the window means for the engine call in progress.

        Every flow that changes something holds `_busy` and says what it is. An engine call
        outside one reads information (a list, the cleanup scan, the system snapshot) or
        starts a program (File Explorer, a log, a Windows tool) and changes no setting.
        """
        return self._busy_close if self._busy else BusyClose(CLOSE_READ)

    def _busy_close_confirmed(self, shown: BusyClose) -> None:
        """Continues closing after the busy dialog that described `shown` was confirmed. An
        operation that started while that dialog was open and is not a read is asked about
        in its own dialog first; job lanes already confirmed are not asked again."""
        assert self.engine is not None
        running = self._running_close() if self._busy or self.engine.busy else None
        unchanged = running is None or running.kind == CLOSE_READ or running == shown
        self._request_close(busy_confirmed=unchanged, confirmed=frozenset(self._close_confirmed))

    def _continue_close(self, lane: str, *, busy_unchanged: bool) -> None:
        """Continues closing after the close dialog of job lane `lane` was confirmed.
        `busy_unchanged` is False when an engine call other than the one already accepted
        started while that dialog was open, so the busy dialog asks about it."""
        self._close_confirmed.add(lane)
        self._request_close(busy_confirmed=busy_unchanged, confirmed=frozenset(self._close_confirmed))

    def _request_close(
        self,
        force: bool = False,
        *,
        busy_confirmed: bool = False,
        confirmed: frozenset[str] = frozenset(),
    ) -> None:
        """Closes the window, first asking about a running engine call, then about each job
        lane's running job, in lane order.

        Confirming the busy dialog continues with the lanes (`busy_confirmed`), or asks again
        when an operation other than a read started while it was open. A lane's dialog
        confirms through `_continue_close`, which adds the lane to `confirmed`, so it is not
        asked again. Only `force` skips every dialog. The busy dialog says what closing means
        for the call that runs (see `_set_busy`): a read can be left at no cost, a change or
        an irreversible action finishes unseen.

        Once closing proceeds, the single-instance lock stops accepting activations, so a
        second start waits for this process instead of signalling a window that no longer
        draws; then telemetry stops, each lane prepares for shutdown and the engine shuts down.
        """
        if not busy_confirmed and not confirmed:
            self._close_confirmed = set()
        if not force and self.engine is not None:
            if not busy_confirmed and (self._busy or self.engine.busy):
                close = self._running_close()
                read = close.kind == CLOSE_READ
                MessageDialog(
                    self,
                    title=BUSY_CLOSE_TITLE,
                    message=busy_close_text(close),
                    confirm_text="Close" if read else "Close anyway",
                    cancel_text="Keep open" if read else "Keep running",
                    danger=not read,
                    on_confirm=lambda: self._busy_close_confirmed(close),
                )
                return
            for lane in self._job_lanes:
                if lane.name not in confirmed and not getattr(self, lane.allow_close)():
                    return
        self._running = False
        self._cancel_settled_check()
        instance = getattr(self, "instance", None)
        if instance is not None:
            try:
                instance.stop_activation()
            except Exception:  # noqa: BLE001 - closing continues without it
                log.exception("stopping window activation failed")
        telemetry, self._telemetry = self._telemetry, None
        if telemetry is not None:
            telemetry.stop()
        if self.engine is not None:
            for lane in self._job_lanes:
                try:
                    getattr(self, lane.before_shutdown)()
                except Exception:  # noqa: BLE001 - closing continues whatever a lane reports
                    log.exception("%s failed", lane.before_shutdown)
            self.engine.shutdown()
        if self._frame_job is not None:
            self.after_cancel(self._frame_job)
            self._frame_job = None
        self.destroy()

    def report_callback_exception(self, exc: type[BaseException], val: BaseException, tb: Any) -> None:
        self.errors.append("".join(traceback.format_exception(exc, val, tb)))
        log.error("Tk callback failed", exc_info=(exc, val, tb))
