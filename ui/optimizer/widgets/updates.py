"""Updates section: app updates and installs through winget, and the Windows Update settings.

The section shows one of three views, picked with a segmented switch: App updates (the
apps winget can update), Install apps (a curated, editable list) and Windows Update (pause,
active hours and three more settings). A job strip below them follows the winget job that
runs, with its progress, its output and its log.

The pure helpers at the top shape the engine's results for display and need no window.
Widgets never create dialogs; the feature does, with the window as their master.
"""

from __future__ import annotations

import logging
import math
import tkinter as tk
from collections.abc import Callable, Iterable, Mapping, Sequence
from datetime import datetime
from typing import Any

import customtkinter as ctk

from .. import APP_NAME, theme
from .controls import PANEL_WRAP, ROW_WRAP, WRAP_MARGIN, WRAP_STEP, fitted_wrap
from .monitor import set_text
from .tools import FLUSH_LINES, MAX_OUTPUT_LINES, OutputBuffer, fmt_duration

log = logging.getLogger(__name__)

# View key -> the segment that shows it, in display order.
VIEWS = {"apps": "App updates", "install": "Install apps", "windows": "Windows Update"}

WINGET_MISSING_TEXT = (
    "winget (App Installer) isn't set up for this account. Windows installs it shortly after your first "
    "sign-in; you can also get App Installer from the Microsoft Store. Then choose Check again."
)
WINGET_OUTDATED_TEXT = (
    "winget {version} is too old; " + APP_NAME + " needs {minimum} or newer. Update App Installer from "
    "the Microsoft Store, then choose Check again."
)
OTHER_USER_TEXT = (
    APP_NAME + " is running as a different account than the one signed in, so app updates and installs "
    "would go to that account. They are turned off; start " + APP_NAME + " from your own account to use them."
)
USER_UNKNOWN_TEXT = (
    APP_NAME
    + " can't tell which account is signed in, so app updates and installs are turned off to be safe."
)
IRREVERSIBLE_NOTE = (
    "Updating or installing apps can't be undone by "
    + APP_NAME
    + ". Each one is recorded in History › Activity."
)
AGREEMENTS_NOTE = (
    "winget downloads apps from their publishers. " + APP_NAME + " accepts winget's source agreements and "
    "each app's license terms for you."
)
PINNED_HELP_TEXT = "winget leaves out pinned apps and apps whose version it can't read."
HOME_DEFER_TEXT = (
    "Windows 11 Home ignores this setting. On Home, a new Windows version installs only when you choose it "
    "in Settings, until your current version nears the end of its support."
)
HOME_DRIVERS_CAVEAT = "Microsoft documents this setting for Pro and higher; Windows 11 Home may ignore it."
POLICY_CAVEAT = (
    "This is a policy, so Windows Settings will say some settings are managed by your organization."
)
NOT_CHECKED_TEXT = "Not checked yet."
CHECKING_TEXT = "Checking for app updates…"
INSTALL_INTRO = "Pick apps to install. Apps already on this PC are skipped."
INSTALL_CHECKING_TEXT = "Checking which apps are installed…"
INVALID_ID_TEXT = "That isn't a winget package id: use letters, digits and dots, like Publisher.App."
DUPLICATE_TEXT = "{name} is already on the list."
NAME_MISSING_TEXT = "Give the app a name to show here."
NAME_TOO_LONG_TEXT = "Use a name of at most 80 characters."
NO_UPDATES_TEXT = "All apps are up to date."
RETRY_LEFT_OUT_TEXT = "Update all leaves it out; tick it to try again."
WU_LOADING_TEXT = "Reading the Windows Update settings…"
WU_ERROR_TEXT = "Could not read the Windows Update settings: {message}"
WU_FOOTER_TEXT = "Changes here are recorded and can be undone here or in History."
RESTART_PENDING_TEXT = "⚠ Windows is waiting for a restart to finish installing updates."
SERVICE_DISABLED_TEXT = (
    "⚠ The Windows Update service is disabled, so Windows doesn't check for updates and these settings "
    "have no effect."
)
APP_INSTALLER_STORE_URI = "ms-windows-store://pdp/?ProductId=9NBLGGH4NNS1"
STORE_UPDATES_URI = "ms-windows-store://downloadsandupdates"
WU_SETTINGS_URI = "ms-settings:windowsupdate"
WU_OPTIONS_URI = "ms-settings:windowsupdate-options"
# winget's own package; the Microsoft Store updates it.
APP_INSTALLER_ID = "Microsoft.AppInstaller"
MAX_NAME_CHARS = 80
# Above this many rows the list uses plain Tk labels, which draw much faster.
LIGHT_ROWS_ABOVE = 60

# Category id -> heading, in display order.
CATEGORY_TITLES = {
    "browsers": "Browsers",
    "chat": "Chat and calls",
    "gaming": "Gaming",
    "media": "Media",
    "productivity": "Productivity",
    "utilities": "Utilities",
    "developer": "Developer tools",
}

PAUSE_CHOICES = ("1 week", "2 weeks", "3 weeks", "4 weeks", "5 weeks")
DEFER_CHOICES = ("Don't delay", "30 days", "60 days", "90 days", "180 days", "365 days")
HOUR_CHOICES = tuple(f"{h:02}:00" for h in range(24))
MAX_ACTIVE_SPAN = 18

SETTING_DESCRIPTIONS = {
    "pause": "Stops Windows from installing updates until the date you choose, for up to 5 weeks. "
    "Security updates wait too.",
    "active_hours": "Windows won't restart your PC for updates during these hours. "
    "They can span up to 18 hours.",
    "exclude_drivers": "Windows Update stops installing device drivers with its monthly updates. Get drivers "
    "from your PC or device maker instead.",
    "defer_feature": "Waits the chosen number of days after a new Windows version is released "
    "before offering it.",
    "restart_notify": "Shows a notification when Windows needs to restart to finish updating.",
}
SETTING_TITLES = {
    "pause": "Pause updates",
    "active_hours": "Active hours",
    "exclude_drivers": "Skip drivers in Windows Update",
    "defer_feature": "Delay feature updates",
    "restart_notify": "Notify me before restarting",
}

# Item state -> (text, theme colour name); updates and installs word success differently.
_ITEM_STATUS: dict[str, tuple[str, str]] = {
    "queued": ("○ Waiting", "INK_MUTED"),
    "already_current": ("✓ Already up to date", "GOOD"),
    "already_installed": ("✓ Already installed", "GOOD"),
    "restart_required": ("⚠ Restart Windows to finish", "WARNING"),
    "timed_out": ("⚠ Still running after 60 min", "WARNING"),
    "left_running": ("◐ Continues after " + APP_NAME + " closed", "INK_SECONDARY"),
    "not_started": ("– Not started", "INK_MUTED"),
    "skipped": ("– Skipped", "INK_MUTED"),
}


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def _mono(size: int) -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.MONO_FAMILY, size=size)


# -- pure helpers --------------------------------------------------------------------


def plural(count: int, noun: str) -> str:
    return f"{count} {noun}{'' if count == 1 else 's'}"


def apps_text(count: int) -> str:
    """ "1 app" or "3 apps"."""
    return plural(count, "app")


def version_change(installed: str | None, available: str | None) -> str:
    """ "1.2.0 → 1.3.0"; a missing side is left out."""
    left, right = str(installed or "").strip(), str(available or "").strip()
    if left and right:
        return f"{left} → {right}"
    return right or left


def item_status(
    state: str | None, message: str | None, kind: str, progress: str | None = None
) -> tuple[str, str]:
    """Status text of one app of a batch and the name of its theme colour. `kind` is "upgrade"
    or "install"; `progress` is added to a running item ("◐ Updating…  ·  45%")."""
    installing = kind == "install"
    if state == "running":
        text = "◐ Installing…" if installing else "◐ Updating…"
        if progress:
            text += f"  ·  {progress}"
        return text, "ACCENT"
    if state == "succeeded":
        return ("✓ Installed" if installing else "✓ Updated"), "GOOD"
    if state == "failed":
        return f"⚠ Failed: {message}" if message else "⚠ Failed", "SERIOUS"
    if state in _ITEM_STATUS:
        return _ITEM_STATUS[state]
    return f"– {state or 'Unknown'}", "INK_MUTED"


def _local(stamp: str | None) -> datetime | None:
    if not stamp:
        return None
    try:
        return datetime.fromisoformat(str(stamp).replace("Z", "+00:00")).astimezone()
    except ValueError:
        return None


def clock_text(stamp: str | None, now: datetime | None = None) -> str:
    """The local time of an RFC 3339 `stamp`: "14:05" on the day of `now`, else "2026-09-24 14:05"."""
    local = _local(stamp)
    if local is None:
        return str(stamp or "")
    today = (now or datetime.now().astimezone()).astimezone().date()
    return local.strftime("%H:%M") if local.date() == today else local.strftime("%Y-%m-%d %H:%M")


def date_text(stamp: str | None, now: datetime | None = None) -> str:
    """ "Mon 12 Oct, 10:00" in local time; the year is added when it is not the year of `now`."""
    local = _local(stamp)
    if local is None:
        return str(stamp or "")
    year = (now or datetime.now().astimezone()).astimezone().year
    text = local.strftime("%a %d %b, %H:%M")
    return text if local.year == year else local.strftime("%a %d %b %Y, %H:%M")


def checking_text(elapsed_ms: float | None = None) -> str:
    """ "Checking for app updates…  ·  12 s"."""
    if elapsed_ms is None:
        return CHECKING_TEXT
    return f"{CHECKING_TEXT}  ·  {fmt_duration(float(elapsed_ms))}"


