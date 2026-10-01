"""Boot history section: how long Windows took to start and shut down, and what slowed it.

A header card holds the summary, the trend, the Fast Startup and unexpected-shutdown notes and
what the engine could not read; a chart card shows the last 30 starts; a bottom card switches
between what slows startup, the recent starts and the shutdowns (with what slowed them).
Windows times only full starts, such as restarts, so Fast Startup starts are not listed. It
keeps these records where only administrators can read them, so without elevation the section
says so and reads nothing.
"""

from __future__ import annotations

import math
import tkinter as tk
from collections.abc import Callable, Mapping, Sequence
from datetime import datetime
from typing import Any, NamedTuple

import customtkinter as ctk

from .. import theme
from .charts import LABEL_FONT, TOOLTIP_FONT, canvas_size
from .security import ScaledFrame

TITLE = "Boot history"
PLACEHOLDER = "How long Windows takes to start and shut down, and what slows it down."
LOADING_TEXT = "Reading startup records…"
NEEDS_ADMIN_TEXT = (
    "⚠ Windows keeps its startup records where only administrators can read them. "
    "Restart Cairn as administrator to see them."
)
LOG_DISABLED_TEXT = (
    "Windows is not recording startup performance on this PC: its Diagnostics-Performance log is turned off."
)
LOG_MISSING_TEXT = "This copy of Windows does not keep startup performance records."
NO_BOOTS_TEXT = "No full starts are recorded yet. Windows records one after each restart."
FAST_STARTUP_TEXT = (
    "Fast Startup is on: Windows times only full starts (restarts), so most starts are not listed."
)
# (mark, mark colour, text) of the chart's legend.
LEGEND = (
    ("■", theme.CPU, "Until the desktop appeared"),
    ("■", theme.blend(theme.CPU, theme.SURFACE, 0.45), "Until Windows settled"),
    ("⚠", theme.WARNING, "Slower than usual"),
)
TURN_OFF_TEXT = "Turn off at startup"
TURN_OFF_NAMED_TEXT = "Turn off “{name}” at startup"
# Longest entry name the button shows in full.
TURN_OFF_NAME_CHARS = 40
ALREADY_OFF_TEXT = "✓ Off at startup"

BOOT_TYPES = {"full": "Full start", "fast_startup": "Fast Startup", "hibernate": "Resume", "unknown": "–"}
KIND_CHIPS = {
    "app": "App",
    "driver": "Driver",
    "service": "Service",
    "device": "Device",
    "windows": "Windows",
    "prefetch": "Prefetch",
    "policy": "Policy",
}
ADVICE_MATCHED = (
    "Starts with Windows. Turning it off at startup makes starts faster; you can still open it yourself."
)
ADVICE_UNMATCHED = (
    "No startup entry was matched to it; a service, a scheduled task or another app may start it."
)
ADVICE = {
    "driver": "Took long to initialize. A newer driver from the device maker often helps.",
    "device": "Took long to initialize. A newer driver from the device maker often helps.",
    "service": "A service took long to start.",
    "windows": "Windows itself took longer than usual; this usually settles after a few starts, for "
    "example after an update.",
}
# kind -> (button text, Windows tool id).
KIND_TOOLS = {
    "driver": ("Open Device Manager", "device_manager"),
    "device": ("Open Device Manager", "device_manager"),
    "service": ("Open Services", "services"),
}
EVENT_VIEWER = "event_viewer"
# Views of the bottom card: key -> button text (the first gets the count of what slowed starts).
VIEWS = {"slow": "Slows startup", "starts": "Recent starts", "shutdowns": "Shutdowns"}

# Axis ceilings in seconds: the smallest that holds the slowest start shown.
NICE_CEILINGS_S = (10, 20, 30, 60, 90, 120, 180, 300, 600)
MAX_BARS = 30
TREND_THRESHOLD = 15.0
POST_BOOT_COLOR = theme.blend(theme.CPU, theme.SURFACE, 0.45)
CARD_PAD = 14


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def _number(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, int | float):
        return None
    return float(value) if math.isfinite(value) else None


