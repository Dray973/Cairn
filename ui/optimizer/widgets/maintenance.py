"""Scheduled maintenance view: the schedule card, what a run does, and the last run.

The pure helpers at the top turn the engine's status, plans and runs into texts; the panel
lays them out. The panel never calls the engine: the owner passes callbacks and feeds it with
`show`, `show_progress`, `set_busy` and `set_idle`. Conditional rows are removed with
`grid_forget` and wrapped texts follow the width their card gets.
"""

from __future__ import annotations

import tkinter as tk
from collections.abc import Callable, Mapping, Sequence
from datetime import datetime
from typing import Any

import customtkinter as ctk

from .. import theme
from .cleanup import fmt_size
from .controls import MIN_WRAP
from .history import task_display_name
from .monitor import set_text

INTRO_TEXT = (
    "Cairn cleans up and checks Windows once a week on its own. It runs only while you're "
    "signed in, the PC is plugged in and it has been idle for a few minutes, and it never wakes the PC. "
    "If the PC is off or asleep at that time, it runs at the next chance."
)
READ_ONLY_TEXT = (
    "The checks only report. Maintenance never repairs anything: if a check finds a problem, "
    "Cairn tells you here and you decide what to do in Tools."
)
IRREVERSIBLE_TEXT = "⚠ Cleaned files are deleted permanently. The Recycle Bin is never emptied on a schedule."
UNRECORDED_TEXT = (
    "A maintenance task for your account exists in Task Scheduler, but Cairn's journal has no "
    "record of creating it (the journal may have been reset). Cairn doesn't change a task it can't undo."
)
ON_BATTERY_TEXT = "The PC is running on battery power. Maintenance runs only when it's plugged in."
TURN_ON_FIRST_TEXT = "Turn on scheduled maintenance to run it now. For a one-time cleanup, use Cleanup."
BATTERY_NOTE = "This PC has a battery: maintenance waits until it's plugged in."
NEEDS_ADMIN_TEXT = "Turning maintenance on, saving it and Run now need administrator rights."
NO_RUNS_TEXT = "– Maintenance hasn't run yet."
LOADING_TEXT = "◐ Loading…"
STARTING_TEXT = "◐ Starting…"
SFC_TITLE = "Check system files"
SFC_DESCRIPTION = (
    "System File Checker (sfc /verifyonly) reports damaged Windows files without changing anything"
    "  ·  usually 10–30 minutes"
)
DISM_TITLE = "Check the component store"
DISM_DESCRIPTION = (
    "DISM CheckHealth reports whether Windows' repair source is marked as damaged  ·  under a minute"
)
PER_USER_NOTE = "your account's files"
SERVICING_NOTE = "skipped while Windows Update is working"

DAY_VALUES = ("monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday")
DAY_LABELS = tuple(day.capitalize() for day in DAY_VALUES)
HOUR_LABELS = tuple(f"{(hour % 12) or 12} {'AM' if hour < 12 else 'PM'}" for hour in range(24))
MINUTE_LABELS = (":00", ":15", ":30", ":45")
# Runs listed under "Earlier runs".
EARLIER_RUNS = 5
# Step lines shown for the last run.
STEP_LINES = 3

# (icon, colour) of a step outcome.
OUTCOME_STYLES: dict[str, tuple[str, str]] = {
    "ok": ("✓", theme.GOOD),
    "attention": ("⚠", theme.WARNING),
    "failed": ("⚠", theme.CRITICAL),
    "skipped": ("○", theme.INK_SECONDARY),
    "not_run": ("–", theme.INK_MUTED),
    "left_running": ("◐", theme.INK_SECONDARY),
    "unknown": ("○", theme.INK_SECONDARY),
}
# (icon, colour) of a finished run's state.
STATE_STYLES: dict[str, tuple[str, str]] = {
    "completed": ("✓", theme.GOOD),
    "attention": ("⚠", theme.WARNING),
    "failed": ("⚠", theme.CRITICAL),
    "stopped": ("○", theme.INK_SECONDARY),
    "skipped": ("○", theme.INK_SECONDARY),
    "interrupted": ("○", theme.INK_SECONDARY),
}

# -- pure helpers --------------------------------------------------------------------


def _parse_time(text: str) -> tuple[int, int] | None:
    hour, sep, minute = str(text).partition(":")
    if not sep or not hour.isdigit() or not minute.isdigit() or len(minute) != 2:
        return None
    h, m = int(hour), int(minute)
    return (h, m) if 0 <= h < 24 and 0 <= m < 60 else None


def menus_to_config_time(hour: str, minute: str) -> str:
    """The "HH:MM" of the hour and minute menus: ("1 PM", ":30") -> "13:30"."""
    h = HOUR_LABELS.index(hour)
    m = int(minute.lstrip(":"))
    return f"{h:02d}:{m:02d}"


def config_time_to_menus(time: str) -> tuple[str, str]:
    """The hour and minute menu values of "HH:MM"; a minute off the quarter hours keeps its
    own value. Malformed text gives 12 PM, :00."""
    parsed = _parse_time(time)
    if parsed is None:
        return HOUR_LABELS[12], MINUTE_LABELS[0]
    return HOUR_LABELS[parsed[0]], f":{parsed[1]:02d}"


def time_text(time: str) -> str:
    """ "13:30" -> "1:30 PM"; malformed text as it is."""
    parsed = _parse_time(time)
    if parsed is None:
        return str(time)
    h, m = parsed
    return f"{(h % 12) or 12}:{m:02d} {'AM' if h < 12 else 'PM'}"


