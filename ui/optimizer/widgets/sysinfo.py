"""System section: a read-only snapshot of the hardware and Windows configuration.

A header card holds the one-line summary, when the snapshot was read and the copy and
refresh actions; below it, one card per section in two columns. The engine renders every
row (label, value, level, note, fraction), so this module only lays them out.
"""

from __future__ import annotations

import math
import re
import tkinter as tk
from collections.abc import Callable, Mapping, Sequence
from typing import Any

import customtkinter as ctk

from .. import theme
from .charts import Meter
from .history import local_time

# Column of each section card (0 left, 1 right). Within a column, cards follow this order.
SECTION_COLUMNS = {
    "windows": 0,
    "processor": 0,
    "memory": 0,
    "graphics": 0,
    "board": 1,
    "displays": 1,
    "storage": 1,
    "security": 1,
}
# Icon prefix and colour of a row value per level; "normal" values are plain.
LEVEL_STYLE = {"good": ("✓ ", theme.GOOD), "warning": ("⚠ ", theme.WARNING)}
PRIVACY_NOTE = "Read-only: nothing on this PC is changed. The copied text leaves out the computer name."
PLACEHOLDER = "Hardware and Windows details appear here."
LOADING_TEXT = "Reading system information…"

LABEL_WIDTH = 130
VALUE_WRAP = 300
GROUP_INDENT = 26
LABEL_GAP = 8
CARD_PAD = 14
# Characters after which a row label's word too wide for the label column may break.
LABEL_BREAKS = "-_/\\"


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def row_style(level: str | None) -> tuple[str, str]:
    """(icon prefix, text colour) of a row value with `level`; unknown levels read as normal."""
    return LEVEL_STYLE.get(level or "normal", ("", theme.INK))


def place_sections(ids: Sequence[str]) -> list[tuple[int, int]]:
    """(column, row) of each section card, in the order of `ids`.

    Known sections go to their `SECTION_COLUMNS` column in that table's order. Unknown ones
    follow, each in the column holding fewer cards at that point (the left one on a tie).
    """
    order = {section: index for index, section in enumerate(SECTION_COLUMNS)}
    positions: list[tuple[int, int]] = [(0, 0)] * len(ids)
    counts = [0, 0]
    known = sorted((i for i, s in enumerate(ids) if s in order), key=lambda i: (order[ids[i]], i))
    for i in known:
        column = SECTION_COLUMNS[ids[i]]
        positions[i] = (column, counts[column])
        counts[column] += 1
    for i, section in enumerate(ids):
        if section not in order:
            column = 0 if counts[0] <= counts[1] else 1
            positions[i] = (column, counts[column])
            counts[column] += 1
    return positions


def meta_text(snapshot: Mapping[str, Any]) -> str:
    """The header's meta line: when the snapshot was read and how long it took, then the
    privacy note. A read that rounds to 0.0 s took "under 0.1 s"."""
    info = snapshot.get("info") or {}
    parts: list[str] = []
    taken = info.get("taken_at") if isinstance(info, Mapping) else None
    if taken:
        read = f"Read {local_time(str(taken))}"
        duration = info.get("duration_ms")
        if isinstance(duration, int | float) and not isinstance(duration, bool) and 0 <= duration < math.inf:
            seconds = f"{duration / 1000:.1f}"
            read += " in under 0.1 s" if seconds == "0.0" else f" in {seconds} s"
        parts.append(read)
    parts.append(PRIVACY_NOTE)
    return "  ·  ".join(parts)


def _fraction(value: Any) -> float | None:
    """`value` as a share between 0 and 1; None for anything that is not a finite number."""
    if isinstance(value, bool) or not isinstance(value, int | float):
        return None
    try:
        share = float(value)
    except OverflowError:
        return None
    return min(max(share, 0.0), 1.0) if math.isfinite(share) else None


def _mappings(value: Any) -> list[Mapping[str, Any]]:
    """The mappings of a list; any other value, and any other item, is left out."""
    if not isinstance(value, list | tuple):
        return []
    return [item for item in value if isinstance(item, Mapping)]


def _break_word(word: str, fits: Callable[[str], bool]) -> str:
    """`word` split into lines after the last `LABEL_BREAKS` character of each line that
    still fits; what no such character can split stays whole."""
    lines: list[str] = []
    rest = word
    while not fits(rest):
        cut = max(
            (i + 1 for i, char in enumerate(rest[:-1]) if char in LABEL_BREAKS and fits(rest[: i + 1])),
            default=0,
        )
        if not cut:
            break
        lines.append(rest[:cut])
        rest = rest[cut:]
    lines.append(rest)
    return "\n".join(lines)