def scan_summary(result: Mapping[str, Any] | None, now: datetime | None = None) -> str:
    """The App updates summary line of a finished check (or "Not checked yet.")."""
    if not result:
        return NOT_CHECKED_TEXT
    error = result.get("error")
    if error:
        return f"⚠ {error.get('message') or 'The check failed.'}"
    parts = []
    count = len(result.get("upgrades") or [])
    parts.append(f"{plural(count, 'update')} available" if count else "All apps are up to date")
    if result.get("checked_at"):
        parts.append(f"checked {clock_text(str(result['checked_at']), now)}")
    if result.get("winget_version"):
        parts.append(f"winget {result['winget_version']}")
    return "  ·  ".join(parts)


def scan_warnings(result: Mapping[str, Any] | None) -> list[str]:
    """The check's warnings plus the lines about rows and apps it could not read."""
    if not result or result.get("error"):
        return []
    warnings = [str(w) for w in result.get("warnings") or []]
    unparsed = int(result.get("unparsed_rows") or 0)
    if unparsed:
        lines = "line" if unparsed == 1 else "lines"
        warnings.append(f"{unparsed} {lines} of winget's list could not be read; those apps are not shown.")
    if result.get("inventory_complete") is False:
        warnings.append("Installed apps could not be read, so some apps may be missing from the list.")
    return warnings


def selectable(row: Mapping[str, Any]) -> bool:
    return bool(row.get("selectable", True)) and str(row.get("id") or "").lower() != APP_INSTALLER_ID.lower()


def retry_futile(last: Mapping[str, Any] | None) -> bool:
    """Whether an app's last attempt of this session failed in a way trying again can't change
    (the engine's `retry` is False; a result without it counts as one a retry can change)."""
    return last is not None and last.get("state") == "failed" and last.get("retry", True) is False


def default_selection(
    upgrades: Iterable[Mapping[str, Any]], last_results: Mapping[str, Mapping[str, Any]] | None = None
) -> set[str]:
    """Ids selected when the list appears, which are also the apps Update all takes: selectable
    rows that winget includes in Update all, that are not Microsoft Store apps and whose last
    attempt of this session (`last_results`, by lowercased id) did not fail in a way a retry
    can't change."""
    last_results = last_results or {}
    return {
        str(row["id"])
        for row in upgrades
        if selectable(row)
        and not row.get("explicit_only")
        and str(row.get("source")) != "msstore"
        and not retry_futile(last_results.get(str(row["id"]).lower()))
    }


def row_note(row: Mapping[str, Any], last: Mapping[str, Any] | None = None) -> str:
    """The note under an app's name: why it can't be picked, how winget treats it, and the
    last failed attempt of this session, with whether Update all leaves the app out for it."""
    parts = []
    if row.get("note"):
        parts.append(str(row["note"]))
    elif str(row.get("id") or "").lower() == APP_INSTALLER_ID.lower():
        parts.append("Updated by the Microsoft Store.")
    elif row.get("explicit_only"):
        parts.append(
            "winget updates this app only when it is picked by name; it is not included in Update all."
        )
    elif str(row.get("source")) == "msstore":
        parts.append("Microsoft Store app  ·  if this fails, update it in the Microsoft Store.")
    if last and last.get("state") in ("failed", "timed_out") and last.get("message"):
        parts.append(f"Last attempt: {last['message']}")
    if retry_futile(last):
        parts.append(RETRY_LEFT_OUT_TEXT)
    return "  ·  ".join(parts)


def update_item(row: Mapping[str, Any]) -> dict[str, Any]:
    """The request item of an upgrade row."""
    return {
        "id": str(row["id"]),
        "source": str(row.get("source") or "winget"),
        "name": str(row.get("name") or row["id"]),
        "from": row.get("installed"),
        "to": row.get("available"),
    }


def pause_text(value: Mapping[str, Any], now: datetime | None = None, *, outside: bool = False) -> str:
    """The Pause row's state line."""
    if value.get("paused"):
        text = f"◐ Paused until {date_text(value.get('until'), now)}"
        return text + ("  ·  set outside " + APP_NAME if outside else "")
    if value.get("expired") and value.get("until"):
        return f"○ The last pause ended on {date_text(value.get('until'), now)}"
    return "○ Updates are on"


def hours_text(start: int, end: int) -> str:
    """ "08:00–17:00"."""
    return f"{int(start):02}:00–{int(end):02}:00"


def active_span(start: int, end: int) -> int:
    """Hours from `start` to `end`, across midnight when `end` is earlier."""
    return (int(end) + 24 - int(start)) % 24


def active_hours_error(start: int, end: int) -> str | None:
    """Why `start`–`end` can't be active hours, or None."""
    if start == end:
        return "Start and end must differ."
    if active_span(start, end) > MAX_ACTIVE_SPAN:
        return f"Active hours can span at most {MAX_ACTIVE_SPAN} hours."
    return None


def active_hours_text(value: Mapping[str, Any], *, outside: bool = False) -> str:
    if value.get("automatic") or value.get("start") is None or value.get("end") is None:
        return "○ Windows adjusts them automatically"
    text = f"✓ {hours_text(value['start'], value['end'])}"
    if value.get("policy"):
        return text + "  ·  set by a policy"
    return text + ("  ·  set outside " + APP_NAME if outside else "")


def valid_package_id(text: str, source: str = "winget") -> bool:
    """Mirrors the engine: 1 to 128 characters, no whitespace, control characters or
    `\\ / : * ? " < > |`, not starting with '-'; a Microsoft Store id is 12 to 14 letters and
    digits, any other id 2 to 8 non-empty parts separated by dots."""
    if not text or len(text) > 128 or text.startswith("-"):
        return False
    if any(c.isspace() or ord(c) < 32 or ord(c) == 127 or c in '\\/:*?"<>|' for c in text):
        return False
    if source.lower() == "msstore":
        return 12 <= len(text) <= 14 and text.isascii() and text.isalnum()
    parts = text.split(".")
    return 2 <= len(parts) <= 8 and all(parts)


def group_apps(apps: Iterable[Mapping[str, Any]]) -> list[tuple[str, list[dict[str, Any]]]]:
    """(heading, apps) per category in display order, without empty categories; apps of an
    unknown category come last under "Other"."""
    groups: dict[str, list[dict[str, Any]]] = {key: [] for key in CATEGORY_TITLES}
    other: list[dict[str, Any]] = []
    for app in apps:
        groups.get(str(app.get("category")), other).append(dict(app))
    out = [(CATEGORY_TITLES[key], items) for key, items in groups.items() if items]
    if other:
        out.append(("Other", other))
    return out


def install_summary(apps: Sequence[Mapping[str, Any]], installed: Iterable[str] | None) -> str:
    if installed is None:
        return INSTALL_CHECKING_TEXT
    ids = {i.lower() for i in installed}
    count = sum(1 for app in apps if str(app.get("id", "")).lower() in ids)
    return f"{INSTALL_INTRO}  ·  {count} of {len(apps)} already installed"


def batch_line(result: Mapping[str, Any] | None) -> str:
    """The result line of a finished batch: "✓ 3 updated  ·  ⚠ 1 failed  ·  restart needed for 1"."""
    if not result:
        return ""
    installing = result.get("kind") == "install"
    states = [str(item.get("state")) for item in result.get("items") or []]

    def count(*names: str) -> int:
        return sum(1 for s in states if s in names)

    parts = []
    done = count("succeeded")
    if done:
        parts.append(f"✓ {done} {'installed' if installing else 'updated'}")
    already = count("already_installed", "already_current")
    if already:
        parts.append(f"{already} already {'installed' if installing else 'up to date'}")
    failed = count("failed", "timed_out")
    if failed:
        parts.append(f"⚠ {failed} failed")
    restart = count("restart_required")
    if restart:
        parts.append(f"restart needed for {restart}")
    left = count("left_running")
    if left:
        parts.append(f"{left} still running")
    not_started = count("not_started", "queued", "running")
    if not_started:
        parts.append(f"{not_started} not started")
    return "  ·  ".join(parts) or "Nothing was changed."


def edition_text(edition: Mapping[str, Any]) -> str:
    """ "Windows 11 Home 25H2  ·  build 26200.9457"."""
    head = " ".join(str(p) for p in (edition.get("name"), edition.get("version")) if p)
    if edition.get("build"):
        return f"{head}  ·  build {edition['build']}" if head else f"build {edition['build']}"
    return head


def pause_days(choice: str) -> int:
    """Days of a pause menu choice ("2 weeks" -> 14)."""
    return int(choice.split()[0]) * 7


def defer_choice(days: int | None) -> str:
    return DEFER_CHOICES[0] if not days else f"{int(days)} days"


def defer_days(choice: str) -> int | None:
    return None if choice == DEFER_CHOICES[0] else int(choice.split()[0])


# -- widget helpers ------------------------------------------------------------------


def _set_state(widget: Any, enabled: bool) -> None:
    state = "normal" if enabled else "disabled"
    if widget.cget("state") != state:
        widget.configure(state=state)


def _set_color(label: ctk.CTkLabel, color: str) -> None:
    if label.cget("text_color") != color:
        label.configure(text_color=color)


def _neutral(master: tk.Misc, text: str, command: Callable[[], None], **kw: Any) -> ctk.CTkButton:
    options: dict[str, Any] = {
        "height": 30,
        "font": _font(12),
        "fg_color": theme.BUTTON_NEUTRAL,
        "hover_color": theme.BUTTON_NEUTRAL_HOVER,
        "text_color": theme.INK,
    }
    options.update(kw)
    return ctk.CTkButton(master, text=text, command=command, **options)


def _accent(master: tk.Misc, text: str, command: Callable[[], None], **kw: Any) -> ctk.CTkButton:
    options: dict[str, Any] = {
        "height": 30,
        "font": _font(12, "bold"),
        "fg_color": theme.ACCENT,
        "hover_color": theme.ACCENT_HOVER,
        "text_color": theme.INK,
    }
    options.update(kw)
    return ctk.CTkButton(master, text=text, command=command, **options)


