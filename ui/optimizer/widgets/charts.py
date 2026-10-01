"""Canvas charts drawn for a 60 Hz redraw budget.

Every item is created once; each frame only moves coordinates (`Canvas.coords`), which
keeps a redraw well under a millisecond. Gridlines and axes are solid hairlines one
shade off the surface; data marks are 2 px lines and thin bars separated by 2 px gaps.

Charts are drawn in the canvas's real size, whatever the layout gives them. Axis labels
that would overlap in a short chart are hidden instead: first the middle value label and
the time axis, then the remaining value labels.
"""

from __future__ import annotations

import tkinter as tk
from collections import deque
from collections.abc import Callable, Sequence

from .. import theme

LABEL_FONT = (theme.FONT_FAMILY, 9)
TOOLTIP_FONT = (theme.FONT_FAMILY, 9, "bold")


def canvas_size(canvas: tk.Canvas) -> tuple[int, int]:
    """The canvas's drawn size, or its requested size until it is first laid out."""
    width, height = canvas.winfo_width(), canvas.winfo_height()
    if width <= 1 or height <= 1:
        return canvas.winfo_reqwidth(), canvas.winfo_reqheight()
    return width, height


def line_height(widget: tk.Misc, font: tuple[str | int, ...]) -> int:
    """Height in pixels of one line of `font` on the widget's display."""
    return int(widget.tk.call("font", "metrics", font, "-linespace"))