def duration_text(ms: Any) -> str:
    """A duration: "41.2 s" below a minute, "1 min 5 s" from a minute, "–" when missing."""
    value = _number(ms)
    if value is None or value < 0:
        return "–"
    if value < 60_000:
        return f"{value / 1000:.1f} s"
    seconds = int((value + 500) // 1000)
    return f"{seconds // 60} min {seconds % 60} s"


def nice_ceiling_ms(ms: Any) -> int:
    """The chart's top in milliseconds: the smallest of 10, 20, 30, 60, 90, 120, 180, 300 and
    600 s that holds `ms`, then whole 10 minutes."""
    value = max(_number(ms) or 0.0, 0.0)
    for seconds in NICE_CEILINGS_S:
        if value <= seconds * 1000:
            return seconds * 1000
    return int(math.ceil(value / 600_000)) * 600_000


class Bar(NamedTuple):
    """Where one start is drawn: its bar spans x0..x1; the desktop appeared at y_main, Windows
    settled at y_top, the axis is at y_base (canvas coordinates, y grows downwards)."""

    x0: float
    x1: float
    y_main: float
    y_top: float
    y_base: float
    fast: bool
    degraded: bool
    boot: Mapping[str, Any]


# Plot margins of the chart, in pixels.
PAD_LEFT = 42
PAD_RIGHT = 8
PAD_TOP = 18
PAD_BOTTOM = 6
BAR_GAP = 2
MAX_BAR_WIDTH = 24


def chart_boots(boots: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    """The starts the chart shows, oldest first: the newest `MAX_BARS` of `boots` (newest
    first)."""
    return list(reversed(list(boots)[:MAX_BARS]))


def bar_geometry(boots: Sequence[Mapping[str, Any]], width: float, height: float, top_ms: float) -> list[Bar]:
    """The bars of the newest 30 of `boots` (newest first), oldest left, in a `width` x
    `height` canvas whose top stands for `top_ms`. Bar heights are proportional to the time;
    each bar has at least 1 px."""
    shown = chart_boots(boots)
    x0, x1 = PAD_LEFT, max(width - PAD_RIGHT, PAD_LEFT + 1)
    y0, base = PAD_TOP, max(height - PAD_BOTTOM, PAD_TOP + 1)
    span = base - y0
    top = max(float(top_ms), 1.0)
    slot = (x1 - x0) / max(len(shown), 1)
    bar_width = max(min(slot - BAR_GAP, MAX_BAR_WIDTH), 1)
    bars = []
    for index, boot in enumerate(shown):
        total = max(_number(boot.get("boot_ms")) or 0.0, 0.0)
        main = min(max(_number(boot.get("main_path_ms")) or 0.0, 0.0), total)
        left = x0 + slot * index + (slot - bar_width) / 2
        y_top = base - max(min(total / top, 1.0) * span, 1.0)
        y_main = base - min(main / top, 1.0) * span
        bars.append(
            Bar(
                x0=left,
                x1=left + bar_width,
                y_main=max(y_main, y_top),
                y_top=y_top,
                y_base=base,
                fast=boot.get("boot_type") == "fast_startup",
                degraded=bool(boot.get("degraded")),
                boot=boot,
            )
        )
    return bars


def summary_text(history: Mapping[str, Any]) -> str:
    """The summary line: "Last full start 41.2 s (desktop after 18.0 s)  ·  Typical 38.5 s over 24
    starts". Windows times only full starts, so those are the starts listed."""
    boots = history.get("boots") or []
    if not boots:
        return NO_BOOTS_TEXT
    latest = boots[0]
    stats = history.get("stats") or {}
    count = stats.get("count") if isinstance(stats.get("count"), int) else len(boots)
    return (
        f"Last full start {duration_text(latest.get('boot_ms'))} "
        f"(desktop after {duration_text(latest.get('main_path_ms'))})  ·  "
        f"Typical {duration_text(stats.get('median_ms'))} over {count} start{'' if count == 1 else 's'}"
    )


def trend_text(stats: Mapping[str, Any]) -> tuple[str, str]:
    """(text, colour) of the trend line; empty text without enough starts."""
    trend = stats.get("trend")
    if not isinstance(trend, Mapping):
        return "", theme.INK_MUTED
    change = _number(trend.get("change_pct"))
    if change is None:
        return "", theme.INK_MUTED
    if change >= TREND_THRESHOLD:
        return f"⚠ Starts are getting slower: {change:.0f}% slower than earlier starts", theme.WARNING
    if change <= -TREND_THRESHOLD:
        return f"✓ Starts are getting faster: {-change:.0f}% faster", theme.GOOD
    return "✓ Start times are steady", theme.GOOD


def _moment(stamp: Any) -> datetime | None:
    if not isinstance(stamp, str) or not stamp:
        return None
    try:
        return datetime.fromisoformat(stamp.replace("Z", "+00:00")).astimezone()
    except ValueError:
        return None


def when_text(record: Mapping[str, Any], fmt: str = "%Y-%m-%d %H:%M") -> str:
    """Local time a start or shutdown began (else when it was logged)."""
    moment = _moment(record.get("started_at")) or _moment(record.get("logged_at"))
    return moment.strftime(fmt) if moment is not None else "–"


def slowed_by(record: Mapping[str, Any]) -> str:
    """What slowed a start or shutdown: "Slowed by: OneDrive, Contoso Driver", or empty."""
    titles: list[str] = []
    for event in record.get("slow") or []:
        if isinstance(event, Mapping):
            title = str(event.get("title") or event.get("name") or "")
            if title and title not in titles:
                titles.append(title)
    return f"Slowed by: {', '.join(titles)}" if titles else ""


def boot_line(boot: Mapping[str, Any]) -> str:
    """One recent start: time, type, durations, startup apps and what was special."""
    line = (
        f"{when_text(boot)}  ·  {BOOT_TYPES.get(str(boot.get('boot_type')), '–')}  ·  "
        f"{duration_text(boot.get('boot_ms'))} (desktop {duration_text(boot.get('main_path_ms'))})"
    )
    apps = boot.get("startup_apps")
    if isinstance(apps, int) and not isinstance(apps, bool):
        line += f"  ·  {apps} startup app{'' if apps == 1 else 's'}"
    if boot.get("degraded"):
        line += "  ·  ⚠ slower than usual"
    if boot.get("after_update"):
        line += "  ·  after an update"
    if boot.get("after_unexpected_shutdown"):
        line += "  ·  after an unexpected shutdown"
    return line


def shutdown_line(shutdown: Mapping[str, Any]) -> str:
    """One shutdown: time and duration, and whether it was slower than usual."""
    line = f"{when_text(shutdown)}  ·  {duration_text(shutdown.get('shutdown_ms'))}"
    if shutdown.get("degraded"):
        line += "  ·  ⚠ slower than usual"
    return line


def tooltip_text(boot: Mapping[str, Any]) -> str:
    """Tooltip of a bar: "Sat 27 Sep 08:54  ·  Full start  ·  41.2 s (desktop 18.0 s)  ·
    12 startup apps  ·  2 slow items"."""
    parts = [
        when_text(boot, "%a %d %b %H:%M"),
        BOOT_TYPES.get(str(boot.get("boot_type")), "–"),
        f"{duration_text(boot.get('boot_ms'))} (desktop {duration_text(boot.get('main_path_ms'))})",
    ]
    apps = boot.get("startup_apps")
    if isinstance(apps, int) and not isinstance(apps, bool):
        parts.append(f"{apps} startup app{'' if apps == 1 else 's'}")
    slow = [e for e in boot.get("slow") or [] if isinstance(e, Mapping)]
    if slow:
        parts.append(f"{len(slow)} slow item{'' if len(slow) == 1 else 's'}")
    return "  ·  ".join(parts)


def slow_item_lines(item: Mapping[str, Any], boots: int, shutdowns: int = 0) -> tuple[str, str]:
    """(line, advice) of a slow item: "{count} of the last {n} starts  ·  usually adds 2.5 s  ·
    last on 2026-09-27" (shutdowns for a shutdown item) and what to do about it."""
    count = item.get("count") if isinstance(item.get("count"), int) else 0
    shutdown = item.get("phase") == "shutdown"
    records, noun = (shutdowns, "shutdown") if shutdown else (boots, "start")
    parts = [f"{count} of the last {records} {noun}{'' if records == 1 else 's'}"]
    parts.append(f"usually adds {duration_text(item.get('median_degradation_ms'))}")
    last = _moment(item.get("last_seen"))
    if last is not None:
        parts.append(f"last on {last.strftime('%Y-%m-%d')}")
    kind = str(item.get("kind"))
    if kind == "app":
        # Only an app that one startup entry starts can be turned off (see `startup_action`).
        ids = item.get("startup_ids")
        matched = isinstance(ids, list | tuple) and len({str(i) for i in ids}) == 1
        advice = ADVICE_MATCHED if matched else ADVICE_UNMATCHED
    else:
        advice = ADVICE.get(kind, ADVICE["windows"])
    return "  ·  ".join(parts), advice


def startup_action(
    item: Mapping[str, Any], entries: Sequence[Mapping[str, Any]]
) -> tuple[str, Mapping[str, Any] | None]:
    """What the slow item's startup button does with the one startup entry that starts it:
    ("turn_off", entry) while it is on (whether it can be toggled is the entry's
    `can_toggle`), ("already_off", entry) once it is off. ("none", None) without such an
    entry, and when the item names several: turning off a guessed one could stop another
    program from starting."""
    ids = {str(i) for i in item.get("startup_ids") or []}
    matched = [e for e in entries if str(e.get("id")) in ids]
    if len(ids) != 1 or len(matched) != 1:
        return "none", None
    entry = matched[0]
    return ("turn_off" if entry.get("enabled") else "already_off"), entry


def turn_off_text(entry: Mapping[str, Any]) -> str:
    """Text of the button that turns `entry` off at startup: "Turn off “Discord” at startup",
    with a long name cut to `TURN_OFF_NAME_CHARS` characters."""
    name = " ".join(str(entry.get("name") or "").split())
    if not name:
        return TURN_OFF_TEXT
    if len(name) > TURN_OFF_NAME_CHARS:
        name = name[: TURN_OFF_NAME_CHARS - 1].rstrip() + "…"
    return TURN_OFF_NAMED_TEXT.format(name=name)


def slow_items(history: Mapping[str, Any], phase: str) -> list[Mapping[str, Any]]:
    """The slow items of one phase ("startup" or "shutdown") in the engine's order; an item
    without a phase slowed starts."""
    return [
        item
        for item in history.get("slow_items") or []
        if isinstance(item, Mapping) and (item.get("phase") or "startup") == phase
    ]


def unexpected_text(history: Mapping[str, Any]) -> str:
    """The unexpected-shutdown note ("⚠ Windows was shut down unexpectedly 2 times (crash or
    power loss), last on 2026-09-27."), or empty."""
    times = [t for t in history.get("unexpected_shutdowns") or [] if isinstance(t, str)]
    if not times:
        return ""
    last = _moment(times[0])
    when = last.strftime("%Y-%m-%d") if last is not None else times[0]
    count = len(times)
    return (
        f"⚠ Windows was shut down unexpectedly {count} time{'' if count == 1 else 's'} "
        f"(crash or power loss), last on {when}."
    )


def notice_lines(history: Mapping[str, Any]) -> list[str]:
    """What the engine could not read while building the history: its notes (the startup list),
    then its errors (the start types, the unexpected shutdowns)."""
    lines: list[str] = []
    for key in ("notes", "errors"):
        values = history.get(key)
        if isinstance(values, list | tuple):
            lines.extend(str(value) for value in values if value)
    return lines


def view_label(key: str, slow_count: int) -> str:
    """Button text of a bottom-card view."""
    return f"{VIEWS[key]} ({slow_count})" if key == "slow" else VIEWS[key]


class BootChart(tk.Canvas):
    """The last 30 starts as stacked bars, oldest left: until the desktop appeared (series
    colour) and until Windows settled (lighter); Fast Startup starts are outlined. A dashed line
    marks the typical time, ⚠ a start Windows called slower than usual. Items are created once
    and moved with `coords`."""

    def __init__(self, master: tk.Misc, *, height: int = 170) -> None:
        super().__init__(master, height=height, bg=theme.SURFACE, highlightthickness=0, bd=0)
        self._boots: list[Mapping[str, Any]] = []
        self._median: float | None = None
        self._bars: list[Bar] = []
        self._main: list[int] = []
        self._post: list[int] = []
        self._warn: list[int] = []
        self._grid = [self.create_line(0, 0, 0, 0, fill=theme.GRID, width=1) for _ in range(2)]
        self._baseline = self.create_line(0, 0, 0, 0, fill=theme.BASELINE, width=1)
        self._y_labels = [
            self.create_text(0, 0, text="", fill=theme.INK_MUTED, font=LABEL_FONT, anchor="e")
            for _ in range(3)
        ]
        self._median_line = self.create_line(
            0, 0, 0, 0, fill=theme.INK_MUTED, width=1, dash=(4, 3), state="hidden"
        )
        self._median_label = self.create_text(
            0, 0, text="", fill=theme.INK_MUTED, font=LABEL_FONT, anchor="se", state="hidden"
        )
        self._tip_bg = self.create_rectangle(
            0, 0, 0, 0, fill=theme.SURFACE_RAISED, outline=theme.BORDER, state="hidden"
        )
        self._tip = self.create_text(
            0, 0, text="", fill=theme.INK, font=TOOLTIP_FONT, anchor="nw", state="hidden"
        )
        self._hover: int | None = None
        self.bind("<Configure>", lambda _e: self._layout())
        self.bind("<Motion>", self._on_motion)
        self.bind("<Leave>", self._on_leave)

    @property
    def shown_bars(self) -> int:
        """How many starts the chart draws."""
        return sum(1 for item in self._main if self.itemcget(item, "state") != "hidden")

    @property
    def bars(self) -> list[Bar]:
        return list(self._bars)

    def render(self, boots: Sequence[Mapping[str, Any]], median_ms: Any) -> None:
        """Draws `boots` (newest first; the newest 30 are shown) with the typical time."""
        self._boots = [b for b in boots if isinstance(b, Mapping)]
        self._median = _number(median_ms)
        self._ensure(len(chart_boots(self._boots)))
        self._layout()

    def _ensure(self, count: int) -> None:
        while len(self._main) < count:
            self._post.append(self.create_rectangle(0, 0, 0, 0, fill=POST_BOOT_COLOR, outline=""))
            self._main.append(self.create_rectangle(0, 0, 0, 0, fill=theme.CPU, outline=""))
            self._warn.append(
                self.create_text(
                    0, 0, text="⚠", fill=theme.WARNING, font=LABEL_FONT, anchor="s", state="hidden"
                )
            )
        for index in range(len(self._main)):
            state = "normal" if index < count else "hidden"
            self.itemconfigure(self._main[index], state=state)
            self.itemconfigure(self._post[index], state=state)
            if index >= count:
                self.itemconfigure(self._warn[index], state="hidden")

    def _layout(self) -> None:
        width, height = canvas_size(self)
        shown = chart_boots(self._boots)
        slowest = max((_number(b.get("boot_ms")) or 0.0 for b in shown), default=0.0)
        top = nice_ceiling_ms(max(slowest, self._median or 0.0))
        self._bars = bar_geometry(self._boots, width, height, top)
        x0, x1 = PAD_LEFT, max(width - PAD_RIGHT, PAD_LEFT + 1)
        y0, base = PAD_TOP, max(height - PAD_BOTTOM, PAD_TOP + 1)
        for index, y in enumerate((y0, (y0 + base) / 2)):
            self.coords(self._grid[index], x0, y, x1, y)
        self.coords(self._baseline, x0, base, x1, base)
        for index, (y, value) in enumerate(((y0, top), ((y0 + base) / 2, top / 2), (base, 0))):
            self.coords(self._y_labels[index], x0 - 6, y)
            self.itemconfigure(self._y_labels[index], text=f"{value / 1000:g} s")
        for index, bar in enumerate(self._bars):
            main, post, warn = self._main[index], self._post[index], self._warn[index]
            self.coords(post, bar.x0, bar.y_top, bar.x1, bar.y_main)
            self.coords(main, bar.x0, bar.y_main, bar.x1, bar.y_base)
            if bar.fast:
                self.itemconfigure(main, fill=theme.SURFACE, outline=theme.CPU, width=1)
                self.itemconfigure(post, fill=theme.SURFACE, outline=POST_BOOT_COLOR, width=1)
            else:
                self.itemconfigure(main, fill=theme.CPU, outline="", width=0)
                self.itemconfigure(post, fill=POST_BOOT_COLOR, outline="", width=0)
            self.coords(warn, (bar.x0 + bar.x1) / 2, bar.y_top - 1)
            self.itemconfigure(warn, state="normal" if bar.degraded else "hidden")
        if self._median is not None and self._bars:
            y = base - min(self._median / max(top, 1), 1.0) * (base - y0)
            self.coords(self._median_line, x0, y, x1, y)
            self.coords(self._median_label, x1, y - 2)
            self.itemconfigure(self._median_label, text=f"typical {duration_text(self._median)}")
            for item in (self._median_line, self._median_label):
                self.itemconfigure(item, state="normal")
                self.tag_raise(item)
        else:
            for item in (self._median_line, self._median_label):
                self.itemconfigure(item, state="hidden")
        if self._hover is not None:
            self._show_tip(self._hover)

    def _on_motion(self, event: tk.Event) -> None:
        hit = next(
            (i for i, bar in enumerate(self._bars) if bar.x0 - BAR_GAP <= event.x <= bar.x1 + BAR_GAP),
            None,
        )
        self._hover = hit
        if hit is None:
            self._on_leave(event)
        else:
            self._show_tip(hit)

    def _on_leave(self, _event: tk.Event | None = None) -> None:
        self._hover = None
        self.itemconfigure(self._tip, state="hidden")
        self.itemconfigure(self._tip_bg, state="hidden")

    def _show_tip(self, index: int) -> None:
        if index >= len(self._bars):
            return
        bar = self._bars[index]
        self.itemconfigure(self._tip, text=tooltip_text(bar.boot))
        bbox = self.bbox(self._tip) or (0, 0, 0, 0)
        tip_width = bbox[2] - bbox[0]
        width = canvas_size(self)[0]
        x = min(max((bar.x0 + bar.x1) / 2 - tip_width / 2 - 4, 2), max(width - tip_width - 10, 2))
        self.coords(self._tip, x + 4, 2)
        self.coords(self._tip_bg, x, 0, x + tip_width + 8, 18)
        for item in (self._tip_bg, self._tip):
            self.itemconfigure(item, state="normal")
            self.tag_raise(item)


class SlowItemCard(ScaledFrame):
    """One thing that slowed starts or shutdowns: title, kind, how often and how much, its path,
    advice, and the startup button (naming the entry it turns off) or a Windows tool button."""

    def __init__(
        self,
        master: tk.Misc,
        item: Mapping[str, Any],
        *,
        boots: int,
        shutdowns: int,
        entries: Sequence[Mapping[str, Any]],
        on_turn_off: Callable[[Mapping[str, Any]], None],
        on_open_tool: Callable[[str], None],
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=8, border_width=1, border_color=theme.BORDER
        )
        self.item = item
        self._boots = boots
        self._shutdowns = shutdowns
        self._on_turn_off = on_turn_off
        self._on_open_tool = on_open_tool
        self.action, self.entry = startup_action(item, entries)
        self.startup_button: ctk.CTkButton | None = None
        self.tool_button: ctk.CTkButton | None = None
        self.note_label: tk.Label | None = None
        self._busy = False
        self._wrapped: list[tk.Label] = []
        self.grid_columnconfigure(0, weight=1)
        self._build()
        self.bind("<Configure>", self._on_resize, add="+")

    def _build(self) -> None:
        item = self.item
        pad = self._px(12)
        self._wrapped = []
        head = tk.Frame(self, bg=self._bg)
        self._content.append(head)
        head.grid(row=0, column=0, sticky="ew", padx=pad, pady=(self._px(10), 0))
        self._label(
            head, str(item.get("title") or item.get("name") or ""), 12, theme.INK, weight="bold"
        ).pack(side="left")
        chip = KIND_CHIPS.get(str(item.get("kind")), "")
        if chip:
            self._label(head, chip, 10, theme.INK_SECONDARY, weight="bold").pack(
                side="left", padx=(self._px(10), 0)
            )
        line, advice = slow_item_lines(item, self._boots, self._shutdowns)
        row = 1
        for text, size, color in (
            (line, 11, theme.INK_SECONDARY),
            (str(item.get("path") or ""), 10, theme.INK_MUTED),
            (advice, 10, theme.INK_SECONDARY),
        ):
            if not text:
                continue
            label = self._label(self, text, size, color)
            label.grid(row=row, column=0, sticky="ew", padx=pad, pady=(self._px(3), 0))
            self._wrapped.append(label)
            row += 1
        buttons = ctk.CTkFrame(self, fg_color="transparent")
        self._content.append(buttons)
        column = 0
        self.startup_button = None
        if self.action == "turn_off" and self.entry is not None:
            self.startup_button = ctk.CTkButton(
                buttons,
                text=turn_off_text(self.entry),
                height=28,
                width=0,
                font=_font(11, "bold"),
                fg_color=theme.ACCENT,
                hover_color=theme.ACCENT_HOVER,
                command=self._turn_off,
            )
        elif self.action == "already_off":
            self.startup_button = ctk.CTkButton(
                buttons,
                text=ALREADY_OFF_TEXT,
                height=28,
                width=0,
                font=_font(11),
                fg_color=theme.BUTTON_NEUTRAL,
                hover_color=theme.BUTTON_NEUTRAL_HOVER,
                state="disabled",
            )
        if self.startup_button is not None:
            self.startup_button.grid(row=0, column=column, padx=(0, 8))
            column += 1
        self.tool_button = None
        tool = KIND_TOOLS.get(str(item.get("kind")))
        if tool is not None:
            text, tool_id = tool
            self.tool_button = ctk.CTkButton(
                buttons,
                text=text,
                height=28,
                width=0,
                font=_font(11),
                fg_color=theme.BUTTON_NEUTRAL,
                hover_color=theme.BUTTON_NEUTRAL_HOVER,
                command=lambda t=tool_id: self._on_open_tool(t),
            )
            self.tool_button.grid(row=0, column=column, padx=(0, 8))
            column += 1
        if column:
            buttons.grid(row=row, column=0, sticky="w", padx=pad, pady=(self._px(8), 0))
            row += 1
        self.note_label = None
        if self.action == "turn_off" and self.entry is not None and not self.entry.get("can_toggle", True):
            self.note_label = self._label(self, str(self.entry.get("note") or ""), 10, theme.INK_MUTED)
            self.note_label.grid(row=row, column=0, sticky="ew", padx=pad, pady=(self._px(4), 0))
            self._wrapped.append(self.note_label)
            row += 1
        self.grid_rowconfigure(row, minsize=self._px(10))
        self._refresh_button()

    def _on_resize(self, event: tk.Event) -> None:
        wrap = max(event.width - 2 * self._px(12) - self._px(4), self._px(120))
        for label in self._wrapped:
            if label.winfo_exists() and int(label.cget("wraplength")) != wrap:
                label.configure(wraplength=wrap)

    def _turn_off(self) -> None:
        if self.entry is not None:
            self._on_turn_off(self.entry)

    def set_busy(self, busy: bool) -> None:
        self._busy = busy
        self._refresh_button()

    def _refresh_button(self) -> None:
        if self.action != "turn_off" or self.startup_button is None or self.entry is None:
            return
        enabled = bool(self.entry.get("can_toggle", True)) and not self._busy
        self.startup_button.configure(state="normal" if enabled else "disabled")


class LineList(ScaledFrame):
    """Records as lines: each a main line and, when given, a muted second line."""

    def __init__(self, master: tk.Misc, lines: Sequence[tuple[str, str]]) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=8, border_width=1, border_color=theme.BORDER
        )
        self.lines = list(lines)
        self.grid_columnconfigure(0, weight=1)
        self._build()

    def _build(self) -> None:
        pad = self._px(12)
        row = 0
        for index, (main, second) in enumerate(self.lines):
            top = self._px(8 if index == 0 else 4)
            self._label(self, main, 11, theme.INK).grid(
                row=row, column=0, sticky="ew", padx=pad, pady=(top, 0)
            )
            row += 1
            if second:
                self._label(self, second, 10, theme.INK_MUTED).grid(
                    row=row, column=0, sticky="ew", padx=pad + self._px(12), pady=(self._px(1), 0)
                )
                row += 1
        self.grid_rowconfigure(row, minsize=self._px(8))


class BootPanel(ctk.CTkFrame):
    """Header card, chart card and the bottom card with its three views.

    Nothing is read before the section is first shown; Refresh reads the history again.
    `view` is the bottom card's view ("slow", "starts" or "shutdowns"), `slow_cards` the cards
    of the slow items: those that slowed starts (the "slow" view), then those that slowed
    shutdowns (the "shutdowns" view).
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_refresh: Callable[[], None],
        on_elevate: Callable[[], None],
        on_turn_off: Callable[[Mapping[str, Any]], None],
        on_open_tool: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_elevate = on_elevate
        self._on_turn_off = on_turn_off
        self._on_open_tool = on_open_tool
        self.loaded = False
        self.loading = False
        self.history: Mapping[str, Any] | None = None
        self.view = "slow"
        self.slow_cards: list[SlowItemCard] = []
        self._view_frames: dict[str, ctk.CTkFrame] = {}
        self._busy = False
        self._unsupported = False
        self._message_action: str | None = None
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(2, weight=1)

        header = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        header.grid(row=0, column=0, sticky="ew", pady=(0, 8))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            header, text=TITLE, font=_font(13, "bold"), text_color=theme.INK_SECONDARY, anchor="w"
        ).grid(row=0, column=0, sticky="w", padx=CARD_PAD, pady=(10, 0))
        self.refresh_button = ctk.CTkButton(
            header,
            text="Refresh",
            width=100,
            height=30,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_refresh,
        )
        self.refresh_button.grid(row=0, column=1, rowspan=2, sticky="ne", padx=CARD_PAD, pady=10)
        self.summary_label = self._header_label(header, PLACEHOLDER, 12, theme.INK_MUTED, weight="bold")
        self.summary_label.grid(row=1, column=0, sticky="w", padx=CARD_PAD, pady=(2, 0))
        self.trend_label = self._header_label(header, "", 11, theme.GOOD)
        self.fast_label = self._header_label(header, FAST_STARTUP_TEXT, 10, theme.INK_MUTED)
        self.crash_label = self._header_label(header, "", 10, theme.WARNING)
        self.notice_label = self._header_label(header, "", 10, theme.WARNING)
        self.message_row = ctk.CTkFrame(header, fg_color="transparent")
        self.message_row.grid_columnconfigure(0, weight=1)
        self.message_label = ctk.CTkLabel(
            self.message_row, text="", font=_font(11), text_color=theme.WARNING, anchor="w", justify="left"
        )
        self.message_label.grid(row=0, column=0, sticky="w")
        self.message_button = ctk.CTkButton(
            self.message_row,
            text="",
            width=0,
            height=28,
            font=_font(11, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            command=self._message_clicked,
        )
        self.meta_label = self._header_label(header, "", 10, theme.INK_MUTED)
        self._header = header
        self._header_rows = {
            "trend": (self.trend_label, 2),
            "fast": (self.fast_label, 3),
            "crash": (self.crash_label, 4),
            "notice": (self.notice_label, 5),
        }
        header.bind("<Configure>", self._on_header_resize, add="+")

        self.chart_card = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self.chart_card.grid_columnconfigure(0, weight=1)
        self.chart = BootChart(self.chart_card, height=170)
        self.chart.configure(width=1)
        self.chart.grid(row=0, column=0, sticky="ew", padx=CARD_PAD, pady=(10, 2))
        self.legend = ctk.CTkFrame(self.chart_card, fg_color="transparent")
        self.legend.grid(row=1, column=0, sticky="w", padx=CARD_PAD, pady=(0, 8))
        for index, (mark, color, text) in enumerate(LEGEND):
            ctk.CTkLabel(self.legend, text=mark, font=_font(10), text_color=color, height=18).grid(
                row=0, column=2 * index, sticky="w", padx=(0 if index == 0 else 14, 4)
            )
            ctk.CTkLabel(self.legend, text=text, font=_font(10), text_color=theme.INK_MUTED, height=18).grid(
                row=0, column=2 * index + 1, sticky="w"
            )

        self.bottom = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self.bottom.grid_columnconfigure(0, weight=1)
        self.bottom.grid_rowconfigure(1, weight=1)
        self.view_selector = ctk.CTkSegmentedButton(
            self.bottom,
            values=[view_label(key, 0) for key in VIEWS],
            font=_font(11),
            command=self._view_clicked,
        )
        self.view_selector.grid(row=0, column=0, sticky="w", padx=CARD_PAD, pady=(10, 6))
        self.list = ctk.CTkScrollableFrame(
            self.bottom,
            fg_color="transparent",
            height=40,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.list.grid(row=1, column=0, sticky="nsew", padx=(CARD_PAD - 4, 4), pady=(0, 8))
        self.list.grid_columnconfigure(0, weight=1)
        self.empty_label = ctk.CTkLabel(
            self.list, text="", font=_font(11), text_color=theme.INK_MUTED, anchor="w"
        )

    # -- helpers -------------------------------------------------------------------------

    @staticmethod
    def _header_label(
        master: tk.Misc, text: str, size: int, color: str, *, weight: str = "normal"
    ) -> ctk.CTkLabel:
        return ctk.CTkLabel(
            master, text=text, font=_font(size, weight), text_color=color, anchor="w", justify="left"
        )

    def _on_header_resize(self, event: tk.Event) -> None:
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        wrap = max(int(event.width / scale) - 2 * CARD_PAD - 8, 200)
        for label in (
            self.trend_label,
            self.fast_label,
            self.crash_label,
            self.notice_label,
            self.meta_label,
        ):
            if label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)
        # The summary shares its line with the Refresh button.
        summary_wrap = max(wrap - 120, 200)
        if self.summary_label.cget("wraplength") != summary_wrap:
            self.summary_label.configure(wraplength=summary_wrap)
        message_wrap = max(wrap - 200, 160)
        if self.message_label.cget("wraplength") != message_wrap:
            self.message_label.configure(wraplength=message_wrap)

    def _show_header_row(self, name: str, text: str, color: str | None = None) -> None:
        label, row = self._header_rows[name]
        if text:
            label.configure(text=text, **({"text_color": color} if color else {}))
            label.grid(row=row, column=0, columnspan=2, sticky="w", padx=CARD_PAD, pady=(2, 0))
        else:
            label.grid_forget()

    def _show_message(self, text: str, *, color: str = theme.WARNING, action: str | None = None) -> None:
        """A message under the summary, with an action button ("elevate" or "event_viewer")."""
        self._message_action = action
        if not text:
            self.message_row.grid_forget()
            return
        self.message_label.configure(text=text, text_color=color)
        self.message_row.grid(row=6, column=0, columnspan=2, sticky="ew", padx=CARD_PAD, pady=(4, 0))
        if action == "elevate":
            self.message_button.configure(text="Restart as administrator")
        elif action == "event_viewer":
            self.message_button.configure(text="Open Event Viewer")
        if action is None:
            self.message_button.grid_forget()
        else:
            self.message_button.grid(row=0, column=1, sticky="e", padx=(10, 0))

    def _message_clicked(self) -> None:
        if self._message_action == "elevate":
            self._on_elevate()
        elif self._message_action == "event_viewer":
            self._on_open_tool(EVENT_VIEWER)

    def _show_meta(self, text: str) -> None:
        if text:
            self.meta_label.configure(text=text)
            self.meta_label.grid(row=7, column=0, columnspan=2, sticky="w", padx=CARD_PAD, pady=(2, 0))
        else:
            self.meta_label.grid_forget()

    def _show_cards(self, shown: bool) -> None:
        if shown:
            self.chart_card.grid(row=1, column=0, sticky="ew", pady=(0, 8))
            self.bottom.grid(row=2, column=0, sticky="nsew")
        else:
            self.chart_card.grid_forget()
            self.bottom.grid_forget()

    def _header_bottom(self) -> None:
        """Keeps a bottom margin under the last header row."""
        self._header.grid_rowconfigure(8, minsize=10)

    # -- states --------------------------------------------------------------------------

    def set_loading(self) -> None:
        """Marks a read in progress; what is shown stays until the new history arrives."""
        self.loading = True
        self.refresh_button.configure(state="disabled")
        if not self.loaded:
            self.summary_label.configure(text=LOADING_TEXT, text_color=theme.INK_MUTED)
        self._show_meta(LOADING_TEXT if self.loaded else "")
        self._header_bottom()

    def set_needs_admin(self) -> None:
        """Explains that the records need administrator rights; nothing was read."""
        self.loading = False
        self.summary_label.configure(text=PLACEHOLDER, text_color=theme.INK_MUTED)
        for name in self._header_rows:
            self._show_header_row(name, "")
        self._show_message(NEEDS_ADMIN_TEXT, action="elevate")
        self._show_meta("")
        self._show_cards(False)
        self.refresh_button.configure(state="disabled")
        self._header_bottom()

    def show(self, history: Mapping[str, Any]) -> None:
        """Shows `history`; malformed data is reported with `show_error`."""
        boots = history.get("boots") if isinstance(history, Mapping) else None
        if not isinstance(boots, list | tuple) or not isinstance(history.get("stats"), Mapping):
            self.show_error("the engine returned no boot history")
            return
        access = str(history.get("access", "ok"))
        if access == "needs_admin":
            self.set_needs_admin()
            self.loaded = True
            return
        self.history = history
        self.loading = False
        self.loaded = True
        self._enable_refresh()
        self._show_meta("")
        if access == "log_missing":
            self.summary_label.configure(text=LOG_MISSING_TEXT, text_color=theme.INK_SECONDARY)
            for name in self._header_rows:
                self._show_header_row(name, "")
            self._show_message("")
            self._show_cards(False)
            self._header_bottom()
            return
        boots = [b for b in boots if isinstance(b, Mapping)]
        stats = history["stats"]
        self.summary_label.configure(
            text=summary_text(history), text_color=theme.INK if boots else theme.INK_SECONDARY
        )
        trend, trend_color = trend_text(stats)
        self._show_header_row("trend", trend, trend_color)
        self._show_header_row(
            "fast", FAST_STARTUP_TEXT if history.get("fast_startup") is True and boots else ""
        )
        self._show_header_row("crash", unexpected_text(history))
        self._show_header_row("notice", "\n".join(f"⚠ {line}" for line in notice_lines(history)))
        if access == "log_disabled":
            self._show_message(LOG_DISABLED_TEXT, color=theme.INK_SECONDARY, action="event_viewer")
        else:
            self._show_message("")
        self._header_bottom()
        if not boots:
            self._show_cards(False)
            return
        self._show_cards(True)
        self.chart.render(boots, stats.get("median_ms"))
        self._build_views(history, boots)

    def _build_views(self, history: Mapping[str, Any], boots: list[Mapping[str, Any]]) -> None:
        for frame in self._view_frames.values():
            frame.destroy()
        self._view_frames = {}
        self.slow_cards = []
        shutdowns = [s for s in history.get("shutdowns") or [] if isinstance(s, Mapping)]
        entries = [e for e in history.get("startup_entries") or [] if isinstance(e, Mapping)]

        def add_cards(view: ctk.CTkFrame, items: list[Mapping[str, Any]]) -> None:
            for index, item in enumerate(items):
                card = SlowItemCard(
                    view,
                    item,
                    boots=len(boots),
                    shutdowns=len(shutdowns),
                    entries=entries,
                    on_turn_off=self._on_turn_off,
                    on_open_tool=self._on_open_tool,
                )
                card.set_busy(self._busy)
                card.grid(row=index, column=0, sticky="ew", pady=(0, 8))
                self.slow_cards.append(card)

        # What slowed starts; what slowed shutdowns is listed above the shutdowns.
        starting = slow_items(history, "startup")
        stopping = slow_items(history, "shutdown")
        slow = ctk.CTkFrame(self.list, fg_color="transparent")
        slow.grid_columnconfigure(0, weight=1)
        add_cards(slow, starting)
        if not starting:
            ctk.CTkLabel(
                slow, text="Nothing slowed the recorded starts.", font=_font(11), text_color=theme.INK_MUTED
            ).grid(row=0, column=0, sticky="w")
        self._view_frames["slow"] = slow

        starts = ctk.CTkFrame(self.list, fg_color="transparent")
        starts.grid_columnconfigure(0, weight=1)
        LineList(starts, [(boot_line(b), slowed_by(b)) for b in boots]).grid(row=0, column=0, sticky="ew")
        self._view_frames["starts"] = starts

        downs = ctk.CTkFrame(self.list, fg_color="transparent")
        downs.grid_columnconfigure(0, weight=1)
        add_cards(downs, stopping)
        if shutdowns:
            LineList(downs, [(shutdown_line(s), slowed_by(s)) for s in shutdowns]).grid(
                row=len(stopping), column=0, sticky="ew"
            )
        else:
            ctk.CTkLabel(
                downs, text="No shutdowns are recorded yet.", font=_font(11), text_color=theme.INK_MUTED
            ).grid(row=len(stopping), column=0, sticky="w")
        self._view_frames["shutdowns"] = downs

        self.view_selector.configure(values=[view_label(key, len(starting)) for key in VIEWS])
        self.select_view(self.view)

    def select_view(self, name: str) -> None:
        """Shows the bottom card's view `name` ("slow", "starts" or "shutdowns")."""
        if name not in VIEWS:
            raise ValueError(f"unknown view {name!r}")
        self.view = name
        count = len(slow_items(self.history or {}, "startup"))
        self.view_selector.set(view_label(name, count))
        for key, frame in self._view_frames.items():
            if key == name:
                frame.grid(row=0, column=0, sticky="ew")
            else:
                frame.grid_forget()

    def _view_clicked(self, value: str) -> None:
        for key in VIEWS:
            if value.startswith(VIEWS[key]):
                self.select_view(key)
                return

    def show_error(self, message: str) -> None:
        """Reports a failed read; an earlier history stays."""
        self.loading = False
        self._enable_refresh()
        self._show_meta("")
        self._show_message(f"Could not read the boot history: {message}", color=theme.CRITICAL)
        self._header_bottom()

    def set_unsupported(self, text: str) -> None:
        """Disables the section for an engine build without the boot history."""
        self._unsupported = True
        self.refresh_button.configure(state="disabled")
        self.summary_label.configure(text=f"⚠ {text}", text_color=theme.WARNING)
        self._show_cards(False)

    def _enable_refresh(self) -> None:
        self.refresh_button.configure(state="disabled" if self._unsupported else "normal")

    def set_actions_enabled(self, enabled: bool) -> None:
        """Enables or disables "Turn off at startup" (the window is busy)."""
        self._busy = not enabled
        for card in self.slow_cards:
            card.set_busy(self._busy)