def _link(master: tk.Misc, text: str, command: Callable[[], None]) -> ctk.CTkButton:
    button = ctk.CTkButton(
        master,
        text=text,
        command=command,
        height=22,
        font=_font(11),
        fg_color="transparent",
        hover_color=theme.SURFACE_RAISED,
        text_color=theme.ACCENT,
    )
    button.configure(width=max(40, len(text) * 7))
    return button


def _label(
    master: tk.Misc, text: str, size: int, color: str, *, weight: str = "normal", **kw: Any
) -> ctk.CTkLabel:
    return ctk.CTkLabel(
        master,
        text=text,
        font=_font(size, weight),
        text_color=color,
        anchor="w",
        justify="left",
        **kw,
    )


def _managed(widget: Any) -> bool:
    """Whether `widget` is gridded; a scrollable frame is gridded through its outer frame."""
    return bool(getattr(widget, "_parent_frame", widget).winfo_manager())


def _show(widget: Any, visible: bool, **grid: Any) -> None:
    """Grids `widget` with `grid` options when `visible`, forgets it otherwise."""
    if visible:
        if not _managed(widget):
            widget.grid(**grid)
    elif _managed(widget):
        widget.grid_forget()


class _Wrapping:
    """Keeps the wrap length of some labels at the width their container gets."""

    def _init_wrapping(self, container: Any, labels: Sequence[ctk.CTkLabel], inset: int, widest: int) -> None:
        self._wrap_labels = list(labels)
        self._wrap_inset = inset
        self._wrap_widest = widest
        self._wrap_width = -1
        container.bind("<Configure>", self._rewrap, add="+")

    def _rewrap(self, event: tk.Event) -> None:
        if event.width == self._wrap_width:
            return
        self._wrap_width = event.width
        scale = ctk.ScalingTracker.get_widget_scaling(event.widget) or 1.0
        wrap = fitted_wrap(event.width / scale - self._wrap_inset, self._wrap_widest)
        for label in self._wrap_labels:
            if label.winfo_exists() and label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)


# -- job strip -----------------------------------------------------------------------


class JobStrip(ctk.CTkFrame, _Wrapping):
    """Follows the winget job that runs: its progress line and bar, Stop, Show output and Open
    log, and afterwards a result line: a batch's outcome, or why a check ended early. A batch's
    line and output stay through the check that follows it. The output box is hidden until
    asked for; lines read while it is hidden are kept and inserted at most `FLUSH_LINES` per
    `flush`.

    A job without a known fraction moves the bar back and forth; the frame loop drives it
    through `animate`, so no timer of its own outlives the window.
    """

    PAD = 14

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_stop: Callable[[], None],
        on_output: Callable[[bool], None],
        on_open_log: Callable[[], None],
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_output = on_output
        self.grid_columnconfigure(0, weight=1)
        self.kind: str | None = None
        self.running = False
        self.output_shown = False
        self._indeterminate = False
        self._buffer = OutputBuffer()
        self._inserted = 0
        # Title of the job followed now, and title and kind of the job whose result line is shown.
        self._title = ""
        self._result_title = ""
        self._result_kind: str | None = None
        # Whether a check began after the batch whose result line is shown.
        self._result_checked = False

        self.label = _label(self, "", 11, theme.INK_SECONDARY, wraplength=PANEL_WRAP)
        self.label.grid(row=0, column=0, sticky="ew", padx=self.PAD, pady=(8, 0))
        self.bar = ctk.CTkProgressBar(
            self, mode="determinate", height=8, progress_color=theme.ACCENT, fg_color=theme.BASELINE
        )
        self.bar.set(0)
        self.bar.grid(row=1, column=0, sticky="ew", padx=self.PAD, pady=(6, 0))
        buttons = ctk.CTkFrame(self, fg_color="transparent")
        buttons.grid(row=2, column=0, sticky="w", padx=self.PAD - 2, pady=(6, 8))
        self.stop_button = _neutral(buttons, "Stop", on_stop, width=90)
        self.stop_button.grid(row=0, column=0, padx=(0, 8))
        self.output_button = _neutral(buttons, "Show output", self._toggle_output, width=110)
        self.output_button.grid(row=0, column=1, padx=(0, 8))
        self.log_button = _neutral(buttons, "Open log", on_open_log, width=90)
        self.log_button.grid(row=0, column=2)
        self.result_label = _label(self, "", 11, theme.INK_SECONDARY, wraplength=PANEL_WRAP)
        self.output = ctk.CTkTextbox(
            self,
            height=150,
            font=_mono(10),
            fg_color=theme.PAGE,
            text_color=theme.INK_SECONDARY,
            border_width=1,
            border_color=theme.BORDER,
            wrap="none",
            undo=False,
            state="disabled",
        )
        self._init_wrapping(self, [self.label, self.result_label], 2 * self.PAD + WRAP_MARGIN, PANEL_WRAP)

    # -- lines -------------------------------------------------------------------------

    @property
    def pending_output(self) -> int:
        """Output lines read from the engine and not yet in the output box."""
        return self._buffer.pending

    def append(self, lines: Iterable[str], skipped: int = 0) -> None:
        self._buffer.feed(lines, skipped)

    def flush(self, limit: int = FLUSH_LINES) -> int:
        """Inserts up to `limit` waiting lines; keeps the newest `MAX_OUTPUT_LINES`."""
        lines = self._buffer.take(limit)
        if not lines:
            return 0
        self.output.configure(state="normal")
        self.output.insert("end", "\n".join(lines) + "\n")
        self._inserted += len(lines)
        extra = self._inserted - MAX_OUTPUT_LINES
        if extra > 0:
            self.output.delete("1.0", f"{extra + 1}.0")
            self._inserted -= extra
        self.output.configure(state="disabled")
        self.output.see("end")
        return len(lines)

    def _toggle_output(self) -> None:
        self.output_shown = not self.output_shown
        set_text(self.output_button, "Hide output" if self.output_shown else "Show output")
        _show(self.output, self.output_shown, row=4, column=0, sticky="nsew", padx=10, pady=(0, 10))
        self._on_output(self.output_shown)

    # -- job ---------------------------------------------------------------------------

    @property
    def result_text(self) -> str:
        """The result line shown, "" when there is none."""
        return str(self.result_label.cget("text"))

    @property
    def shows_batch_result(self) -> bool:
        """Whether the result line shown is an update or install batch's, which stays while the
        check that follows the batch runs and after it ends."""
        return self._result_kind in ("upgrade", "install") and bool(self.result_text)

    def begin(self, kind: str, title: str) -> None:
        """Starts following a new job, clearing the output and the result line shown. A check
        follows every batch, so the first check after a batch keeps the batch's line and its
        output, and adds its own output after a line with its title; the job after that clears
        them."""
        self.kind = kind
        self.running = True
        self._title = title
        batch_line = self._result_kind not in (None, "scan")
        if kind == "scan" and batch_line and not self._result_checked:
            self._result_checked = True
            self._buffer.feed([f"== {title} =="])
        else:
            self._buffer.clear()
            self._inserted = 0
            self.output.configure(state="normal")
            self.output.delete("1.0", "end")
            self.output.configure(state="disabled")
            self._result_kind = None
            set_text(self.result_label, "")
            _show(self.result_label, False)
        set_text(self.label, title + "…")
        _set_color(self.label, theme.INK_SECONDARY)
        set_text(self.stop_button, "Stop" if kind == "scan" else "Stop after this app")
        self.stop_button.configure(width=90 if kind == "scan" else 150)
        _set_state(self.stop_button, True)
        _show(self.stop_button, True, row=0, column=0, padx=(0, 8))
        _set_state(self.log_button, True)
        self._set_progress(None)

    def update_job(self, view: Mapping[str, Any]) -> None:
        """Shows a job view's progress line and fraction."""
        line = view.get("progress_line") or view.get("title") or ""
        set_text(self.label, str(line))
        self._set_progress(view.get("progress"))
        if view.get("cancel_requested") and self.running:
            _set_state(self.stop_button, False)

    def _set_progress(self, progress: float | None) -> None:
        if progress is None:
            self._indeterminate = True
            return
        self._indeterminate = False
        value = max(0.0, min(1.0, float(progress) / 100.0))
        if abs(self.bar.get() - value) > 0.001:
            self.bar.set(value)

    def animate(self, now: float) -> None:
        """Moves the bar of a job without a known fraction; called by the frame loop."""
        if not (self.running and self._indeterminate):
            return
        phase = (now % 2.0) / 2.0
        self.bar.set(0.5 - 0.5 * math.cos(2 * math.pi * phase))

    def set_stopping(self, text: str) -> None:
        set_text(self.label, text)
        _set_state(self.stop_button, False)

    def finish(self, result_line: str | None = None, color: str = theme.INK_SECONDARY) -> None:
        """Ends following and shows `result_line` as this job's result until `begin` clears it;
        None keeps the line shown now."""
        self.running = False
        self._indeterminate = False
        self.bar.set(1.0)
        _show(self.stop_button, False)
        if result_line is not None:
            self._result_title = self._title
            self._result_kind = self.kind
            self._result_checked = False
            set_text(self.result_label, result_line)
            _set_color(self.result_label, color)
        shown = bool(self.result_text)
        if shown:
            set_text(self.label, self._result_title)
            _set_color(self.label, theme.INK_SECONDARY)
        _show(self.result_label, shown, row=3, column=0, sticky="ew", padx=self.PAD, pady=(0, 8))

    def lose(self, text: str) -> None:
        self.running = False
        self._indeterminate = False
        self.bar.set(0)
        _show(self.stop_button, False)
        set_text(self.label, text)
        _set_color(self.label, theme.CRITICAL)


# -- App updates ---------------------------------------------------------------------