class TimeSeriesChart(tk.Canvas):
    """Smoothly scrolling line + area chart of one series on a fixed 0..y_max axis.

    Samples are averaged into `bucket_s` buckets so a 60 s window holds a few hundred
    points; the newest point follows every sample, and x positions are recomputed from
    wall-clock time each frame, so the chart scrolls continuously rather than stepping.
    """

    PAD_LEFT = 34
    PAD_RIGHT = 10
    PAD_TOP = 8
    PAD_BOTTOM = 20

    def __init__(
        self,
        master: tk.Misc,
        *,
        color: str,
        fill: str,
        label: str,
        window_s: float = 60.0,
        bucket_s: float = 0.1,
        y_max: float = 100.0,
        height: int = 150,
        format_value: Callable[[float], str] = lambda v: f"{v:.0f}%",
    ) -> None:
        super().__init__(master, height=height, bg=theme.SURFACE, highlightthickness=0, bd=0)
        self.window_s = window_s
        self.bucket_s = bucket_s
        self.y_max = y_max
        self.label = label
        self.format_value = format_value
        self._points: deque[tuple[float, float]] = deque(maxlen=int(window_s / bucket_s) + 4)
        self._bucket_start: float | None = None
        self._bucket_sum = 0.0
        self._bucket_n = 0
        self._head: tuple[float, float] | None = None
        self._now = 0.0
        self._hover_x: float | None = None
        self._label_h = line_height(self, LABEL_FONT)

        self._grid = [self.create_line(0, 0, 0, 0, fill=theme.GRID, width=1) for _ in range(3)]
        self._baseline = self.create_line(0, 0, 0, 0, fill=theme.BASELINE, width=1)
        self._y_labels = [
            self.create_text(0, 0, text=t, fill=theme.INK_MUTED, font=LABEL_FONT, anchor="e")
            for t in (format_value(y_max), format_value(y_max / 2), format_value(0))
        ]
        self._x_labels = [
            self.create_text(0, 0, text=t, fill=theme.INK_MUTED, font=LABEL_FONT, anchor=a)
            for t, a in ((f"{window_s:.0f} s ago", "w"), (f"{window_s / 2:.0f} s", "center"), ("now", "e"))
        ]
        self._area = self.create_polygon(0, 0, 0, 0, 0, 0, fill=fill, outline="")
        self._line = self.create_line(0, 0, 0, 0, fill=color, width=2, capstyle="round", joinstyle="round")
        self._cross = self.create_line(0, 0, 0, 0, fill=theme.INK_MUTED, width=1, state="hidden")
        self._dot = self.create_oval(0, 0, 0, 0, fill=color, outline=theme.SURFACE, width=2, state="hidden")
        self._tip_bg = self.create_rectangle(
            0, 0, 0, 0, fill=theme.SURFACE_RAISED, outline=theme.BORDER, state="hidden"
        )
        self._tip = self.create_text(
            0, 0, text="", fill=theme.INK, font=TOOLTIP_FONT, anchor="nw", state="hidden"
        )

        self.bind("<Configure>", lambda _e: self._layout())
        self.bind("<Motion>", self._on_motion)
        self.bind("<Leave>", self._on_leave)

    # -- data ----------------------------------------------------------------------

    def push(self, t: float, value: float) -> None:
        if self._bucket_start is None:
            self._bucket_start = t
        if t - self._bucket_start >= self.bucket_s and self._bucket_n:
            self._points.append((self._bucket_start + self.bucket_s / 2, self._bucket_sum / self._bucket_n))
            self._bucket_start = t
            self._bucket_sum = 0.0
            self._bucket_n = 0
        self._bucket_sum += value
        self._bucket_n += 1
        self._head = (t, value)

    @property
    def point_count(self) -> int:
        return len(self._points) + (1 if self._head else 0)

    # -- geometry ------------------------------------------------------------------

    def _time_axis_shown(self, height: int) -> bool:
        """Whether the time labels fit under a plot tall enough for all three value labels."""
        return height - self.PAD_TOP - self.PAD_BOTTOM >= 2 * self._label_h

    def _plot(self) -> tuple[float, float, float, float]:
        w, h = canvas_size(self)
        # Without the time axis the bottom margin only has to hold half a value label.
        bottom = self.PAD_BOTTOM if self._time_axis_shown(h) else self.PAD_TOP
        x1 = max(w - self.PAD_RIGHT, self.PAD_LEFT + 1)
        y1 = max(h - bottom, self.PAD_TOP + 1)
        return self.PAD_LEFT, self.PAD_TOP, x1, y1

    @property
    def shown_labels(self) -> list[str]:
        """Texts of the axis labels currently drawn, value labels first."""
        return [
            str(self.itemcget(item, "text"))
            for item in self._y_labels + self._x_labels
            if self.itemcget(item, "state") != "hidden"
        ]

    def _layout(self) -> None:
        x0, y0, x1, y1 = self._plot()
        span = y1 - y0
        # Value labels are centred on their gridlines: the top and bottom ones need one label
        # height between them, the middle one needs one on each side.
        shown = (span >= self._label_h, span >= 2 * self._label_h, span >= self._label_h)
        for i, line in enumerate(self._grid):
            y = y0 + span * i / 2
            self.coords(line, x0, y, x1, y)
            self.coords(self._y_labels[i], x0 - 6, y)
            self.itemconfigure(self._y_labels[i], state="normal" if shown[i] else "hidden")
        self.coords(self._baseline, x0, y1, x1, y1)
        self.itemconfigure(self._grid[2], state="hidden")
        time_axis = "normal" if self._time_axis_shown(canvas_size(self)[1]) else "hidden"
        for label, x in zip(self._x_labels, (x0, (x0 + x1) / 2, x1), strict=True):
            self.coords(label, x, y1 + 11)
            self.itemconfigure(label, state=time_axis)

    def _xy(self, t: float, v: float, plot: tuple[float, float, float, float]) -> tuple[float, float]:
        x0, y0, x1, y1 = plot
        x = x1 - (self._now - t) / self.window_s * (x1 - x0)
        clamped = min(max(v, 0.0), self.y_max)
        y = y1 - clamped / self.y_max * (y1 - y0)
        return x, y

    def render(self, now: float) -> None:
        self._now = now
        plot = self._plot()
        x0, _, x1, y1 = plot
        pts = [p for p in self._points if now - p[0] <= self.window_s + self.bucket_s]
        if self._head is not None:
            pts.append(self._head)
        if len(pts) < 2:
            return
        flat: list[float] = []
        for t, v in pts:
            x, y = self._xy(t, v, plot)
            flat.extend((max(x, x0), y))
        self.coords(self._line, *flat)
        self.coords(self._area, flat[0], y1, *flat, flat[-2], y1)
        if self._hover_x is not None:
            self._update_hover(pts, plot)

    # -- hover ---------------------------------------------------------------------

    def _on_motion(self, event: tk.Event) -> None:
        x0, _, x1, _ = self._plot()
        self._hover_x = min(max(event.x, x0), x1)

    def _on_leave(self, _event: tk.Event) -> None:
        self._hover_x = None
        for item in (self._cross, self._dot, self._tip, self._tip_bg):
            self.itemconfigure(item, state="hidden")

    def _update_hover(
        self, pts: Sequence[tuple[float, float]], plot: tuple[float, float, float, float]
    ) -> None:
        x0, y0, x1, y1 = plot
        assert self._hover_x is not None
        best = min(pts, key=lambda p: abs(self._xy(p[0], p[1], plot)[0] - self._hover_x))
        x, y = self._xy(best[0], best[1], plot)
        if x < x0:
            return
        ago = self._now - best[0]
        text = f"{self.label} {self.format_value(best[1])}  ·  {'now' if ago < 0.5 else f'{ago:.0f} s ago'}"
        self.coords(self._cross, x, y0, x, y1)
        self.coords(self._dot, x - 5, y - 5, x + 5, y + 5)
        self.itemconfigure(self._tip, text=text)
        bbox = self.bbox(self._tip) or (0, 0, 0, 0)
        tw = bbox[2] - bbox[0]
        tx = x + 10 if x + 10 + tw + 8 < x1 else x - 10 - tw - 8
        self.coords(self._tip, tx + 4, y0 + 4)
        self.coords(self._tip_bg, tx, y0, tx + tw + 8, y0 + 20)
        for item in (self._cross, self._dot, self._tip_bg, self._tip):
            self.itemconfigure(item, state="normal")
            self.tag_raise(item)