def break_long_words(text: str, fits: Callable[[str], bool]) -> str:
    """`text` with each word (a run without whitespace) that does not fit broken into lines
    after a '-', '_', '/' or '\\'.

    Tk wraps a label at whitespace and splits a word wider than the wrap length at any
    character ("Controller0-ChannelA-DI" / "MM1"); breaking at a separator first keeps the
    parts readable ("Controller0-ChannelA-" / "DIMM1"). A word without a separator that
    helps is left to Tk. `fits` tells whether a piece of text fits on one line.
    """
    if fits(text):
        return text
    return "".join(
        part if not part or part.isspace() else _break_word(part, fits) for part in re.split(r"(\s+)", text)
    )


class SectionCard(ctk.CTkFrame):
    """One snapshot section: its note or error, its rows, then one titled block per group.

    `rows` lists (label, value text as shown, with the level icon) in display order, group
    rows included; `row_labels` and `value_labels` hold the matching label and value
    widgets, and `meters` the (row label, meter) pairs of rows that carry a fraction. A
    label word too wide for the label column is broken at a separator (see
    `break_long_words`); `rows` keeps the label as the engine sent it.

    A snapshot has dozens of rows, so the text inside the card uses plain Tk labels with the
    font and sizes CustomTkinter would apply at the window's scaling; that halves the time a
    refresh holds the frame loop. CustomTkinter rescales only its own widgets, so the card
    lays its content out again when the scaling changes.
    """

    def __init__(self, master: tk.Misc, section: Mapping[str, Any]) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._section = section
        self._scale = ctk.ScalingTracker.get_widget_scaling(self)
        self.section_id = str(section.get("id") or "")
        self.rows: list[tuple[str, str]] = []
        self.row_labels: list[tk.Label] = []
        self.value_labels: list[tk.Label] = []
        self.meters: list[tuple[str, Meter]] = []
        self.note_label: tk.Label | None = None
        self.error_label: tk.Label | None = None
        self.title_label: tk.Label
        # Every widget of the current layout, replaced as a whole when the scaling changes.
        self._content: list[tk.Misc] = []
        self._next_row = 0
        self.grid_columnconfigure(1, weight=1)
        self._build()

    def _build(self) -> None:
        """Lays the section out at the current scaling."""
        section = self._section
        self.rows = []
        self.row_labels = []
        self.value_labels = []
        self.meters = []
        self.note_label = None
        self.error_label = None
        self._next_row = 0
        # Every label column has the same width, so values line up across cards.
        self.grid_columnconfigure(0, minsize=self._px(CARD_PAD + LABEL_WIDTH + LABEL_GAP))

        self.title_label = self._text(
            str(section.get("title") or self.section_id), 13, theme.INK_SECONDARY, weight="bold"
        )
        self._place(self.title_label, pady=(10, 4))
        note = section.get("note")
        if note:
            self.note_label = self._text(str(note), 10, theme.INK_MUTED, wrap=LABEL_WIDTH + VALUE_WRAP)
            self._place(self.note_label, pady=(0, 4))
        error = section.get("error")
        if error:
            self.error_label = self._text(
                f"⚠ Could not read: {error}", 10, theme.WARNING, wrap=LABEL_WIDTH + VALUE_WRAP
            )
            self._place(self.error_label, pady=(0, 4))
        for row in _mappings(section.get("rows")):
            self._add_row(row, indent=0)
        for group in _mappings(section.get("groups")):
            title = self._text(str(group.get("title") or ""), 12, theme.INK, weight="bold")
            self._place(title, pady=(6, 2))
            for row in _mappings(group.get("rows")):
                self._add_row(row, indent=GROUP_INDENT)
        # Bottom margin below the last line.
        self.grid_rowconfigure(self._next_row, minsize=self._px(10))

    def _set_scaling(self, new_widget_scaling: float, new_window_scaling: float) -> None:
        super()._set_scaling(new_widget_scaling, new_window_scaling)
        if new_widget_scaling == self._scale:
            return
        self._scale = new_widget_scaling
        for widget in self._content:
            widget.destroy()
        self._content = []
        # The same section gives the same rows, so every grid row is configured again.
        self._build()

    def _px(self, value: float) -> int:
        """`value` logical pixels at the window's scaling."""
        return round(value * self._scale)

    def _pad(self, pad: int | tuple[int, int]) -> int | tuple[int, int]:
        return self._px(pad) if isinstance(pad, int) else (self._px(pad[0]), self._px(pad[1]))

    def _text(self, text: str, size: int, color: str, *, weight: str = "normal", wrap: int = 0) -> tk.Label:
        """A label in the theme font at `size` logical pixels, wrapped at `wrap` (0: no wrap)."""
        label = tk.Label(
            self,
            text=text,
            font=(theme.FONT_FAMILY, -self._px(size), weight),
            fg=color,
            bg=theme.SURFACE,
            anchor="nw",
            justify="left",
            wraplength=self._px(wrap),
            bd=0,
            padx=0,
            pady=0,
            highlightthickness=0,
        )
        self._content.append(label)
        return label

    def _fits(self, text: str, size: int, wrap: int) -> bool:
        """Whether `text` in the theme font at `size` logical pixels fits in `wrap` logical
        pixels, measured at the window's scaling as the label draws it."""
        font = (theme.FONT_FAMILY, -self._px(size), "normal")
        return int(self.tk.call("font", "measure", font, text)) <= self._px(wrap)

    def _place(self, widget: tk.Misc, *, pady: int | tuple[int, int] = 0) -> None:
        """Grids `widget` across both columns on the next row."""
        widget.grid(
            row=self._next_row,
            column=0,
            columnspan=2,
            sticky="ew",
            padx=self._px(CARD_PAD),
            pady=self._pad(pady),
        )
        self._next_row += 1

    def _in_value_column(self, widget: tk.Misc, *, sticky: str, pady: int | tuple[int, int]) -> None:
        widget.grid(
            row=self._next_row, column=1, sticky=sticky, padx=(0, self._px(CARD_PAD)), pady=self._pad(pady)
        )
        self._next_row += 1

    def _add_row(self, row: Mapping[str, Any], *, indent: int) -> None:
        label = str(row.get("label") or "")
        level = row.get("level")
        prefix, color = row_style(level if isinstance(level, str) else None)
        raw = row.get("value")
        value = prefix + ("" if raw is None else str(raw))

        wrap = LABEL_WIDTH - indent
        shown = break_long_words(label, lambda text: self._fits(text, 11, wrap))
        row_label = self._text(shown, 11, theme.INK_MUTED, wrap=wrap)
        row_label.grid(
            row=self._next_row,
            column=0,
            sticky="nw",
            padx=(self._px(CARD_PAD + indent), self._px(LABEL_GAP)),
            pady=self._pad(2),
        )
        value_label = self._text(value, 11, color, wrap=VALUE_WRAP)
        self._in_value_column(value_label, sticky="nw", pady=2)
        self.rows.append((label, value))
        self.row_labels.append(row_label)
        self.value_labels.append(value_label)

        fraction = _fraction(row.get("fraction"))
        if fraction is not None:
            meter = Meter(
                self, color=theme.WARNING if level == "warning" else theme.ACCENT, height=self._px(6)
            )
            # The meter takes the width of the value column; a canvas asks for 10 cm by default,
            # which would make the card ask for more than its column.
            meter.configure(width=1)
            self._content.append(meter)
            self._in_value_column(meter, sticky="ew", pady=(1, 3))
            meter.bind("<Configure>", lambda _e, m=meter, f=fraction: m.render(f))
            meter.render(fraction)
            self.meters.append((label, meter))

        note = row.get("note")
        if note:
            self._in_value_column(
                self._text(str(note), 10, theme.INK_MUTED, wrap=VALUE_WRAP), sticky="nw", pady=(0, 3)
            )