def day_label(value: str) -> str:
    """ "sunday" -> "Sunday"."""
    return str(value).capitalize()


def schedule_text(config: Mapping[str, Any]) -> str:
    """ "every Sunday at 12:00 PM"."""
    return f"every {day_label(config.get('day', ''))} at {time_text(config.get('time', ''))}"


def local_time(stamp: str | None) -> datetime | None:
    """A journal time (RFC 3339, UTC) or a Task Scheduler time (local, no offset) as a naive
    local time."""
    if not stamp:
        return None
    try:
        parsed = datetime.fromisoformat(str(stamp).replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is not None:
        parsed = parsed.astimezone().replace(tzinfo=None)
    return parsed


def _clock(moment: datetime) -> str:
    return f"{(moment.hour % 12) or 12}:{moment.minute:02d} {'AM' if moment.hour < 12 else 'PM'}"


def short_date(stamp: str | None) -> str:
    """ "Sun 20 Sep"; "" when unknown."""
    moment = local_time(stamp)
    return "" if moment is None else f"{moment:%a} {moment.day} {moment:%b}"


def local_when(stamp: str | None) -> str:
    """ "Sun 27 Sep, 12:14 PM"; "at an unknown time" when unknown."""
    moment = local_time(stamp)
    if moment is None:
        return "at an unknown time"
    return f"{short_date(stamp)}, {_clock(moment)}"


def clock_text(stamp: str | None) -> str:
    """ "12:03 PM"; "" when unknown."""
    moment = local_time(stamp)
    return "" if moment is None else _clock(moment)


def duration_text(ms: Any) -> str:
    """ "45 s", "18 min", "1 h 5 min"; "" when unknown."""
    if not isinstance(ms, int | float) or ms < 0:
        return ""
    seconds = int(ms // 1000)
    if seconds < 60:
        return f"{seconds} s"
    minutes = seconds // 60
    if minutes < 60:
        return f"{minutes} min"
    return f"{minutes // 60} h {minutes % 60} min"


def run_state(run: Mapping[str, Any]) -> str:
    """The run's state; a stale running row (no run holds the lock) counts as interrupted."""
    return "interrupted" if run.get("stale") else str(run.get("state", ""))


def run_state_line(run: Mapping[str, Any]) -> tuple[str, str]:
    """The state line of a run and its colour."""
    state = run_state(run)
    if state == "running":
        progress = run.get("progress") or {}
        step = ""
        if progress.get("index") and progress.get("count"):
            step = f"  ·  step {progress['index']} of {progress['count']}"
        return f"◐ Running since {clock_text(run.get('started_at'))}{step}", theme.ACCENT
    report = run.get("report") or {}
    when = local_when(run.get("ended_at") or run.get("started_at"))
    duration = duration_text(report.get("duration_ms"))
    suffix = f"  ·  {duration}" if duration else ""
    icon, color = STATE_STYLES.get(state, ("–", theme.INK_MUTED))
    texts = {
        "completed": f"Finished {when}{suffix}",
        "attention": f"Finished {when}{suffix}",
        "failed": f"Failed {when}{suffix}",
        "stopped": f"Stopped early {when}",
        "skipped": f"Skipped {when}",
        "interrupted": f"Didn't finish (started {local_when(run.get('started_at'))})",
    }
    return f"{icon} {texts.get(state, state or 'Unknown')}", color


def step_lines(report: Mapping[str, Any] | None) -> list[tuple[str, str]]:
    """Up to three lines, one per step of a finished run, with their colours."""
    if not report:
        return []
    lines: list[tuple[str, str]] = []
    cleanup = report.get("cleanup")
    if cleanup:
        outcome = str(cleanup.get("outcome", ""))
        icon, color = OUTCOME_STYLES.get(outcome, ("–", theme.INK_MUTED))
        if outcome in ("ok", "attention"):
            text = f"Clean up: freed {fmt_size(int(cleanup.get('freed_bytes') or 0))}"
        elif outcome == "failed":
            text = f"Clean up: failed ({cleanup.get('error') or 'see the log'})"
        elif outcome == "not_run":
            text = "Clean up: not run"
        else:
            text = "Clean up: skipped"
        lines.append((f"{icon} {text}", color))
    for check in report.get("checks") or []:
        icon, color = OUTCOME_STYLES.get(str(check.get("outcome", "")), ("–", theme.INK_MUTED))
        lines.append((f"{icon} {check.get('title', '')}: {check.get('text', '')}", color))
    return lines[:STEP_LINES]


def attention_hints(report: Mapping[str, Any] | None) -> list[str]:
    """The run's things to look at."""
    return [str(a) for a in (report or {}).get("attention") or []]


def names_tools(report: Mapping[str, Any] | None) -> bool:
    """Whether a hint of the run points to a repair in Tools."""
    texts = attention_hints(report) + [str(c.get("hint") or "") for c in (report or {}).get("checks") or []]
    return any("Tools" in text for text in texts)


def notice_for(run: Mapping[str, Any]) -> tuple[str, str] | None:
    """The status bar message about a finished run, or None for a running one."""
    state = run_state(run)
    report = run.get("report") or {}
    when = local_when(run.get("ended_at") or run.get("started_at"))
    attention = attention_hints(report)
    if state == "completed":
        return f"✓ Maintenance ran {when}: {report.get('headline') or 'finished'}.", theme.GOOD
    if state == "attention":
        first = attention[0] if attention else "see Maintenance"
        return f"⚠ Maintenance ran {when} and found something to look at: {first}", theme.WARNING
    if state == "failed":
        first = attention[0] if attention else "a step failed"
        return f"⚠ Maintenance ran {when}, but {first}", theme.WARNING
    if state == "stopped":
        reason = report.get("stopped_reason") or "it stopped before the end"
        return f"○ Maintenance stopped early {when}: {reason}", theme.INK_SECONDARY
    if state == "skipped":
        reason = report.get("stopped_reason") or "nothing ran"
        return f"○ Maintenance was skipped {when}: {reason}", theme.INK_SECONDARY
    if state == "interrupted":
        return (
            f"○ The maintenance run of {local_when(run.get('started_at'))} didn't finish "
            "(the PC shut down or you signed out).",
            theme.INK_SECONDARY,
        )
    return None


def needs_attention(run: Mapping[str, Any]) -> bool:
    """A finished run the sidebar badge points to: it found something or failed."""
    return run_state(run) in ("attention", "failed")


def _normalized(config: Mapping[str, Any]) -> tuple[Any, ...]:
    return (
        str(config.get("day", "")),
        str(config.get("time", "")),
        tuple(config.get("targets") or ()),
        bool(config.get("system_file_check")),
        bool(config.get("component_store_check")),
    )


def config_differs(a: Mapping[str, Any] | None, b: Mapping[str, Any] | None) -> bool:
    """Whether two schedules differ in any field."""
    if a is None or b is None:
        return a is not b
    return _normalized(a) != _normalized(b)


def plan_details(plan: Mapping[str, Any], targets: Sequence[Mapping[str, Any]]) -> list[str]:
    """Detail lines of the Turn on / Save dialog: when, as whom, what it checks and every
    location whose files it deletes."""
    titles = {t.get("id"): t.get("title") for t in targets}
    config = plan.get("config") or {}
    lines = [
        f"• Runs {schedule_text(config)}; next run {local_when(plan.get('next_run'))}",
        f"• Runs as {plan.get('account', 'your account')} with administrator rights, only while the PC is "
        "idle and plugged in",
    ]
    checks = []
    if config.get("system_file_check"):
        checks.append("System File Checker (sfc /verifyonly)")
    if config.get("component_store_check"):
        checks.append("DISM CheckHealth")
    lines.append("• Checks, read-only: " + (", ".join(checks) if checks else "none"))
    chosen = list(config.get("targets") or [])
    if chosen:
        lines.append("• Each run permanently deletes the files in:")
        lines += [f"      {titles.get(t) or t}" for t in chosen]
    else:
        lines.append("• Cleans nothing")
    lines += [f"• {note}" for note in plan.get("notes") or []]
    lines.append(f"• Task Scheduler: {task_display_name(str(plan.get('task_path', '')))}")
    return lines


def state_label(status: Mapping[str, Any]) -> tuple[str, str]:
    """The schedule's state and its colour."""
    if status.get("running"):
        return "◐ Running…", theme.ACCENT
    task = status.get("task")
    if not task:
        return "○ Off", theme.INK_MUTED
    if not status.get("recorded"):
        return "⚠ Not recorded by Cairn", theme.WARNING
    if not task.get("enabled"):
        return "⚠ Disabled in Task Scheduler", theme.WARNING
    if task.get("drift") or not task.get("config"):
        return "⚠ Changed outside Cairn", theme.WARNING
    return f"✓ On · {schedule_text(task['config'])}", theme.GOOD


def next_run_text(status: Mapping[str, Any]) -> str:
    """ "Next run: Sun 4 Oct, 12:00 PM  ·  waits until the PC is idle and plugged in"; "" while
    off or disabled."""
    task = status.get("task")
    if not task or not status.get("recorded") or not task.get("enabled") or not task.get("next_run_time"):
        return ""
    return f"Next run: {local_when(task['next_run_time'])}  ·  waits until the PC is idle and plugged in"


def effective_config(status: Mapping[str, Any]) -> dict[str, Any]:
    """The schedule the controls show: the recorded task's, else the defaults."""
    task = status.get("task")
    if task and status.get("recorded") and task.get("config"):
        return dict(task["config"])
    return dict(status.get("defaults") or {})


def run_now_block(status: Mapping[str, Any]) -> str | None:
    """Why Run now is not available, or None."""
    task = status.get("task")
    if not task or not status.get("recorded") or not task.get("enabled"):
        return TURN_ON_FIRST_TEXT
    if status.get("running"):
        return "Maintenance is running."
    if status.get("on_battery"):
        return ON_BATTERY_TEXT
    return status.get("blocked_reason") or None


def earlier_lines(runs: Sequence[Mapping[str, Any]]) -> list[str]:
    """ "Sun 20 Sep  ·  ✓ Freed 800.2 MB" for the runs after the latest one."""
    lines = []
    for run in runs[1 : 1 + EARLIER_RUNS]:
        icon, _ = STATE_STYLES.get(run_state(run), ("◐", theme.ACCENT))
        headline = (run.get("report") or {}).get("headline") or run_state(run)
        lines.append(f"{short_date(run.get('started_at'))}  ·  {icon} {headline}")
    return lines


def status_bar_text(observation: Mapping[str, Any] | None) -> str:
    """The status bar text of the maintenance lane: the running step, "" when idle."""
    if not observation:
        return ""
    if observation.get("running"):
        run = observation.get("run") or {}
        title = (run.get("progress") or {}).get("title") or "Running…"
        return f"◐ Maintenance: {title}"
    if observation.get("waiting_for_start"):
        return "◐ Maintenance: Starting…"
    return ""


def target_description(target: Mapping[str, Any]) -> str:
    """A target's catalog description with its scheduling notes."""
    text = str(target.get("description") or "")
    if target.get("per_user"):
        text += f"  ·  {PER_USER_NOTE}"
    if target.get("servicing_guard"):
        text += f"  ·  {SERVICING_NOTE}"
    return text


# -- widgets -------------------------------------------------------------------------

# Space the schedule card's texts leave: the card's padding on both sides.
CARD_TEXT_PAD = 2 * 14
# The Remove task button beside the unrecorded text.
REMOVE_COLUMN_WIDTH = 110 + 2 * 10
# Space the rows of the scrollable cards leave for their texts.
BODY_TEXT_PAD = 2 * 10 + 30
# Padding of the descriptions under the "What it does" check boxes, on both sides; the card's
# other texts have 6 px.
DESCRIPTION_PADX = 34


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def _description_wrap(wrap: int) -> int:
    """Wrap of a check box's description when the card's other texts wrap at `wrap`."""
    return max(MIN_WRAP // 2, wrap - 2 * (DESCRIPTION_PADX - 6))


def _set_state(widget: ctk.CTkBaseClass, enabled: bool) -> None:
    state = "normal" if enabled else "disabled"
    if widget.cget("state") != state:
        widget.configure(state=state)


def _set_color(label: ctk.CTkLabel, color: str) -> None:
    if label.cget("text_color") != color:
        label.configure(text_color=color)


def _button(
    master: tk.Misc, text: str, command: Callable[[], None], *, width: int, **kw: Any
) -> ctk.CTkButton:
    options: dict[str, Any] = {
        "height": 30,
        "font": _font(12),
        "fg_color": theme.BUTTON_NEUTRAL,
        "hover_color": theme.BUTTON_NEUTRAL_HOVER,
        "text_color": theme.INK,
    }
    options.update(kw)
    return ctk.CTkButton(master, text=text, command=command, width=width, **options)


def _menu(master: tk.Misc, values: Sequence[str], width: int) -> ctk.CTkOptionMenu:
    return ctk.CTkOptionMenu(
        master,
        values=list(values),
        width=width,
        height=28,
        dynamic_resizing=False,
        font=_font(12),
        fg_color=theme.BUTTON_NEUTRAL,
        button_color=theme.BASELINE,
        button_hover_color=theme.INK_MUTED,
        text_color=theme.INK,
        dropdown_fg_color=theme.SURFACE_RAISED,
        dropdown_hover_color=theme.BUTTON_NEUTRAL_HOVER,
        dropdown_text_color=theme.INK,
        dropdown_font=_font(11),
    )


def _label(master: tk.Misc, text: str, size: int, color: str, weight: str = "normal") -> ctk.CTkLabel:
    return ctk.CTkLabel(
        master,
        text=text,
        font=_font(size, weight),
        text_color=color,
        anchor="w",
        justify="left",
        wraplength=MIN_WRAP,
    )


def _show(widget: tk.Misc, shown: bool, **grid: Any) -> None:
    """Grids `widget` with `grid` when `shown`, else removes it."""
    if shown and not widget.winfo_manager():
        widget.grid(**grid)
    elif not shown and widget.winfo_manager():
        widget.grid_forget()


def _scroll_card(master: tk.Misc) -> ctk.CTkScrollableFrame:
    return ctk.CTkScrollableFrame(
        master,
        fg_color=theme.SURFACE,
        corner_radius=0,
        scrollbar_button_color=theme.BASELINE,
        scrollbar_button_hover_color=theme.INK_MUTED,
    )


class MaintenancePanel(ctk.CTkFrame):
    """The Scheduled maintenance section: the schedule card across the top, "What it does"
    (the checks and cleanup locations a run uses) and "Last run" below it.

    `show(status, …)` fills everything from `maintenance_status()` and never raises;
    malformed data is shown as an error. The controls keep the user's edits across reloads
    until the schedule they show changes.
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_turn_on: Callable[[], None],
        on_turn_off: Callable[[], None],
        on_run_now: Callable[[], None],
        on_remove_task: Callable[[], None],
        on_open_log: Callable[[int], None],
        on_open_tools: Callable[[], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_open_log = on_open_log
        self.grid_columnconfigure(0, weight=3, uniform="maintenance")
        self.grid_columnconfigure(1, weight=2, uniform="maintenance")
        self.grid_rowconfigure(1, weight=1)

        self.loaded = False
        self.loading = False
        self.status: dict[str, Any] | None = None
        self._engine_ready = True
        self._elevated = False
        self._unsupported = False
        self._error: str | None = None
        self._actions_enabled = True
        self._busy_text: str | None = None
        self._progress: dict[str, Any] | None = None
        self._shown_config: dict[str, Any] | None = None
        self._target_ids: list[str] = []
        self.target_vars: dict[str, tk.BooleanVar] = {}
        self.target_rows: dict[str, ctk.CTkCheckBox] = {}
        self._target_labels: list[ctk.CTkLabel] = []
        self._log_run: int | None = None
        self._wraps: dict[str, int] = {}

        self._build_schedule_card(on_turn_on, on_turn_off, on_run_now, on_remove_task)
        self._build_what_card()
        self._build_last_run_card(on_open_tools)
        self._apply_state()

    # -- layout ------------------------------------------------------------------------

    def _build_schedule_card(
        self,
        on_turn_on: Callable[[], None],
        on_turn_off: Callable[[], None],
        on_run_now: Callable[[], None],
        on_remove_task: Callable[[], None],
    ) -> None:
        card = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        card.grid(row=0, column=0, columnspan=2, sticky="ew", pady=(0, 10))
        card.grid_columnconfigure(0, weight=1)
        self.schedule_card = card

        header = ctk.CTkFrame(card, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(2, weight=1)
        ctk.CTkLabel(
            header, text="Scheduled maintenance", font=_font(13, "bold"), text_color=theme.INK_SECONDARY
        ).grid(row=0, column=0, sticky="w")
        self.state_label = ctk.CTkLabel(
            header, text=LOADING_TEXT, font=_font(12, "bold"), text_color=theme.INK_MUTED, anchor="w"
        )
        self.state_label.grid(row=0, column=1, sticky="w", padx=(12, 0))
        self.run_now_button = _button(header, "Run now", on_run_now, width=100)
        self.run_now_button.grid(row=0, column=3, sticky="e", padx=(6, 0))
        self.turn_off_button = _button(header, "Turn off", on_turn_off, width=100)
        self.primary_button = _button(
            header,
            "Turn on",
            on_turn_on,
            width=120,
            font=_font(12, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
        )
        self.primary_button.grid(row=0, column=5, sticky="e", padx=(6, 0))

        self.intro_label = _label(card, INTRO_TEXT, 11, theme.INK_MUTED)
        self.intro_label.grid(row=1, column=0, sticky="w", padx=14, pady=(4, 0))

        controls = ctk.CTkFrame(card, fg_color="transparent")
        controls.grid(row=2, column=0, sticky="w", padx=14, pady=(8, 0))
        ctk.CTkLabel(controls, text="Every", font=_font(12), text_color=theme.INK_SECONDARY).grid(
            row=0, column=0, padx=(0, 6)
        )
        self.day_menu = _menu(controls, DAY_LABELS, 120)
        self.day_menu.grid(row=0, column=1)
        ctk.CTkLabel(controls, text="at", font=_font(12), text_color=theme.INK_SECONDARY).grid(
            row=0, column=2, padx=6
        )
        self.hour_menu = _menu(controls, HOUR_LABELS, 90)
        self.hour_menu.grid(row=0, column=3)
        self.minute_menu = _menu(controls, MINUTE_LABELS, 70)
        self.minute_menu.grid(row=0, column=4, padx=(4, 0))
        self.day_menu.set("Sunday")
        self.hour_menu.set(HOUR_LABELS[12])
        self.minute_menu.set(MINUTE_LABELS[0])

        self.next_label = _label(card, "", 11, theme.INK_SECONDARY)
        self.hint_label = _label(card, "", 10, theme.INK_MUTED)
        self.warnings_label = _label(card, "", 10, theme.WARNING)
        self.unrecorded_box = ctk.CTkFrame(card, fg_color="transparent")
        self.unrecorded_box.grid_columnconfigure(0, weight=1)
        self.unrecorded_label = _label(self.unrecorded_box, f"⚠ {UNRECORDED_TEXT}", 10, theme.WARNING)
        self.unrecorded_label.grid(row=0, column=0, sticky="w")
        self.remove_button = _button(
            self.unrecorded_box,
            "Remove task",
            on_remove_task,
            width=110,
            fg_color=theme.CRITICAL,
            hover_color=theme.CRITICAL_HOVER,
        )
        self.remove_button.grid(row=0, column=1, sticky="e", padx=(10, 0))
        # Bottom padding of the card whatever rows it shows.
        ctk.CTkFrame(card, fg_color="transparent", height=10).grid(row=7, column=0)
        card.bind("<Configure>", self._fit_schedule_texts, add="+")

    def _build_what_card(self) -> None:
        card = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        card.grid(row=1, column=0, sticky="nsew", padx=(0, 5))
        card.grid_columnconfigure(0, weight=1)
        card.grid_rowconfigure(1, weight=1)
        ctk.CTkLabel(card, text="What it does", font=_font(13, "bold"), text_color=theme.INK_SECONDARY).grid(
            row=0, column=0, sticky="w", padx=14, pady=(10, 2)
        )
        body = _scroll_card(card)
        body.grid(row=1, column=0, sticky="nsew", padx=4, pady=(0, 6))
        body.grid_columnconfigure(0, weight=1)
        self.what_body = body

        ctk.CTkLabel(
            body, text="Check (read-only)", font=_font(12, "bold"), text_color=theme.INK, anchor="w"
        ).grid(row=0, column=0, sticky="w", padx=6, pady=(4, 2))
        self.sfc_var = tk.BooleanVar(value=True)
        self.dism_var = tk.BooleanVar(value=True)
        self.sfc_check = self._check(body, SFC_TITLE, self.sfc_var)
        self.sfc_check.grid(row=1, column=0, sticky="w", padx=6, pady=(4, 0))
        self.sfc_label = _label(body, SFC_DESCRIPTION, 10, theme.INK_MUTED)
        self.sfc_label.grid(row=2, column=0, sticky="w", padx=DESCRIPTION_PADX)
        self.dism_check = self._check(body, DISM_TITLE, self.dism_var)
        self.dism_check.grid(row=3, column=0, sticky="w", padx=6, pady=(6, 0))
        self.dism_label = _label(body, DISM_DESCRIPTION, 10, theme.INK_MUTED)
        self.dism_label.grid(row=4, column=0, sticky="w", padx=DESCRIPTION_PADX)
        self.read_only_label = _label(body, READ_ONLY_TEXT, 10, theme.INK_MUTED)
        self.read_only_label.grid(row=5, column=0, sticky="w", padx=6, pady=(6, 0))
        ctk.CTkLabel(body, text="Clean up", font=_font(12, "bold"), text_color=theme.INK, anchor="w").grid(
            row=6, column=0, sticky="w", padx=6, pady=(12, 0)
        )
        self.irreversible_label = _label(body, IRREVERSIBLE_TEXT, 10, theme.WARNING)
        self.irreversible_label.grid(row=7, column=0, sticky="w", padx=6)
        self.targets_box = ctk.CTkFrame(body, fg_color="transparent")
        self.targets_box.grid(row=8, column=0, sticky="ew", pady=(2, 6))
        self.targets_box.grid_columnconfigure(0, weight=1)
        body.bind("<Configure>", self._fit_body_texts, add="+")

    def _check(self, master: tk.Misc, text: str, var: tk.BooleanVar) -> ctk.CTkCheckBox:
        return ctk.CTkCheckBox(
            master,
            text=text,
            variable=var,
            font=_font(12),
            text_color=theme.INK,
            checkbox_width=18,
            checkbox_height=18,
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            border_color=theme.BASELINE,
        )

    def _build_last_run_card(self, on_open_tools: Callable[[], None]) -> None:
        card = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        card.grid(row=1, column=1, sticky="nsew", padx=(5, 0))
        card.grid_columnconfigure(0, weight=1)
        card.grid_rowconfigure(1, weight=1)
        header = ctk.CTkFrame(card, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 2))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(header, text="Last run", font=_font(13, "bold"), text_color=theme.INK_SECONDARY).grid(
            row=0, column=0, sticky="w"
        )
        self.open_log_button = _button(header, "Open log", self._open_log, width=90, height=28)
        self.open_log_button.grid(row=0, column=1, sticky="e")
        body = _scroll_card(card)
        body.grid(row=1, column=0, sticky="nsew", padx=4, pady=(0, 6))
        body.grid_columnconfigure(0, weight=1)
        self.run_body = body

        self.run_state_label = _label(body, NO_RUNS_TEXT, 12, theme.INK_MUTED)
        self.run_state_label.grid(row=0, column=0, sticky="w", padx=6, pady=(4, 0))
        self.step_title_label = _label(body, "", 11, theme.INK)
        self.progress_bar = ctk.CTkProgressBar(
            body, height=8, progress_color=theme.ACCENT, fg_color=theme.BASELINE, mode="determinate"
        )
        self.progress_bar.set(0)
        self._bar_running = False
        self.headline_label = _label(body, "", 12, theme.INK)
        self.step_labels = [_label(body, "", 11, theme.INK_SECONDARY) for _ in range(STEP_LINES)]
        self.hints_label = _label(body, "", 10, theme.WARNING)
        self.open_tools_button = _button(body, "Open Tools", on_open_tools, width=110, height=28)
        self.earlier_title = _label(body, "Earlier runs", 10, theme.INK_MUTED)
        self.earlier_label = _label(body, "", 10, theme.INK_MUTED)
        body.bind("<Configure>", self._fit_run_texts, add="+")

    # -- wrapping ------------------------------------------------------------------------

    def _wrap(self, key: str, event: tk.Event, pad: int) -> int | None:
        """The new wrap for a card `event` resized, or None when it did not change."""
        width = int(event.width / self._get_widget_scaling())
        wrap = max(MIN_WRAP // 2, width - pad)
        if self._wraps.get(key) == wrap:
            return None
        self._wraps[key] = wrap
        return wrap

    def _fit_schedule_texts(self, event: tk.Event) -> None:
        wrap = self._wrap("schedule", event, CARD_TEXT_PAD)
        if wrap is None:
            return
        for label in (self.intro_label, self.next_label, self.hint_label, self.warnings_label):
            label.configure(wraplength=wrap)
        self.unrecorded_label.configure(wraplength=max(MIN_WRAP // 2, wrap - REMOVE_COLUMN_WIDTH))

    def _fit_body_texts(self, event: tk.Event) -> None:
        wrap = self._wrap("what", event, BODY_TEXT_PAD)
        if wrap is None:
            return
        for label in (self.read_only_label, self.irreversible_label):
            label.configure(wraplength=wrap)
        for label in (self.sfc_label, self.dism_label, *self._target_labels):
            label.configure(wraplength=_description_wrap(wrap))

    def _fit_run_texts(self, event: tk.Event) -> None:
        wrap = self._wrap("run", event, BODY_TEXT_PAD)
        if wrap is None:
            return
        labels = [
            self.run_state_label,
            self.step_title_label,
            self.headline_label,
            self.hints_label,
            self.earlier_label,
        ]
        for label in labels + self.step_labels:
            label.configure(wraplength=wrap)

    # -- state ---------------------------------------------------------------------------

    def set_loading(self) -> None:
        self.loading = True
        if not self.loaded:
            self._set_state_text(LOADING_TEXT, theme.INK_MUTED)

    def set_unsupported(self, text: str) -> None:
        """Shows that the section cannot be used (no engine, or an outdated one)."""
        self._unsupported = True
        self._set_state_text(f"– {text}", theme.INK_MUTED)
        self._apply_state()

    def show_error(self, message: str) -> None:
        self.loading = False
        self._error = message
        self._set_state_text(f"⚠ {message}", theme.WARNING)
        self._apply_state()

    def set_busy(self, text: str) -> None:
        """Shows the operation that runs and disables the actions."""
        self._busy_text = text
        self._set_state_text(f"◐ {text}", theme.ACCENT)
        self._apply_state()

    def set_idle(self) -> None:
        self._busy_text = None
        self._apply_state()

    def set_actions_enabled(self, enabled: bool) -> None:
        """Enables or disables every action, while another operation runs."""
        self._actions_enabled = enabled
        self._apply_state()

    def _set_state_text(self, text: str, color: str) -> None:
        set_text(self.state_label, text)
        _set_color(self.state_label, color)

    def show(self, status: Mapping[str, Any], *, engine_ready: bool, elevated: bool) -> None:
        """Shows `status` (see `maintenance_status`). Never raises: malformed data is shown as
        an error."""
        self.loading = False
        self._engine_ready = engine_ready
        self._elevated = elevated
        try:
            self._show(dict(status))
        except (KeyError, TypeError, ValueError, AttributeError, IndexError) as exc:
            self.show_error(f"The maintenance status could not be read: {exc}")
            return
        self._error = None
        self.loaded = True
        self._apply_state()

    def _show(self, status: dict[str, Any]) -> None:
        targets = list(status["targets"])
        ids = [str(t["id"]) for t in targets]
        if ids != self._target_ids:
            self._build_targets(targets)
        config = effective_config(status)
        if self._shown_config is None or config_differs(config, self._shown_config):
            self._apply_config(config)
            self._shown_config = config
        self.status = status
        self._show_runs(status)

    def _build_targets(self, targets: list[dict[str, Any]]) -> None:
        for child in self.targets_box.winfo_children():
            child.destroy()
        self.target_vars = {}
        self.target_rows = {}
        self._target_labels = []
        wrap = _description_wrap(self._wraps.get("what", MIN_WRAP))
        for index, target in enumerate(targets):
            target_id = str(target["id"])
            var = tk.BooleanVar(value=bool(target.get("default_on")))
            check = self._check(self.targets_box, str(target.get("title") or target_id), var)
            check.grid(row=2 * index, column=0, sticky="w", padx=6, pady=(6, 0))
            label = _label(self.targets_box, target_description(target), 10, theme.INK_MUTED)
            label.configure(wraplength=wrap)
            label.grid(row=2 * index + 1, column=0, sticky="w", padx=DESCRIPTION_PADX)
            self.target_vars[target_id] = var
            self.target_rows[target_id] = check
            self._target_labels.append(label)
        self._target_ids = [str(t["id"]) for t in targets]

    def _apply_config(self, config: Mapping[str, Any]) -> None:
        day = str(config.get("day", "sunday"))
        self.day_menu.set(day_label(day) if day in DAY_VALUES else "Sunday")
        hour, minute = config_time_to_menus(str(config.get("time", "12:00")))
        self.hour_menu.set(hour)
        self.minute_menu.set(minute)
        chosen = set(config.get("targets") or ())
        for target_id, var in self.target_vars.items():
            var.set(target_id in chosen)
        self.sfc_var.set(bool(config.get("system_file_check")))
        self.dism_var.set(bool(config.get("component_store_check")))

    def config(self) -> dict[str, Any]:
        """The schedule the controls show, as `maintenance_set_schedule` takes it."""
        day = self.day_menu.get()
        return {
            "day": DAY_VALUES[DAY_LABELS.index(day)] if day in DAY_LABELS else "sunday",
            "time": menus_to_config_time(self.hour_menu.get(), self.minute_menu.get()),
            "targets": [t for t in self._target_ids if self.target_vars[t].get()],
            "system_file_check": bool(self.sfc_var.get()),
            "component_store_check": bool(self.dism_var.get()),
        }

    def _apply_state(self) -> None:
        """Texts, buttons and conditional rows from the last status and the busy state."""
        status = self.status or {}
        ready = self._engine_ready and not self._unsupported and self.loaded and self._error is None
        enabled = ready and self._actions_enabled and self._busy_text is None
        task = status.get("task")
        recorded = bool(status.get("recorded")) and bool(task)
        unrecorded = bool(task) and not status.get("recorded")
        if ready and self._busy_text is None:
            if self._progress is not None and (
                self._progress.get("running") or self._progress.get("waiting_for_start")
            ):
                self._set_state_text(
                    "◐ Running…" if self._progress.get("running") else STARTING_TEXT, theme.ACCENT
                )
            else:
                self._set_state_text(*state_label(status))
        set_text(self.primary_button, "Save changes" if recorded else "Turn on")
        blocked = status.get("blocked_reason")
        _set_state(self.primary_button, enabled and not blocked)
        _show(self.turn_off_button, recorded, row=0, column=4, sticky="e", padx=(6, 0))
        _set_state(self.turn_off_button, enabled)
        run_block = run_now_block(status) if ready else None
        running = bool(
            self._progress and (self._progress.get("running") or self._progress.get("waiting_for_start"))
        )
        _set_state(self.run_now_button, enabled and run_block is None and not running)
        for menu in (self.day_menu, self.hour_menu, self.minute_menu):
            _set_state(menu, enabled)
        for check in [self.sfc_check, self.dism_check, *self.target_rows.values()]:
            _set_state(check, enabled)

        next_text = next_run_text(status) if ready else ""
        set_text(self.next_label, next_text)
        _show(self.next_label, bool(next_text), row=3, column=0, sticky="w", padx=14, pady=(6, 0))
        hint = ""
        if ready:
            if not self._elevated:
                hint = NEEDS_ADMIN_TEXT
            elif run_block in (TURN_ON_FIRST_TEXT, ON_BATTERY_TEXT) and not unrecorded:
                hint = run_block
            elif status.get("has_battery"):
                hint = BATTERY_NOTE
        set_text(self.hint_label, hint)
        _show(self.hint_label, bool(hint), row=4, column=0, sticky="w", padx=14, pady=(2, 0))
        warnings = [str(w) for w in status.get("warnings") or []] if ready else []
        if ready and blocked and not unrecorded:
            warnings.insert(0, str(blocked))
        set_text(self.warnings_label, "\n".join(f"⚠ {w}" for w in warnings))
        _show(self.warnings_label, bool(warnings), row=5, column=0, sticky="w", padx=14, pady=(4, 0))
        _show(self.unrecorded_box, ready and unrecorded, row=6, column=0, sticky="ew", padx=14, pady=(6, 0))
        _set_state(self.remove_button, enabled)
        _set_state(self.open_log_button, ready and self._log_run is not None)

    # -- last run ------------------------------------------------------------------------

    def _open_log(self) -> None:
        if self._log_run is not None:
            self._on_open_log(self._log_run)

    def _show_runs(self, status: Mapping[str, Any]) -> None:
        runs = list(status.get("runs") or [])
        latest = runs[0] if runs else None
        self._log_run = int(latest["id"]) if latest and latest.get("log_path") else None
        if self._progress is not None and (
            self._progress.get("running") or self._progress.get("waiting_for_start")
        ):
            return
        self._hide_progress()
        if latest is None:
            set_text(self.run_state_label, NO_RUNS_TEXT)
            _set_color(self.run_state_label, theme.INK_MUTED)
            for widget in [self.headline_label, *self.step_labels, self.hints_label, self.open_tools_button]:
                _show(widget, False)
            _show(self.earlier_title, False)
            _show(self.earlier_label, False)
            return
        text, color = run_state_line(latest)
        set_text(self.run_state_label, text)
        _set_color(self.run_state_label, color)
        report = latest.get("report") or {}
        headline = str(report.get("headline") or "")
        set_text(self.headline_label, headline)
        _show(self.headline_label, bool(headline), row=3, column=0, sticky="w", padx=6, pady=(4, 0))
        lines = step_lines(report)
        for index, label in enumerate(self.step_labels):
            if index < len(lines):
                set_text(label, lines[index][0])
                _set_color(label, lines[index][1])
            _show(label, index < len(lines), row=4 + index, column=0, sticky="w", padx=6)
        hints = attention_hints(report)
        set_text(self.hints_label, "\n".join(f"⚠ {h}" for h in hints))
        _show(self.hints_label, bool(hints), row=7, column=0, sticky="w", padx=6, pady=(4, 0))
        _show(self.open_tools_button, names_tools(report), row=8, column=0, sticky="w", padx=6, pady=(6, 0))
        earlier = earlier_lines(runs)
        set_text(self.earlier_label, "\n".join(earlier))
        _show(self.earlier_title, bool(earlier), row=9, column=0, sticky="w", padx=6, pady=(12, 0))
        _show(self.earlier_label, bool(earlier), row=10, column=0, sticky="w", padx=6)

    def _hide_progress(self) -> None:
        self._stop_bar()
        _show(self.step_title_label, False)
        _show(self.progress_bar, False)

    def _stop_bar(self) -> None:
        """Stops the indeterminate animation and its pending `after` callback."""
        if self._bar_running:
            self.progress_bar.stop()
            self._bar_running = False

    def destroy(self) -> None:
        self._stop_bar()
        super().destroy()

    def show_starting(self) -> None:
        """Shows that Run now was requested and the run has not appeared yet."""
        self.show_progress({"running": False, "waiting_for_start": True, "run": None})

    def show_progress(self, observation: Mapping[str, Any] | None) -> None:
        """Follows a run in progress (or one about to start); once none is, shows the last
        status again."""
        active = bool(observation) and bool(
            observation.get("running") or observation.get("waiting_for_start")
        )
        if not active:
            if self._progress is not None:
                self._progress = None
                self._hide_progress()
                if self.status is not None:
                    self._show_runs(self.status)
                self._apply_state()
            return
        assert observation is not None
        self._progress = dict(observation)
        run = observation.get("run") or {}
        progress = run.get("progress") or {}
        for widget in [
            self.headline_label,
            *self.step_labels,
            self.hints_label,
            self.open_tools_button,
            self.earlier_title,
            self.earlier_label,
        ]:
            _show(widget, False)
        if observation.get("running"):
            text, color = run_state_line(run)
            title = str(progress.get("title") or "Running…")
        else:
            text, color = STARTING_TEXT, theme.ACCENT
            title = "Waiting for Task Scheduler to start the run…"
        set_text(self.run_state_label, text)
        _set_color(self.run_state_label, color)
        set_text(self.step_title_label, title)
        _show(self.step_title_label, True, row=1, column=0, sticky="w", padx=6, pady=(2, 0))
        _show(self.progress_bar, True, row=2, column=0, sticky="ew", padx=6, pady=(6, 0))
        percent = progress.get("percent") if observation.get("running") else None
        if isinstance(percent, int | float):
            self._stop_bar()
            if self.progress_bar.cget("mode") != "determinate":
                self.progress_bar.configure(mode="determinate")
            self.progress_bar.set(max(0.0, min(1.0, float(percent) / 100.0)))
        elif not self._bar_running:
            self.progress_bar.configure(mode="indeterminate")
            self.progress_bar.start()
            self._bar_running = True
        self._apply_state()