class CoreBars(tk.Canvas):
    """One thin bar per logical processor on a shared 0..100 % axis, 2 px apart."""

    PAD_X = 4
    PAD_TOP = 6
    PAD_BOTTOM = 16
    GAP = 2

    def __init__(self, master: tk.Misc, *, color: str, height: int = 90) -> None:
        super().__init__(master, height=height, bg=theme.SURFACE, highlightthickness=0, bd=0)
        self._color = color
        self._bars: list[int] = []
        self._labels: list[int] = []
        self._values: list[tuple[float, int]] = []
        self._baseline = self.create_line(0, 0, 0, 0, fill=theme.BASELINE, width=1)
        self._tip_bg = self.create_rectangle(
            0, 0, 0, 0, fill=theme.SURFACE_RAISED, outline=theme.BORDER, state="hidden"
        )
        self._tip = self.create_text(
            0, 0, text="", fill=theme.INK, font=TOOLTIP_FONT, anchor="nw", state="hidden"
        )
        self._hover: int | None = None
        self.bind("<Motion>", self._on_motion)
        self.bind("<Leave>", self._on_leave)
        self.bind("<Configure>", lambda _e: self._layout_labels())

    def _ensure(self, n: int) -> None:
        while len(self._bars) < n:
            self._bars.append(self.create_rectangle(0, 0, 0, 0, fill=self._color, outline=""))
            self._labels.append(
                self.create_text(0, 0, text=str(len(self._labels)), fill=theme.INK_MUTED, font=LABEL_FONT)
            )
        while len(self._bars) > n:
            self.delete(self._bars.pop())
            self.delete(self._labels.pop())
        self._layout_labels()

    def _geometry(self, n: int) -> tuple[float, float, float, float]:
        w, h = canvas_size(self)
        w = max(w, 2 * self.PAD_X + 1)
        h = max(h, self.PAD_TOP + self.PAD_BOTTOM + 1)
        x0, x1 = self.PAD_X, w - self.PAD_X
        slot = (x1 - x0) / max(n, 1)
        return x0, slot, self.PAD_TOP, h - self.PAD_BOTTOM

    def _layout_labels(self) -> None:
        n = len(self._bars)
        if not n:
            return
        x0, slot, _, base = self._geometry(n)
        step = 1 if slot >= 16 else 2 if slot >= 8 else 4
        for i, label in enumerate(self._labels):
            self.coords(label, x0 + slot * (i + 0.5), base + 8)
            self.itemconfigure(label, state="normal" if i % step == 0 else "hidden")
        self.coords(self._baseline, x0, base, x0 + slot * n, base)

    def render(self, values: Sequence[tuple[float, int]]) -> None:
        """`values` holds (utilisation %, clock MHz) per core."""
        self._values = list(values)
        n = len(self._values)
        if n != len(self._bars):
            self._ensure(n)
        x0, slot, top, base = self._geometry(n)
        width = max(slot - self.GAP, 1)
        for i, (util, _mhz) in enumerate(self._values):
            left = x0 + slot * i + self.GAP / 2
            height = (base - top) * min(max(util, 0.0), 100.0) / 100.0
            self.coords(self._bars[i], left, base - max(height, 1), left + width, base)
        if self._hover is not None:
            self._show_tip(self._hover)

    def _on_motion(self, event: tk.Event) -> None:
        n = len(self._bars)
        if not n:
            return
        x0, slot, _, _ = self._geometry(n)
        index = int((event.x - x0) // slot)
        self._hover = index if 0 <= index < n else None
        if self._hover is None:
            self._on_leave(event)

    def _on_leave(self, _event: tk.Event) -> None:
        self._hover = None
        self.itemconfigure(self._tip, state="hidden")
        self.itemconfigure(self._tip_bg, state="hidden")

    def _show_tip(self, index: int) -> None:
        if index >= len(self._values):
            return
        util, mhz = self._values[index]
        clock = f"  ·  {mhz / 1000:.2f} GHz" if mhz else ""
        self.itemconfigure(self._tip, text=f"Core {index}: {util:.0f}%{clock}")
        x0, slot, top, _ = self._geometry(len(self._bars))
        bbox = self.bbox(self._tip) or (0, 0, 0, 0)
        tw = bbox[2] - bbox[0]
        x = x0 + slot * (index + 0.5)
        w = canvas_size(self)[0]
        tx = min(max(x - tw / 2 - 4, 2), w - tw - 10)
        self.coords(self._tip, tx + 4, top + 2)
        self.coords(self._tip_bg, tx, top - 2, tx + tw + 8, top + 18)
        for item in (self._tip_bg, self._tip):
            self.itemconfigure(item, state="normal")
            self.tag_raise(item)


class Meter(tk.Canvas):
    """Horizontal part-of-whole bar: filled share in the series colour on a hairline track."""

    def __init__(self, master: tk.Misc, *, color: str, height: int = 8) -> None:
        super().__init__(master, height=height, bg=theme.SURFACE, highlightthickness=0, bd=0)
        self._track = self.create_rectangle(0, 0, 0, 0, fill=theme.GRID, outline="")
        self._fill = self.create_rectangle(0, 0, 0, 0, fill=color, outline="")

    def render(self, fraction: float) -> None:
        w = max(self.winfo_width(), 10)
        h = max(self.winfo_height(), 4)
        f = min(max(fraction, 0.0), 1.0)
        self.coords(self._track, 0, 0, w, h)
        self.coords(self._fill, 0, 0, max(w * f, 1), h)