class SystemPanel(ctk.CTkFrame):
    """Header card with the summary and actions above the section cards in two columns.

    The first snapshot loads when the section is first shown; Refresh reads a new one while the
    current cards stay visible. `text` is the snapshot's plain-text form, which "Copy as
    text" hands to `on_copy`.
    """

    def __init__(
        self,
        master: tk.Misc,
        on_refresh: Callable[[], None],
        on_copy: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_copy = on_copy
        self.loaded = False
        self.loading = False
        self.text = ""
        self.cards: dict[str, SectionCard] = {}
        self._meta = ""
        self._unsupported = False
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(1, weight=1)

        header = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        header.grid(row=0, column=0, sticky="ew", pady=(0, 8))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            header, text="This PC", font=_font(13, "bold"), text_color=theme.INK_SECONDARY, anchor="w"
        ).grid(row=0, column=0, sticky="w", padx=CARD_PAD, pady=(10, 0))
        buttons = ctk.CTkFrame(header, fg_color="transparent")
        buttons.grid(row=0, column=1, rowspan=3, sticky="ne", padx=CARD_PAD, pady=10)
        self.copy_button = ctk.CTkButton(
            buttons,
            text="Copy as text",
            width=120,
            height=30,
            font=_font(12, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            state="disabled",
            command=self._copy,
        )
        self.copy_button.pack(side="left", padx=(0, 8))
        self.refresh_button = ctk.CTkButton(
            buttons,
            text="Refresh",
            width=100,
            height=30,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_refresh,
        )
        self.refresh_button.pack(side="left")
        self.summary = ctk.CTkLabel(
            header,
            text=PLACEHOLDER,
            font=_font(12, "bold"),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=820,
        )
        self.summary.grid(row=1, column=0, sticky="w", padx=CARD_PAD, pady=(2, 0))
        self.meta = ctk.CTkLabel(
            header, text="", font=_font(10), text_color=theme.INK_MUTED, anchor="w", justify="left"
        )
        self.meta.grid(row=2, column=0, sticky="w", padx=CARD_PAD, pady=(0, 8))

        self.body = ctk.CTkScrollableFrame(
            self,
            fg_color="transparent",
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.body.grid(row=1, column=0, sticky="nsew")
        self.body.grid_columnconfigure((0, 1), weight=1, uniform="sysinfo")
        self._columns: list[ctk.CTkFrame] = []
        for column in range(2):
            frame = ctk.CTkFrame(self.body, fg_color="transparent")
            frame.grid(row=0, column=column, sticky="new", padx=(0, 5) if column == 0 else (5, 0))
            frame.grid_columnconfigure(0, weight=1)
            self._columns.append(frame)

    @property
    def section_count(self) -> int:
        return len(self.cards)

    def set_loading(self) -> None:
        """Marks a read in progress; the current cards stay until the new snapshot arrives."""
        self.loading = True
        self.refresh_button.configure(state="disabled")
        self.copy_button.configure(state="disabled")
        self.meta.configure(text=LOADING_TEXT)

    def show(self, snapshot: Mapping[str, Any]) -> None:
        """Replaces the cards with the sections of `snapshot`."""
        listed = snapshot.get("sections") if isinstance(snapshot, Mapping) else None
        if not isinstance(listed, list | tuple):
            self.show_error("the engine returned no sections")
            return
        sections = _mappings(listed)
        self.loading = False
        self.loaded = True
        self.text = str(snapshot.get("text") or "")
        self.summary.configure(text=str(snapshot.get("summary") or "–"), text_color=theme.INK)
        self._meta = meta_text(snapshot)
        self.meta.configure(text=self._meta)
        # Every widget in the columns goes, including a card whose id a later one repeated.
        for column_frame in self._columns:
            for child in column_frame.winfo_children():
                child.destroy()
        self.cards = {}
        ids = [str(s.get("id") or "") for s in sections]
        for section, (column, row) in zip(sections, place_sections(ids), strict=True):
            card = SectionCard(self._columns[column], section)
            card.grid(row=row, column=0, sticky="ew", pady=(0, 10))
            self.cards[card.section_id] = card
        self._enable_actions()

    def show_error(self, message: str) -> None:
        """Reports a failed read; cards and text of an earlier snapshot stay available."""
        self.loading = False
        self.summary.configure(
            text=f"Could not read system information: {message}", text_color=theme.CRITICAL
        )
        self.meta.configure(text=self._meta)
        self._enable_actions()

    def set_unsupported(self, text: str) -> None:
        """Disables the section for an engine build without system information."""
        self._unsupported = True
        self.refresh_button.configure(state="disabled")
        self.copy_button.configure(state="disabled")
        self.summary.configure(text=f"⚠ {text}", text_color=theme.WARNING)
        self.meta.configure(text="")

    def _enable_actions(self) -> None:
        self.refresh_button.configure(state="disabled" if self._unsupported else "normal")
        self.copy_button.configure(state="normal" if self.text and not self._unsupported else "disabled")

    def _copy(self) -> None:
        if self.text:
            self._on_copy(self.text)