class UpdateRow(ctk.CTkFrame):
    """One app with an update: a check box, its name and id, the versions, its source and the
    status of its update. `light` rows draw their texts with plain Tk labels.

    The versions, the source and the status are columns of a fixed width whose texts wrap
    inside them; the name and the id wrap in the width of the list the other columns leave."""

    VERSION_WIDTH = 210
    SOURCE_WIDTH = 70
    STATUS_WIDTH = 190
    # A check box without text: the box, the gap CustomTkinter keeps after it and the empty
    # label that follows the gap.
    CHECK_WIDTH = 24 + 6 + 2
    # Horizontal padding of the check box, the name and id, the three fixed columns, and of
    # the row in its list.
    PADDING = (10 + 4) + (4 + 10) + 2 * 5 + 2 * 5 + (5 + 10) + 2 * 6
    TEXT_INSET = CHECK_WIDTH + VERSION_WIDTH + SOURCE_WIDTH + STATUS_WIDTH + PADDING + WRAP_MARGIN
    # Vertical padding of a plain Tk label, as much as a Tk label's default border and padding.
    LIGHT_PADY = 3

    def __init__(
        self,
        master: tk.Misc,
        row: dict[str, Any],
        *,
        selected: bool,
        note: str,
        on_toggle: Callable[[], None],
        on_open_uri: Callable[[str], None],
        light: bool = False,
        wraplength: int = ROW_WRAP,
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.row = row
        self.light = light
        self.can_select = selectable(row)
        self.grid_columnconfigure(1, weight=1)
        self.var = tk.IntVar(value=1 if selected and self.can_select else 0)
        self.check = ctk.CTkCheckBox(
            self, text="", width=24, variable=self.var, onvalue=1, offvalue=0, command=on_toggle
        )
        self.check.grid(row=0, column=0, rowspan=2, padx=(10, 4), pady=8, sticky="n")
        if not self.can_select:
            self.check.configure(state="disabled")
        names = ctk.CTkFrame(self, fg_color="transparent")
        names.grid(row=0, column=1, sticky="ew", padx=(4, 10), pady=(8, 0))
        names.grid_columnconfigure(0, weight=1)
        self._texts: list[Any] = []
        self.name_label = self._text(names, str(row.get("name") or row.get("id")), 12, theme.INK, wraplength)
        self.name_label.grid(row=0, column=0, sticky="w")
        self.id_label = self._text(names, str(row.get("id")), 10, theme.INK_MUTED, wraplength, mono=True)
        self.id_label.grid(row=1, column=0, sticky="w")
        self.note_label = self._text(self, note, 10, theme.INK_MUTED, wraplength)
        if note:
            self.note_label.grid(row=1, column=1, sticky="w", padx=(4, 10), pady=(0, 8))
        version = version_change(row.get("installed"), row.get("available"))
        self.version_label = self._text(self, version, 11, theme.INK_SECONDARY, 0, width=self.VERSION_WIDTH)
        self.source_label = self._text(
            self, str(row.get("source") or ""), 11, theme.INK_MUTED, 0, width=self.SOURCE_WIDTH
        )
        self.status_label = self._text(self, "", 11, theme.INK_MUTED, 0, width=self.STATUS_WIDTH)
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        fixed = (
            (self.version_label, self.VERSION_WIDTH, (5, 5)),
            (self.source_label, self.SOURCE_WIDTH, (5, 5)),
            (self.status_label, self.STATUS_WIDTH, (5, 10)),
        )
        for column, (label, width, padx) in enumerate(fixed, 2):
            label.grid(row=0, column=column, sticky="w", padx=padx, pady=(8, 0))
            if light:
                # A Tk label is only as wide as its text, so its column keeps the width.
                self.grid_columnconfigure(column, minsize=round(width * scale) + sum(padx))
        self.store_link: ctk.CTkButton | None = None
        if str(row.get("source")) == "msstore":
            self.store_link = _link(self, "Open Store updates", lambda: on_open_uri(STORE_UPDATES_URI))
            self.store_link.grid(row=1, column=4, sticky="w", padx=(5, 10), pady=(0, 6))
        if not note and self.store_link is None:
            self.grid_rowconfigure(1, minsize=8)

    def _text(
        self,
        master: tk.Misc,
        text: str,
        size: int,
        color: str,
        wraplength: int,
        *,
        width: int = 0,
        mono: bool = False,
    ) -> Any:
        """A text of the row. With `width` it is a column that wide whose text wraps inside it
        (`wraplength` 0); otherwise it wraps at `wraplength`, which then follows the row's width
        through `set_wraplength`. A plain Tk label has no border or side padding, so its text
        takes its whole wrap length, as in a CustomTkinter label."""
        wrap = width or wraplength
        if self.light:
            scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
            family = theme.MONO_FAMILY if mono else theme.FONT_FAMILY
            label = tk.Label(
                master,
                text=text,
                font=(family, -round(size * 4 / 3 * scale)),
                fg=color,
                bg=theme.SURFACE_RAISED,
                anchor="w",
                justify="left",
                borderwidth=0,
                padx=0,
                pady=self.LIGHT_PADY,
                wraplength=round(wrap * scale) if wrap else 0,
            )
        else:
            options: dict[str, Any] = {"wraplength": wrap} if wrap else {}
            if width:
                options["width"] = width
            label = ctk.CTkLabel(
                master,
                text=text,
                font=_mono(size) if mono else _font(size),
                text_color=color,
                anchor="w",
                justify="left",
                **options,
            )
        if wraplength:
            self._texts.append(label)
        return label

    @property
    def selected(self) -> bool:
        return self.can_select and self.var.get() == 1

    def set_selected(self, selected: bool) -> None:
        self.var.set(1 if selected and self.can_select else 0)

    def set_enabled(self, enabled: bool) -> None:
        _set_state(self.check, enabled and self.can_select)

    def set_wraplength(self, wrap: int) -> None:
        for label in self._texts:
            if isinstance(label, ctk.CTkLabel):
                if label.cget("wraplength") != wrap:
                    label.configure(wraplength=wrap)
            else:
                scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
                label.configure(wraplength=round(wrap * scale))

    def set_status(self, text: str, color_name: str) -> None:
        color = getattr(theme, color_name, theme.INK_MUTED)
        if self.light:
            self.status_label.configure(text=text, fg=color)
        else:
            set_text(self.status_label, text)
            _set_color(self.status_label, color)

    @property
    def status_text(self) -> str:
        return str(self.status_label.cget("text"))


class AppUpdatesView(ctk.CTkFrame, _Wrapping):
    """The App updates card: Check again, Update selected and Update all, the check's summary
    and warnings, and the list of apps with an update. When winget can't be used, a message
    takes the list's place."""

    PAD = 14

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_check: Callable[[], None],
        on_update: Callable[[bool], None],
        on_open_uri: Callable[[str], None],
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_check = on_check
        self._on_update = on_update
        self._on_open_uri = on_open_uri
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(4, weight=1)
        self.rows: list[UpdateRow] = []
        self.result: dict[str, Any] | None = None
        # The session's last batch result of each app (lowercased id) the rows were drawn with.
        self._last_results: dict[str, dict[str, Any]] = {}
        self._enabled = True
        self._job_running = False
        self._ready = True
        self._minimum = "1.6.0"
        self._row_wrap = ROW_WRAP
        self._list_width = -1

        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=self.PAD, pady=(10, 0))
        header.grid_columnconfigure(0, weight=1)
        _label(header, "App updates", 13, theme.INK_SECONDARY, weight="bold").grid(
            row=0, column=0, sticky="w"
        )
        self.check_button = _neutral(header, "Check again", on_check, width=100)
        self.check_button.grid(row=0, column=1, padx=(8, 0))
        self.selected_button = _accent(header, "Update selected (0)", lambda: on_update(False))
        self.selected_button.grid(row=0, column=2, padx=(8, 0))
        self.all_button = _accent(
            header, "Update all (0)", lambda: on_update(True), width=130, font=_font(12)
        )
        self.all_button.grid(row=0, column=3, padx=(8, 0))

        self.summary = _label(self, NOT_CHECKED_TEXT, 11, theme.INK_MUTED, wraplength=PANEL_WRAP)
        self.summary.grid(row=1, column=0, sticky="ew", padx=self.PAD, pady=(4, 0))
        self.warnings = _label(self, "", 11, theme.WARNING, wraplength=PANEL_WRAP)
        self.notice = _label(
            self, f"{IRREVERSIBLE_NOTE}  {AGREEMENTS_NOTE}", 10, theme.INK_MUTED, wraplength=PANEL_WRAP
        )
        self.notice.grid(row=3, column=0, sticky="ew", padx=self.PAD, pady=(4, 6))

        self.list = ctk.CTkScrollableFrame(self, fg_color="transparent")
        self.list.grid_columnconfigure(0, weight=1)
        self.list.grid(row=4, column=0, sticky="nsew", padx=6, pady=(0, 8))
        self.list.bind("<Configure>", self._list_resized, add="+")
        self.empty = _label(self.list, "", 12, theme.INK_MUTED, wraplength=ROW_WRAP)
        self.help = _label(self.list, PINNED_HELP_TEXT, 10, theme.INK_MUTED, wraplength=ROW_WRAP)

        self.message = ctk.CTkFrame(self, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.message.grid_columnconfigure(0, weight=1)
        self.message_label = _label(self.message, "", 12, theme.INK_SECONDARY, wraplength=PANEL_WRAP)
        self.message_label.grid(row=0, column=0, sticky="ew", padx=12, pady=(12, 6))
        message_buttons = ctk.CTkFrame(self.message, fg_color="transparent")
        message_buttons.grid(row=1, column=0, sticky="w", padx=10, pady=(0, 12))
        self.store_button = _accent(
            message_buttons, "Get App Installer", lambda: on_open_uri(APP_INSTALLER_STORE_URI), width=150
        )
        self.message_check = _neutral(message_buttons, "Check again", on_check, width=100)
        self._message_buttons = message_buttons
        self._init_wrapping(
            self,
            [self.summary, self.warnings, self.notice, self.message_label],
            2 * self.PAD + 24,
            PANEL_WRAP,
        )
        self._refresh_buttons()

    # -- state -------------------------------------------------------------------------

    def _list_resized(self, event: tk.Event) -> None:
        if event.width == self._list_width:
            return
        self._list_width = event.width
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        width = event.width / scale
        self._row_wrap = fitted_wrap(width - UpdateRow.TEXT_INSET, ROW_WRAP)
        for row in self.rows:
            row.set_wraplength(self._row_wrap)
        wrap = fitted_wrap(width - 2 * 10 - WRAP_MARGIN, ROW_WRAP)
        for label in (self.empty, self.help):
            if label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)

    def _show_message(self, text: str | None, *, store: bool = False, check: bool = False) -> None:
        """Replaces the list with `text` and its buttons, or puts the list back (None)."""
        if text is None:
            _show(self.message, False)
            _show(self.list, True, row=4, column=0, sticky="nsew", padx=6, pady=(0, 8))
            return
        set_text(self.message_label, text)
        _show(self.store_button, store, row=0, column=0, padx=(0, 8))
        _show(self.message_check, check, row=0, column=1)
        _show(self._message_buttons, store or check, row=1, column=0, sticky="w", padx=10, pady=(0, 12))
        _show(self.list, False)
        _show(self.message, True, row=4, column=0, sticky="new", padx=self.PAD, pady=(0, 12))

    def set_winget(self, status: Mapping[str, Any] | None) -> None:
        """Shows what winget's status means: nothing when it is ready."""
        availability = (status or {}).get("availability", "ready")
        self._ready = availability == "ready"
        self._minimum = str((status or {}).get("min_version") or self._minimum)
        if availability == "missing":
            self._show_message(
                str((status or {}).get("message") or WINGET_MISSING_TEXT), store=True, check=True
            )
        elif availability == "other_user":
            self._show_message(OTHER_USER_TEXT)
        elif availability == "user_unknown":
            self._show_message(USER_UNKNOWN_TEXT)
        else:
            self._show_message(None)
        self._refresh_buttons()

    def set_checking(self, elapsed_ms: float | None = None) -> None:
        set_text(self.summary, checking_text(elapsed_ms))
        _set_color(self.summary, theme.INK_MUTED)

    def set_enabled(self, enabled: bool, job_running: bool) -> None:
        self._enabled = enabled
        self._job_running = job_running
        self._refresh_buttons()

    def _refresh_buttons(self) -> None:
        selected = len(self.selected_items())
        everything = len(self.all_items())
        set_text(self.selected_button, f"Update selected ({selected})")
        set_text(self.all_button, f"Update all ({everything})")
        can_act = self._enabled and not self._job_running and self._ready
        _set_state(self.check_button, self._enabled and not self._job_running)
        _set_state(self.message_check, self._enabled and not self._job_running)
        _set_state(self.selected_button, can_act and selected > 0)
        _set_state(self.all_button, can_act and everything > 0)
        for row in self.rows:
            row.set_enabled(not self._job_running)

    def show_scan(
        self, result: Mapping[str, Any] | None, last_results: Mapping[str, Mapping[str, Any]]
    ) -> None:
        """Lists a finished check's apps with their default selection; `last_results` holds the
        session's last batch result of each app by lowercased id."""
        try:
            self._show_scan(result, last_results)
        except Exception as exc:  # noqa: BLE001 - malformed engine data is shown, not raised
            log.exception("showing the update check failed")
            self.show_error(f"The check's result could not be read: {exc}")

    def _show_scan(
        self, result: Mapping[str, Any] | None, last_results: Mapping[str, Mapping[str, Any]]
    ) -> None:
        self.result = dict(result) if result else None
        self._last_results = {str(key): dict(item) for key, item in last_results.items()}
        error = (result or {}).get("error")
        if error and error.get("outdated"):
            version = str((result or {}).get("winget_version") or "?")
            text = str(
                error.get("message") or WINGET_OUTDATED_TEXT.format(version=version, minimum=self._minimum)
            )
            self._show_message(text, store=True, check=True)
            set_text(self.summary, "⚠ winget is too old.")
            _set_color(self.summary, theme.WARNING)
            self._clear_rows()
            self._refresh_buttons()
            return
        if self._ready:
            self._show_message(None)
        set_text(self.summary, scan_summary(result))
        _set_color(self.summary, theme.WARNING if error else theme.INK_MUTED)
        warnings = scan_warnings(result)
        set_text(self.warnings, "\n".join(f"⚠ {w}" for w in warnings))
        _show(self.warnings, bool(warnings), row=2, column=0, sticky="ew", padx=self.PAD, pady=(2, 0))
        self._clear_rows()
        upgrades = [dict(r) for r in (result or {}).get("upgrades") or []]
        chosen = default_selection(upgrades, self._last_results)
        light = len(upgrades) > LIGHT_ROWS_ABOVE
        for index, row in enumerate(upgrades):
            last = self._last_results.get(str(row.get("id", "")).lower())
            widget = UpdateRow(
                self.list,
                row,
                selected=str(row["id"]) in chosen,
                note=row_note(row, last),
                on_toggle=self._refresh_buttons,
                on_open_uri=self._on_open_uri,
                light=light,
                wraplength=self._row_wrap,
            )
            widget.grid(row=index, column=0, sticky="ew", padx=6, pady=3)
            self.rows.append(widget)
        if not upgrades:
            set_text(self.empty, NO_UPDATES_TEXT if result and not error else "")
            _show(self.empty, bool(result) and not error, row=0, column=0, sticky="w", padx=10, pady=10)
        self.help.grid(row=len(upgrades) + 1, column=0, sticky="w", padx=10, pady=(6, 4))
        # A resize can arrive while a row is being built, before it is listed.
        for row in self.rows:
            row.set_wraplength(self._row_wrap)
        self._refresh_buttons()

    def _clear_rows(self) -> None:
        for row in self.rows:
            row.destroy()
        self.rows = []
        _show(self.empty, False)

    def show_error(self, text: str) -> None:
        set_text(self.summary, f"⚠ {text}")
        _set_color(self.summary, theme.WARNING)

    def set_items(self, items: Sequence[Mapping[str, Any]], kind: str = "upgrade") -> None:
        """Shows the batch state of each app that is in the list."""
        by_id = {str(i.get("id", "")).lower(): i for i in items}
        for row in self.rows:
            item = by_id.get(str(row.row.get("id", "")).lower())
            if item is None:
                continue
            text, color = item_status(item.get("state"), item.get("message"), kind, item.get("progress"))
            row.set_status(text, color)

    def selected_items(self) -> list[dict[str, Any]]:
        return [update_item(r.row) for r in self.rows if r.selected]

    def all_items(self) -> list[dict[str, Any]]:
        """The apps Update all takes: selectable, not only by name, not Store apps, and not
        apps whose last attempt failed in a way a retry can't change."""
        chosen = default_selection([r.row for r in self.rows], self._last_results)
        return [update_item(r.row) for r in self.rows if str(r.row["id"]) in chosen]


# -- Install apps --------------------------------------------------------------------


class AppTile(ctk.CTkFrame):
    """One app of the install list: a check box, its name and id on the left, and on the right
    whether it is installed or how its install went. In edit mode it offers Remove.

    The status wraps at `STATUS_WRAP`; the name and the id wrap in the width the status
    leaves them, which follows the tile's width (`set_tile_width`) and the status text."""

    # Widest the status gets before it wraps, and the width the Remove link takes.
    STATUS_WRAP = 200
    REMOVE_WIDTH = 60
    # Width of a tile that neither its texts nor its status get: the check box and the gaps.
    INSET = 10 + 24 + 6 + 4 + 2 * 4 + 4 + 10 + WRAP_MARGIN
    MIN_TEXT_WRAP = 100
    # Width of a tile in the narrowest window, used until the list tells its real width.
    DEFAULT_WIDTH = 420

    def __init__(
        self,
        master: tk.Misc,
        app: dict[str, Any],
        *,
        installed: bool,
        editing: bool,
        on_toggle: Callable[[], None],
        on_remove: Callable[[dict[str, Any]], None],
        width: float = DEFAULT_WIDTH,
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.app = app
        self.installed = installed
        self._tile_width = width
        self.grid_columnconfigure(1, weight=1)
        self.var = tk.IntVar(value=0)
        self.check = ctk.CTkCheckBox(self, text="", width=24, variable=self.var, command=on_toggle)
        self.check.grid(row=0, column=0, padx=(10, 4), pady=8, sticky="n")
        texts = ctk.CTkFrame(self, fg_color="transparent")
        texts.grid(row=0, column=1, sticky="new", padx=4, pady=8)
        self.name_label = _label(texts, str(app.get("name") or app.get("id")), 12, theme.INK)
        self.name_label.grid(row=0, column=0, sticky="w")
        self.id_label = ctk.CTkLabel(
            texts,
            text=str(app.get("id")),
            font=_mono(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
        )
        self.id_label.grid(row=1, column=0, sticky="w")
        side = ctk.CTkFrame(self, fg_color="transparent")
        side.grid(row=0, column=2, sticky="ne", padx=(4, 8), pady=(8, 6))
        self.badge = _label(
            side, "✓ Installed" if installed else "", 11, theme.GOOD, wraplength=self.STATUS_WRAP
        )
        self.badge.grid(row=0, column=0, sticky="e", padx=(0, 2))
        self.remove = _link(side, "Remove", lambda: on_remove(app))
        if editing:
            self.remove.grid(row=1, column=0, sticky="e", pady=(4, 0))
        if installed:
            self.check.configure(state="disabled")
        self._rewrap()

    @property
    def selected(self) -> bool:
        return not self.installed and self.var.get() == 1

    def set_enabled(self, enabled: bool) -> None:
        _set_state(self.check, enabled and not self.installed)

    def set_status(self, text: str, color_name: str) -> None:
        set_text(self.badge, text)
        _set_color(self.badge, getattr(theme, color_name, theme.INK_MUTED))
        self._rewrap()

    def set_tile_width(self, width: float) -> None:
        """Tells the tile its width in CustomTkinter units, so its texts wrap inside it."""
        self._tile_width = width
        self._rewrap()

    def _side_width(self) -> int:
        """Width the right side takes: the status up to its wrap length, or the Remove link."""
        text = str(self.badge.cget("text"))
        status = 0
        if text:
            status = min(self.STATUS_WRAP, int(self.badge.cget("font").measure(text)) + WRAP_MARGIN)
        return max(status, self.REMOVE_WIDTH) if self.remove.winfo_manager() else status

    def _rewrap(self) -> None:
        room = int(self._tile_width - self.INSET - self._side_width())
        wrap = max(self.MIN_TEXT_WRAP, room // WRAP_STEP * WRAP_STEP)
        for label in (self.name_label, self.id_label):
            if label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)


class InstallAppsView(ctk.CTkFrame, _Wrapping):
    """The Install apps card: the list by category, Install selected and the list editor."""

    PAD = 14
    # Gap on each side of a tile; two tiles share the list's width.
    TILE_PADX = 4

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_install: Callable[[], None],
        on_save_list: Callable[[list[dict[str, Any]] | None], None],
        on_check: Callable[[], None],
        on_open_uri: Callable[[str], None],
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_save_list = on_save_list
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(3, weight=1)
        self.apps: list[dict[str, Any]] = []
        self.installed: list[str] | None = None
        self.tiles: list[AppTile] = []
        self.editing = False
        self._enabled = True
        self._job_running = False
        self._ready = True
        self._body_width = -1
        self._tile_width: float = AppTile.DEFAULT_WIDTH

        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=self.PAD, pady=(10, 0))
        header.grid_columnconfigure(0, weight=1)
        _label(header, "Install apps", 13, theme.INK_SECONDARY, weight="bold").grid(
            row=0, column=0, sticky="w"
        )
        self.edit_button = _neutral(header, "Edit list", self._toggle_editing, width=100)
        self.edit_button.grid(row=0, column=1, padx=(8, 0))
        self.install_button = _accent(header, "Install selected (0)", on_install)
        self.install_button.grid(row=0, column=2, padx=(8, 0))
        self.summary = _label(self, INSTALL_CHECKING_TEXT, 11, theme.INK_MUTED, wraplength=PANEL_WRAP)
        self.summary.grid(row=1, column=0, sticky="ew", padx=self.PAD, pady=(4, 0))
        self.notice = _label(
            self, f"{IRREVERSIBLE_NOTE}  {AGREEMENTS_NOTE}", 10, theme.INK_MUTED, wraplength=PANEL_WRAP
        )
        self.notice.grid(row=2, column=0, sticky="ew", padx=self.PAD, pady=(4, 6))

        self.body = ctk.CTkScrollableFrame(self, fg_color="transparent")
        self.body.grid_columnconfigure(0, weight=1)
        self.body.grid(row=3, column=0, sticky="nsew", padx=6, pady=(0, 8))
        self.body.bind("<Configure>", self._body_resized, add="+")
        self._groups = ctk.CTkFrame(self.body, fg_color="transparent")
        self._groups.grid(row=0, column=0, sticky="ew")
        self._groups.grid_columnconfigure(0, weight=1)

        self.form = ctk.CTkFrame(self.body, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.form.grid_columnconfigure(0, weight=1)
        self.form.grid_columnconfigure(1, weight=1)
        self.id_entry = ctk.CTkEntry(
            self.form, placeholder_text="Package id, e.g. Mozilla.Firefox", font=_font(12)
        )
        self.id_entry.grid(row=0, column=0, sticky="ew", padx=(10, 4), pady=(10, 4))
        self.name_entry = ctk.CTkEntry(self.form, placeholder_text="Name shown here", font=_font(12))
        self.name_entry.grid(row=0, column=1, sticky="ew", padx=4, pady=(10, 4))
        self.category_menu = ctk.CTkOptionMenu(
            self.form, values=list(CATEGORY_TITLES.values()), width=150, font=_font(12)
        )
        self.category_menu.set(CATEGORY_TITLES["utilities"])
        self.category_menu.grid(row=0, column=2, padx=4, pady=(10, 4))
        self.add_button = _accent(self.form, "Add", self._add, width=70)
        self.add_button.grid(row=0, column=3, padx=(4, 10), pady=(10, 4))
        self.error = _label(self.form, "", 11, theme.WARNING, wraplength=ROW_WRAP)
        self.reset_link = _link(self.form, "Restore the default list", lambda: self._on_save_list(None))
        self.reset_link.grid(row=2, column=0, sticky="w", padx=6, pady=(0, 8))

        self.message = _label(self, "", 12, theme.INK_SECONDARY, wraplength=PANEL_WRAP)
        self.message_check = _neutral(self, "Check again", on_check, width=100)
        self.store_button = _accent(
            self, "Get App Installer", lambda: on_open_uri(APP_INSTALLER_STORE_URI), width=150
        )
        self._init_wrapping(self, [self.summary, self.notice, self.message], 2 * self.PAD + 24, PANEL_WRAP)
        self._refresh_buttons()

    def _body_resized(self, event: tk.Event) -> None:
        if event.width == self._body_width:
            return
        self._body_width = event.width
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        self._tile_width = event.width / scale / 2 - 2 * self.TILE_PADX
        for tile in self.tiles:
            tile.set_tile_width(self._tile_width)

    def _toggle_editing(self) -> None:
        self.set_editing(not self.editing)

    def set_editing(self, editing: bool) -> None:
        self.editing = editing
        set_text(self.edit_button, "Done" if editing else "Edit list")
        _show(self.form, editing, row=1, column=0, sticky="ew", padx=6, pady=(8, 6))
        if not editing:
            self.show_error("")
        self._build_tiles()

    def set_winget(self, status: Mapping[str, Any] | None) -> None:
        availability = (status or {}).get("availability", "ready")
        self._ready = availability == "ready"
        text = {
            "missing": str((status or {}).get("message") or WINGET_MISSING_TEXT),
            "other_user": OTHER_USER_TEXT,
            "user_unknown": USER_UNKNOWN_TEXT,
        }.get(str(availability))
        _show(self.message, text is not None, row=4, column=0, sticky="ew", padx=self.PAD, pady=(0, 6))
        if text is not None:
            set_text(self.message, text)
        missing = availability == "missing"
        _show(self.store_button, missing, row=5, column=0, sticky="w", padx=self.PAD, pady=(0, 6))
        _show(self.message_check, missing, row=6, column=0, sticky="w", padx=self.PAD, pady=(0, 10))
        self._refresh_buttons()

    def set_enabled(self, enabled: bool, job_running: bool) -> None:
        self._enabled = enabled
        self._job_running = job_running
        self._refresh_buttons()

    def _refresh_buttons(self) -> None:
        selected = len(self.selected_items())
        set_text(self.install_button, f"Install selected ({selected})")
        can_act = self._enabled and not self._job_running and self._ready
        _set_state(self.install_button, can_act and selected > 0)
        _set_state(self.edit_button, self._enabled)
        _set_state(self.message_check, self._enabled and not self._job_running)
        for tile in self.tiles:
            tile.set_enabled(not self._job_running)
        for widget in (self.add_button, self.reset_link):
            _set_state(widget, self._enabled and not self._job_running)

    def show_apps(self, app_list: Mapping[str, Any] | None, installed: Iterable[str] | None) -> None:
        """Shows the list with the installed apps marked (`installed` None: not known yet)."""
        try:
            self.apps = [dict(a) for a in (app_list or {}).get("apps") or []]
            self.installed = None if installed is None else [str(i) for i in installed]
            warnings = [str(w) for w in (app_list or {}).get("warnings") or []]
            text = install_summary(self.apps, self.installed)
            if warnings:
                text += "\n" + "\n".join(f"⚠ {w}" for w in warnings)
            set_text(self.summary, text)
            _set_color(self.summary, theme.INK_MUTED)
            self._build_tiles()
        except Exception as exc:  # noqa: BLE001 - malformed engine data is shown, not raised
            log.exception("showing the install list failed")
            self.show_error(f"The list could not be read: {exc}")

    def _is_installed(self, app: Mapping[str, Any]) -> bool:
        ids = {i.lower() for i in self.installed or []}
        return str(app.get("id", "")).lower() in ids

    def _build_tiles(self) -> None:
        chosen = {t.app["id"] for t in self.tiles if t.selected}
        for child in self._groups.winfo_children():
            child.destroy()
        self.tiles = []
        row = 0
        for title, apps in group_apps(self.apps):
            _label(self._groups, title, 12, theme.INK_SECONDARY, weight="bold").grid(
                row=row, column=0, sticky="w", padx=8, pady=(10, 2)
            )
            row += 1
            grid = ctk.CTkFrame(self._groups, fg_color="transparent")
            grid.grid(row=row, column=0, sticky="ew")
            grid.grid_columnconfigure((0, 1), weight=1, uniform="tiles")
            row += 1
            for index, app in enumerate(apps):
                tile = AppTile(
                    grid,
                    app,
                    installed=self._is_installed(app),
                    editing=self.editing,
                    on_toggle=self._refresh_buttons,
                    on_remove=self._remove,
                    width=self._tile_width,
                )
                if app["id"] in chosen and not tile.installed:
                    tile.var.set(1)
                tile.grid(row=index // 2, column=index % 2, sticky="nsew", padx=self.TILE_PADX, pady=3)
                self.tiles.append(tile)
        self._refresh_buttons()

    def selected_items(self) -> list[dict[str, Any]]:
        return [
            {
                "id": str(t.app["id"]),
                "source": str(t.app.get("source") or "winget"),
                "name": str(t.app.get("name")),
            }
            for t in self.tiles
            if t.selected
        ]

    @property
    def selection_count(self) -> int:
        """Ticked tiles, installed or not."""
        return sum(1 for t in self.tiles if t.var.get() == 1)

    def set_items(self, items: Sequence[Mapping[str, Any]]) -> None:
        by_id = {str(i.get("id", "")).lower(): i for i in items}
        for tile in self.tiles:
            item = by_id.get(str(tile.app.get("id", "")).lower())
            if item is not None:
                text, color = item_status(
                    item.get("state"), item.get("message"), "install", item.get("progress")
                )
                tile.set_status(text, color)

    def show_error(self, text: str) -> None:
        set_text(self.error, text)
        _show(self.error, bool(text), row=1, column=0, columnspan=4, sticky="w", padx=10, pady=(0, 4))

    def _entries(self) -> list[dict[str, Any]]:
        return [
            {
                "id": a["id"],
                "name": a.get("name") or a["id"],
                "category": a.get("category"),
                "source": a.get("source", "winget"),
            }
            for a in self.apps
        ]

    def _add(self) -> None:
        package = self.id_entry.get().strip()
        name = self.name_entry.get().strip()
        if not valid_package_id(package):
            self.show_error(INVALID_ID_TEXT)
            return
        existing = next((a for a in self.apps if str(a.get("id", "")).lower() == package.lower()), None)
        if existing is not None:
            self.show_error(DUPLICATE_TEXT.format(name=existing.get("name") or existing["id"]))
            return
        if not name:
            self.show_error(NAME_MISSING_TEXT)
            return
        if len(name) > MAX_NAME_CHARS:
            self.show_error(NAME_TOO_LONG_TEXT)
            return
        category = next(
            (key for key, title in CATEGORY_TITLES.items() if title == self.category_menu.get()), "utilities"
        )
        self.show_error("")
        entries = self._entries()
        entries.append({"id": package, "name": name, "category": category, "source": "winget"})
        self.id_entry.delete(0, "end")
        self.name_entry.delete(0, "end")
        self._on_save_list(entries)

    def _remove(self, app: Mapping[str, Any]) -> None:
        entries = [e for e in self._entries() if str(e["id"]).lower() != str(app.get("id", "")).lower()]
        self._on_save_list(entries)


# -- Windows Update ------------------------------------------------------------------


class SettingRow(ctk.CTkFrame):
    """One Windows Update setting: its title, description, current state, caveat, controls and
    an Undo link while a change of Cairn's is in effect. The controls call `on_set(id, value)`;
    what clearing a setting means (Undo or a change of its own) is the feature's decision."""

    CONTROL_WIDTH = 330
    TEXT_INSET = CONTROL_WIDTH + 2 * 12 + 2 * 6 + WRAP_MARGIN

    def __init__(
        self,
        master: tk.Misc,
        setting: dict[str, Any],
        *,
        edition: Mapping[str, Any],
        on_set: Callable[[str, Any], None],
        on_undo: Callable[[str], None],
        wraplength: int = ROW_WRAP,
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.setting = setting
        self.id = str(setting.get("id"))
        self._on_set = on_set
        self._on_undo = on_undo
        self._enabled = True
        self.grid_columnconfigure(0, weight=1)
        value = setting.get("value") or {}
        self.available = bool(setting.get("available", True))
        outside = not setting.get("by_cairn")

        texts = ctk.CTkFrame(self, fg_color="transparent")
        texts.grid(row=0, column=0, sticky="new", padx=(12, 6), pady=10)
        texts.grid_columnconfigure(0, weight=1)
        self._labels: list[ctk.CTkLabel] = []
        title = str(setting.get("title") or SETTING_TITLES.get(self.id, self.id))
        self._add_label(texts, title, 12, theme.INK, "bold", wraplength)
        self._add_label(
            texts, SETTING_DESCRIPTIONS.get(self.id, ""), 11, theme.INK_SECONDARY, "normal", wraplength
        )
        state, color = self._state_line(value, outside)
        self.state_label = self._add_label(texts, state, 11, color, "normal", wraplength)
        reason = setting.get("unavailable_reason")
        caveat = setting.get("caveat")
        self.reason_label = self._add_label(texts, str(reason or ""), 10, theme.WARNING, "normal", wraplength)
        self.caveat_label = self._add_label(
            texts, str(caveat or ""), 10, theme.INK_MUTED, "normal", wraplength
        )
        self.undo_link: ctk.CTkButton | None = None
        if setting.get("by_cairn") and setting.get("differs"):
            self.undo_link = _link(texts, "Undo", lambda: on_undo(self.id))
            self.undo_link.grid(row=len(texts.grid_slaves()), column=0, sticky="w", pady=(2, 0))

        self.controls = ctk.CTkFrame(self, fg_color="transparent", width=self.CONTROL_WIDTH)
        self.controls.grid(row=0, column=1, sticky="ne", padx=(6, 12), pady=10)
        self.error = _label(self.controls, "", 11, theme.WARNING, wraplength=self.CONTROL_WIDTH - 10)
        self._widgets: list[Any] = []
        builder = getattr(self, f"_build_{self.id}", None)
        if builder is not None:
            builder(value)
        self.set_enabled(True)

    def _add_label(
        self, master: tk.Misc, text: str, size: int, color: str, weight: str, wraplength: int
    ) -> ctk.CTkLabel:
        label = _label(master, text, size, color, weight=weight, wraplength=wraplength)
        if text:
            label.grid(row=len(master.grid_slaves()), column=0, sticky="w")
        self._labels.append(label)
        return label

    def _state_line(self, value: Mapping[str, Any], outside: bool) -> tuple[str, str]:
        kind = value.get("kind")
        if kind == "pause":
            text = pause_text(value, outside=outside)
            return text, theme.WARNING if value.get("paused") else theme.INK_SECONDARY
        if kind == "active_hours":
            return active_hours_text(value, outside=outside), theme.INK_SECONDARY
        if kind == "switch":
            return ("✓ On" if value.get("on") else "○ Off"), theme.INK_SECONDARY
        if kind == "defer":
            days = value.get("days")
            return (f"◐ Delayed by {days} days" if days else "○ No delay"), theme.INK_SECONDARY
        return "– Unknown", theme.INK_MUTED

    def _widget(self, widget: Any, row: int, column: int, **grid: Any) -> Any:
        widget.grid(row=row, column=column, **grid)
        self._widgets.append(widget)
        return widget

    def _build_pause(self, value: Mapping[str, Any]) -> None:
        self.pause_menu = self._widget(
            ctk.CTkOptionMenu(self.controls, values=list(PAUSE_CHOICES), width=110, font=_font(12)),
            0,
            0,
            padx=(0, 6),
        )
        paused = bool(value.get("paused"))
        self.pause_button = self._widget(
            _accent(
                self.controls,
                "Extend" if paused else "Pause",
                lambda: self._on_set("pause", pause_days(self.pause_menu.get())),
                width=80,
            ),
            0,
            1,
        )
        if paused:
            self.resume_button = self._widget(
                _neutral(self.controls, "Resume updates", lambda: self._on_set("pause", None), width=130),
                1,
                0,
                columnspan=2,
                sticky="w",
                pady=(6, 0),
            )

    def _build_active_hours(self, value: Mapping[str, Any]) -> None:
        start = value.get("start")
        end = value.get("end")
        self.start_menu = self._widget(
            ctk.CTkOptionMenu(self.controls, values=list(HOUR_CHOICES), width=90, font=_font(12)), 0, 0
        )
        self.end_menu = self._widget(
            ctk.CTkOptionMenu(self.controls, values=list(HOUR_CHOICES), width=90, font=_font(12)),
            0,
            1,
            padx=6,
        )
        self.start_menu.set(HOUR_CHOICES[int(start) % 24] if start is not None else HOUR_CHOICES[8])
        self.end_menu.set(HOUR_CHOICES[int(end) % 24] if end is not None else HOUR_CHOICES[17])
        self.set_button = self._widget(_accent(self.controls, "Set", self._set_hours, width=60), 0, 2)
        self.auto_button = self._widget(
            _neutral(self.controls, "Use automatic", lambda: self._on_set("active_hours", None), width=120),
            1,
            0,
            columnspan=3,
            sticky="w",
            pady=(6, 0),
        )

    def _set_hours(self) -> None:
        start = HOUR_CHOICES.index(self.start_menu.get())
        end = HOUR_CHOICES.index(self.end_menu.get())
        problem = active_hours_error(start, end)
        self.show_error(problem or "")
        if problem is None:
            self._on_set("active_hours", [start, end])

    def _build_switch(self, value: Mapping[str, Any]) -> None:
        self.switch = self._widget(
            ctk.CTkSwitch(self.controls, text="", width=50, command=self._switched), 0, 0, sticky="e"
        )
        if value.get("on"):
            self.switch.select()
        else:
            self.switch.deselect()

    def _build_exclude_drivers(self, value: Mapping[str, Any]) -> None:
        self._build_switch(value)

    def _build_restart_notify(self, value: Mapping[str, Any]) -> None:
        self._build_switch(value)

    def _switched(self) -> None:
        self._on_set(self.id, bool(self.switch.get()))

    def _build_defer_feature(self, value: Mapping[str, Any]) -> None:
        self.defer_menu = self._widget(
            ctk.CTkOptionMenu(self.controls, values=list(DEFER_CHOICES), width=130, font=_font(12)),
            0,
            0,
            padx=(0, 6),
        )
        self.defer_menu.set(defer_choice(value.get("days")))
        self.defer_button = self._widget(
            _accent(
                self.controls,
                "Set",
                lambda: self._on_set("defer_feature", defer_days(self.defer_menu.get())),
                width=60,
            ),
            0,
            1,
        )

    def show_error(self, text: str) -> None:
        set_text(self.error, text)
        _show(self.error, bool(text), row=2, column=0, columnspan=3, sticky="w", pady=(4, 0))

    def set_enabled(self, enabled: bool) -> None:
        self._enabled = enabled
        for widget in self._widgets:
            _set_state(widget, enabled and self.available)
        if self.undo_link is not None:
            _set_state(self.undo_link, enabled)

    def set_wraplength(self, wrap: int) -> None:
        for label in self._labels:
            if label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)

    @property
    def state_text(self) -> str:
        return str(self.state_label.cget("text"))


class WindowsUpdateView(ctk.CTkScrollableFrame):
    """The Windows Update card: the edition and service state, organization notes, one row per
    setting and links to Windows' own pages. Scrolls as a whole."""

    PAD = 14

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_set: Callable[[str, Any], None],
        on_undo: Callable[[str], None],
        on_open_uri: Callable[[str], None],
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_set = on_set
        self._on_undo = on_undo
        self._on_open_uri = on_open_uri
        self.grid_columnconfigure(0, weight=1)
        self.rows: dict[str, SettingRow] = {}
        self.state: dict[str, Any] | None = None
        self._enabled = True
        self._row_wrap = ROW_WRAP
        self._width = -1
        self._block_labels: list[ctk.CTkLabel] = []

        _label(self, "Windows Update", 13, theme.INK_SECONDARY, weight="bold").grid(
            row=0, column=0, sticky="w", padx=self.PAD, pady=(10, 0)
        )
        self.status_block = ctk.CTkFrame(self, fg_color="transparent")
        self.status_block.grid(row=1, column=0, sticky="ew", padx=self.PAD, pady=(4, 6))
        self.status_block.grid_columnconfigure(0, weight=1)
        self.body = ctk.CTkFrame(self, fg_color="transparent")
        self.body.grid(row=2, column=0, sticky="ew", padx=6)
        self.body.grid_columnconfigure(0, weight=1)
        footer = ctk.CTkFrame(self, fg_color="transparent")
        footer.grid(row=3, column=0, sticky="ew", padx=self.PAD, pady=(8, 12))
        footer.grid_columnconfigure(0, weight=1)
        self.footer_label = _label(footer, WU_FOOTER_TEXT, 10, theme.INK_MUTED, wraplength=PANEL_WRAP)
        self.footer_label.grid(row=0, column=0, columnspan=2, sticky="w")
        _link(footer, "Open Windows Update settings", lambda: on_open_uri(WU_SETTINGS_URI)).grid(
            row=1, column=0, sticky="w", pady=(4, 0)
        )
        _link(footer, "Advanced options", lambda: on_open_uri(WU_OPTIONS_URI)).grid(
            row=1, column=1, sticky="w", padx=(8, 0), pady=(4, 0)
        )
        self.bind("<Configure>", self._resized, add="+")
        self.set_loading()

    def _resized(self, event: tk.Event) -> None:
        if event.width == self._width:
            return
        self._width = event.width
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        width = event.width / scale
        self._row_wrap = fitted_wrap(width - 12 - SettingRow.TEXT_INSET, ROW_WRAP)
        for row in self.rows.values():
            row.set_wraplength(self._row_wrap)
        wrap = fitted_wrap(width - 2 * self.PAD - WRAP_MARGIN, PANEL_WRAP)
        for label in [*self._block_labels, self.footer_label]:
            if label.winfo_exists() and label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)

    def _block(self, lines: Sequence[tuple[str, str, int]]) -> None:
        for child in self.status_block.winfo_children():
            child.destroy()
        self._block_labels = []
        for index, (text, color, size) in enumerate(lines):
            label = _label(self.status_block, text, size, color, wraplength=self._block_wrap())
            label.grid(row=index, column=0, sticky="w")
            self._block_labels.append(label)

    def _clear_rows(self) -> None:
        for row in self.rows.values():
            row.destroy()
        self.rows = {}

    def set_loading(self) -> None:
        if self.state is None:
            self._block([(WU_LOADING_TEXT, theme.INK_MUTED, 11)])

    def show_error(self, message: str) -> None:
        self.state = None
        self._clear_rows()
        self._block([(WU_ERROR_TEXT.format(message=message), theme.WARNING, 11)])

    def show_state(self, state: Mapping[str, Any]) -> None:
        try:
            self._show_state(state)
        except Exception as exc:  # noqa: BLE001 - malformed engine data is shown, not raised
            log.exception("showing the Windows Update state failed")
            self.show_error(str(exc))

    def _show_state(self, state: Mapping[str, Any]) -> None:
        self.state = dict(state)
        edition = dict(state.get("edition") or {})
        lines: list[tuple[str, str, int]] = [(edition_text(edition) or "Windows", theme.INK, 12)]
        if state.get("restart_pending"):
            lines.append((RESTART_PENDING_TEXT, theme.WARNING, 11))
        if state.get("service") == "disabled":
            lines.append((SERVICE_DISABLED_TEXT, theme.WARNING, 11))
        for note in list(state.get("managed") or []) + list(state.get("warnings") or []):
            lines.append((f"⚠ {note}", theme.WARNING, 11))
        self._block(lines)
        self._clear_rows()
        for index, setting in enumerate(state.get("settings") or []):
            row = SettingRow(
                self.body,
                dict(setting),
                edition=edition,
                on_set=self._on_set,
                on_undo=self._on_undo,
                wraplength=self._row_wrap,
            )
            row.grid(row=index, column=0, sticky="ew", padx=6, pady=3)
            row.set_enabled(self._enabled)
            self.rows[row.id] = row
        # A resize can arrive while a row is being built, before it is listed.
        for row in self.rows.values():
            row.set_wraplength(self._row_wrap)
        for label in self._block_labels:
            label.configure(wraplength=self._block_wrap())

    def _block_wrap(self) -> int:
        if self._width <= 0:
            return PANEL_WRAP
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        return fitted_wrap(self._width / scale - 2 * self.PAD - WRAP_MARGIN, PANEL_WRAP)

    def setting(self, setting_id: str) -> dict[str, Any] | None:
        for setting in (self.state or {}).get("settings") or []:
            if setting.get("id") == setting_id:
                return dict(setting)
        return None

    def refresh(self) -> None:
        """Draws the last state again, putting controls back where the system is."""
        if self.state is not None:
            self.show_state(self.state)

    def set_enabled(self, enabled: bool) -> None:
        self._enabled = enabled
        for row in self.rows.values():
            row.set_enabled(enabled)


# -- the panel -----------------------------------------------------------------------


class UpdatesPanel(ctk.CTkFrame):
    """The Updates section: the view switch, the three views and the job strip."""

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_view: Callable[[str], None],
        on_check: Callable[[], None],
        on_update: Callable[[bool], None],
        on_stop: Callable[[], None],
        on_output: Callable[[bool], None],
        on_open_log: Callable[[], None],
        on_install: Callable[[], None],
        on_save_list: Callable[[list[dict[str, Any]] | None], None],
        on_wu_set: Callable[[str, Any], None],
        on_wu_undo: Callable[[str], None],
        on_open_uri: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_view = on_view
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(1, weight=1)
        self.view = "apps"
        self.unsupported = False
        self.engine_ready = True
        self.elevated = False

        self.switch = ctk.CTkSegmentedButton(
            self, values=list(VIEWS.values()), command=self._switched, font=_font(12)
        )
        self.switch.set(VIEWS["apps"])
        self.switch.grid(row=0, column=0, sticky="w", pady=(0, 6))
        self.host = ctk.CTkFrame(self, fg_color="transparent")
        self.host.grid(row=1, column=0, sticky="nsew")
        self.host.grid_columnconfigure(0, weight=1)
        self.host.grid_rowconfigure(0, weight=1)
        self.apps = AppUpdatesView(self.host, on_check=on_check, on_update=on_update, on_open_uri=on_open_uri)
        self.install = InstallAppsView(
            self.host,
            on_install=on_install,
            on_save_list=on_save_list,
            on_check=on_check,
            on_open_uri=on_open_uri,
        )
        self.windows = WindowsUpdateView(
            self.host, on_set=on_wu_set, on_undo=on_wu_undo, on_open_uri=on_open_uri
        )
        self.job = JobStrip(self, on_stop=on_stop, on_output=on_output, on_open_log=on_open_log)
        self.unsupported_label = _label(self, "", 12, theme.INK_MUTED, wraplength=PANEL_WRAP)
        self._views: dict[str, Any] = {"apps": self.apps, "install": self.install, "windows": self.windows}
        self.show_view("apps")

    def _switched(self, title: str) -> None:
        key = next((k for k, v in VIEWS.items() if v == title), "apps")
        self.show_view(key)
        self._on_view(key)

    def show_view(self, key: str) -> None:
        self.view = key if key in self._views else "apps"
        if self.switch.get() != VIEWS[self.view]:
            self.switch.set(VIEWS[self.view])
        if self.unsupported:
            return
        _show(self._views[self.view], True, row=0, column=0, sticky="nsew")
        for name, widget in self._views.items():
            if name != self.view:
                _show(widget, False)

    def set_unsupported(self, text: str) -> None:
        """Shows only `text`: the engine lacks this section's functions."""
        self.unsupported = True
        for widget in (self.switch, self.host, self.job):
            _show(widget, False)
        set_text(self.unsupported_label, text)
        self.unsupported_label.grid(row=0, column=0, sticky="nw", padx=10, pady=10)

    def set_winget_status(self, status: Mapping[str, Any] | None) -> None:
        self.apps.set_winget(status)
        self.install.set_winget(status)

    def set_access(self, engine_ready: bool, elevated: bool) -> None:
        self.engine_ready = engine_ready
        self.elevated = elevated

    def set_actions_enabled(self, enabled: bool, job_running: bool = False) -> None:
        enabled = enabled and self.engine_ready
        self.apps.set_enabled(enabled, job_running)
        self.install.set_enabled(enabled, job_running)
        self.windows.set_enabled(enabled)

    def show_job(self, visible: bool) -> None:
        if self.unsupported:
            return
        _show(self.job, visible, row=2, column=0, sticky="ew", pady=(6, 0))
