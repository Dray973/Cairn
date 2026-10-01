"""Optimization controls: mode toggles and the scanned item lists."""

from __future__ import annotations

import tkinter as tk
from collections.abc import Callable, Sequence
from typing import Any

import customtkinter as ctk

from .. import theme

MODE_DESCRIPTIONS = {
    "privacy": "Limits Windows, Office and Edge diagnostic data and turns off the telemetry service "
    "and telemetry scheduled tasks, Activity History, Recall, the advertising ID, suggested apps and "
    "lock screen ads.",
    "gaming": "Turns off background game recording and the Sticky Keys prompt, turns on Game Mode "
    "and prioritises games in multimedia scheduling.",
    "performance": "Ultimate Performance power plan, SysMain and other background services set to "
    "manual, Edge kept from running in the background, faster menus.",
    "interface": "Shows file extensions, removes Widgets and adds End task to the taskbar menu.",
}

CATEGORY_HEADINGS = {
    "privacy": "Privacy and telemetry",
    "performance": "Performance and background services",
    "gaming": "Gaming",
    "interface": "Windows interface",
    "bloatware": "Bloatware apps",
}

# state -> (icon, label, colour)
STATE_STYLE = {
    "applied": ("✓", "Applied", theme.GOOD),
    "partial": ("◐", "Partly applied", theme.WARNING),
    "not_applied": ("○", "Not applied", theme.INK_MUTED),
    "unavailable": ("–", "Not on this PC", theme.INK_MUTED),
}

RESTART_TAGS = {
    "explorer": "Explorer restart",
    "sign_out": "sign-out needed",
    "restart": "restart needed",
}

# Prefix of the scan warning emitted when the Store package inventory cannot be read.
APPX_INVENTORY_WARNING = "Store package inventory unavailable"

# Mode status shown when a scan failed and no current scan describes the system.
STATUS_UNKNOWN = "Status unknown, scan again"

# Summary of an item list while a scan runs: what the engine reads.
SCANNING_TEXT = "Reading policies, services, scheduled tasks, Store apps and the power plan…"

# Wrap lengths in CustomTkinter units: the widest a line of text gets (row descriptions and
# caveats, the panel's summary and warnings, the empty-list message), and the narrowest it
# is made when the list is too narrow for the widest.
ROW_WRAP = 560
PANEL_WRAP = 700
EMPTY_WRAP = 640
MIN_WRAP = 200
# Wrap lengths follow the width in steps of this many units, so dragging the window edge
# reconfigures the labels every few pixels rather than on every pixel.
WRAP_STEP = 10
# Room kept free on the right of wrapped text, in CustomTkinter units.
WRAP_MARGIN = 8


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def fitted_wrap(available: float, widest: int) -> int:
    """Wrap length for text in `available` CustomTkinter units: `widest` when it fits,
    otherwise the available width rounded down to `WRAP_STEP`, but at least `MIN_WRAP`."""
    if available >= widest:
        return widest
    return max(MIN_WRAP, int(available) // WRAP_STEP * WRAP_STEP)


def recorded_recommended(items: Sequence[dict[str, Any]], category: str) -> int:
    """Recommended items of `category` that are applied and have journal records.

    An item whose value was already in place before Cairn ran scans as applied but is
    not revertible, so it is not counted. Scans without the `revertible` field count every
    applied item. The count only decides whether a mode's switch shows on; turning the mode
    off restores the records of every recommended item, whatever its scanned state.
    """
    return sum(
        1
        for i in items
        if i["category"] == category
        and i["recommended"]
        and i["state"] == "applied"
        and i.get("revertible", True)
    )


def route_warnings(
    report: dict[str, Any], categories: Sequence[str]
) -> tuple[dict[str, list[str]], list[str]]:
    """Splits the scan warnings that concern `categories` into per-item and panel lists.

    A warning that starts with `<item id>:` belongs to that item; the Store inventory
    warning belongs to the bloatware category; any other warning concerns every panel.
    Returns (item id -> messages without the id prefix, every warning for the panel).
    """
    category_of = {i["id"]: i["category"] for i in report.get("items", [])}
    per_item: dict[str, list[str]] = {}
    panel: list[str] = []
    for warning in report.get("warnings", []) or []:
        head, _, rest = warning.partition(":")
        head = head.strip()
        if head in category_of:
            if category_of[head] in categories:
                per_item.setdefault(head, []).append(rest.strip() or warning)
                panel.append(warning)
        elif warning.startswith(APPX_INVENTORY_WARNING):
            if "bloatware" in categories:
                panel.append(warning)
        else:
            panel.append(warning)
    return per_item, panel


class CategoryToggle(ctk.CTkFrame):
    """One optimization mode: name, switch, description and applied count.

    The switch shows on while every recommended change of the mode is in place, or while at
    least one recommended item is applied and has journal records. A partly applied mode
    without such an item (its values were already in place before Cairn ran) shows off,
    and flipping it applies the rest. Turning a mode off restores the records of all its
    recommended items, whatever their scanned state. A mode with some recommended changes
    applied also offers "Apply remaining".
    Flipping the switch does not change its position; the owner confirms first and then
    calls `set_status` with the scanned state, so the switch always reflects the system.
    """

    def __init__(
        self,
        master: tk.Misc,
        category: str,
        title: str,
        on_toggle: Callable[[str, bool], None],
        on_apply_remaining: Callable[[str], None] | None = None,
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.category = category
        self._on_toggle = on_toggle
        self._on_apply_remaining = on_apply_remaining
        self._on = False
        self._partial = False
        self._enabled = True
        self.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(self, text=title, font=_font(14, "bold"), text_color=theme.INK, anchor="w").grid(
            row=0, column=0, sticky="w", padx=12, pady=(10, 0)
        )
        self.switch = ctk.CTkSwitch(
            self,
            text="",
            width=46,
            switch_width=42,
            switch_height=22,
            progress_color=theme.ACCENT,
            button_color=theme.INK,
            button_hover_color=theme.INK_SECONDARY,
            fg_color=theme.BASELINE,
            command=self._flipped,
        )
        self.switch.grid(row=0, column=1, sticky="e", padx=(0, 6), pady=(10, 0))
        ctk.CTkLabel(
            self,
            text=MODE_DESCRIPTIONS.get(category, ""),
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=360,
        ).grid(row=1, column=0, columnspan=2, sticky="w", padx=12)
        self.status = ctk.CTkLabel(
            self, text="Scan to see status", font=_font(11), text_color=theme.INK_SECONDARY, anchor="w"
        )
        self._place_status(10)
        self.remaining_button = ctk.CTkButton(
            self,
            text="Apply remaining",
            width=120,
            height=24,
            font=_font(11),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            text_color=theme.INK,
            command=self._apply_remaining,
        )

    def _place_status(self, bottom: int) -> None:
        self.status.grid(row=2, column=0, columnspan=2, sticky="w", padx=12, pady=(2, bottom))

    @property
    def partial(self) -> bool:
        """Some, but not all, recommended changes of the mode are applied."""
        return self._partial

    def _flipped(self) -> None:
        desired = bool(self.switch.get())
        self._render_switch()
        self._on_toggle(self.category, desired)

    def _apply_remaining(self) -> None:
        if self._on_apply_remaining is not None:
            self._on_apply_remaining(self.category)

    def _render_switch(self) -> None:
        if self._on:
            self.switch.select()
        else:
            self.switch.deselect()

    def set_status(self, status: dict[str, Any] | None, recorded: int = 0, *, unknown: bool = False) -> None:
        """Shows the mode's scanned `status`, or a placeholder when it is None.

        `recorded` is the number of the mode's recommended items that are applied and
        recorded in the journal (see `recorded_recommended`). `unknown` selects the
        placeholder for a failed scan instead of the one for a mode not scanned yet.
        """
        if status is None:
            self._on = False
            self._partial = False
            if unknown:
                self.status.configure(text=STATUS_UNKNOWN, text_color=theme.WARNING)
            else:
                self.status.configure(text="Scan to see status", text_color=theme.INK_SECONDARY)
        else:
            active = bool(status["active"])
            done, total = status["recommended_applied"], status["recommended"]
            self._on = active or recorded > 0
            self._partial = not active and done > 0
            if active:
                self.status.configure(
                    text=f"✓ On  ·  all {total} recommended changes applied", text_color=theme.GOOD
                )
            elif done:
                self.status.configure(
                    text=f"◐ Partly on  ·  {done} of {total} recommended changes applied",
                    text_color=theme.WARNING,
                )
            else:
                self.status.configure(
                    text=f"○ Off  ·  {total} recommended changes available", text_color=theme.INK_SECONDARY
                )
        self._render_switch()
        # grid_forget rather than grid_remove: CustomTkinter replays the last grid call when
        # the display scaling changes, which would show a removed widget again.
        if self._partial and self._on_apply_remaining is not None:
            self._place_status(6)
            self.remaining_button.grid(row=3, column=0, sticky="w", padx=12, pady=(0, 10))
        else:
            self._place_status(10)
            self.remaining_button.grid_forget()
        self.set_enabled(self._enabled)

    def set_busy(self, text: str) -> None:
        self.status.configure(text=f"… {text}", text_color=theme.INK_SECONDARY)
        self.switch.configure(state="disabled")
        self.remaining_button.configure(state="disabled")

    def set_enabled(self, enabled: bool) -> None:
        self._enabled = enabled
        state = "normal" if enabled else "disabled"
        self.switch.configure(state=state)
        self.remaining_button.configure(state=state)


class ScanRow(ctk.CTkFrame):
    """One scanned item: state, tags, description, caveats and its Apply/Undo action.

    `warnings` are this scan's read errors for the item. An item that is applied but has
    nothing recorded in the journal (`revertible` false: the value was already in place)
    shows an "Already set" tag instead of an Undo button.

    The description and caveats wrap at `wraplength` CustomTkinter units; the owning list
    passes the width its text column has (see `TEXT_INSET`) and updates it with
    `set_wraplength` when the list is resized.
    """

    # Columns beside the text, in CustomTkinter units: the state icon and the action button
    # (or the "Already set" tag), each with its horizontal padding.
    ICON_WIDTH = 18
    ICON_PADX = (2, 6)
    ACTION_WIDTH = 74
    ACTION_PADX = (6, 4)
    # Horizontal padding of a row in its list.
    ROW_PADX = 6
    # Width of a list that its rows' text column does not get.
    TEXT_INSET = 2 * ROW_PADX + ICON_WIDTH + sum(ICON_PADX) + ACTION_WIDTH + sum(ACTION_PADX) + WRAP_MARGIN

    def __init__(
        self,
        master: tk.Misc,
        item: dict[str, Any],
        on_action: Callable[[dict[str, Any], str], None],
        mutations_enabled: bool,
        warnings: Sequence[str] = (),
        wraplength: int = ROW_WRAP,
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self.item = item
        self.action_button: ctk.CTkButton | None = None
        self.tag_label: ctk.CTkLabel | None = None
        self.note_label: ctk.CTkLabel | None = None
        self.warning_labels: list[ctk.CTkLabel] = []
        self._wraplength = wraplength
        self.grid_columnconfigure(1, weight=1)
        icon, label, color = STATE_STYLE.get(item["state"], STATE_STYLE["not_applied"])
        if item["state"] == "unavailable" and warnings:
            icon, label, color = "⚠", "Could not read", theme.WARNING
        ctk.CTkLabel(self, text=icon, font=_font(14, "bold"), text_color=color, width=self.ICON_WIDTH).grid(
            row=0, column=0, rowspan=2, sticky="n", padx=self.ICON_PADX, pady=(3, 0)
        )
        ctk.CTkLabel(self, text=item["title"], font=_font(12, "bold"), text_color=theme.INK, anchor="w").grid(
            row=0, column=1, sticky="w"
        )
        tags = [label]
        if item["recommended"]:
            tags.append("recommended")
        tags.append(f"{item['risk']} risk")
        if item["restart"] in RESTART_TAGS:
            tags.append(RESTART_TAGS[item["restart"]])
        risk_color = theme.SERIOUS if item["risk"] == "high" else theme.INK_MUTED
        ctk.CTkLabel(
            self,
            text=("▲ " if item["risk"] == "high" else "") + "  ·  ".join(tags),
            font=_font(10),
            text_color=risk_color,
            anchor="w",
        ).grid(row=1, column=1, sticky="w")
        self.description_label = ctk.CTkLabel(
            self,
            text=item["description"],
            font=_font(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=wraplength,
        )
        self.description_label.grid(row=2, column=1, sticky="w")
        row = 3
        note = item.get("note")
        if note:
            # An item that is not on this PC (its requirement is missing) says why in muted
            # text; every other note is a caveat.
            if item["state"] == "unavailable" and not warnings:
                self.note_label = self._caveat(f"– {note}", row, theme.INK_MUTED)
            else:
                self.note_label = self._caveat(f"⚠ {note}", row)
            row += 1
        for warning in warnings:
            self.warning_labels.append(self._caveat(f"⚠ {warning}", row))
            row += 1

        action = self.action_for(item)
        if action is not None:
            verb, kind = action
            self.action_button = ctk.CTkButton(
                self,
                text=verb,
                width=self.ACTION_WIDTH,
                height=26,
                font=_font(11),
                fg_color=theme.BUTTON_NEUTRAL,
                hover_color=theme.BUTTON_NEUTRAL_HOVER,
                text_color=theme.INK,
                state="normal" if mutations_enabled else "disabled",
                command=lambda: on_action(item, kind),
            )
            self.action_button.grid(row=0, column=2, rowspan=2, sticky="e", padx=self.ACTION_PADX)
        elif item["state"] == "applied":
            self.tag_label = ctk.CTkLabel(
                self, text="Already set", font=_font(11), text_color=theme.INK_MUTED, width=self.ACTION_WIDTH
            )
            self.tag_label.grid(row=0, column=2, rowspan=2, sticky="e", padx=self.ACTION_PADX)

    def _caveat(self, text: str, row: int, color: str = theme.WARNING) -> ctk.CTkLabel:
        label = ctk.CTkLabel(
            self,
            text=text,
            font=_font(10),
            text_color=color,
            anchor="w",
            justify="left",
            wraplength=self._wraplength,
        )
        label.grid(row=row, column=1, sticky="w")
        return label

    @property
    def wrapped_labels(self) -> list[ctk.CTkLabel]:
        """The description and every caveat: the labels that wrap to the text column."""
        caveats = [self.note_label] if self.note_label is not None else []
        return [self.description_label, *caveats, *self.warning_labels]

    def set_wraplength(self, wraplength: int) -> None:
        """Wraps the description and caveats at `wraplength` CustomTkinter units."""
        if wraplength == self._wraplength:
            return
        self._wraplength = wraplength
        for label in self.wrapped_labels:
            label.configure(wraplength=wraplength)

    @staticmethod
    def action_for(item: dict[str, Any]) -> tuple[str, str] | None:
        appx = item["kind"] == "appx"
        if item["state"] in ("not_applied", "partial"):
            return ("Remove" if appx else "Apply", "apply")
        # Undo needs a journal record; the field is absent from engines that predate it.
        if item["state"] == "applied" and item.get("revertible", True):
            return ("Restore" if appx else "Undo", "revert")
        return None


class ItemListPanel(ctk.CTkFrame):
    """Scanned items of some categories, grouped by category with an optional filter bar.

    Rows are built once per scan; the filter only shows or hides them. An optional bulk
    button hands the visible recommended, not-yet-applied items to `on_bulk`. Scan
    warnings that concern the panel's categories are shown under the summary, and read
    errors of single items on their rows.

    Wrapped text follows the panel's width: rows, the summary, the warnings and the empty
    message wrap at their usual lengths while those fit and at the width they have when the
    window is narrower.
    """

    # Width of the panel that the summary and warnings do not get (padding and margin).
    PANEL_TEXT_INSET = 2 * 14 + WRAP_MARGIN
    # Width of the list that the empty-list message does not get.
    EMPTY_TEXT_INSET = 2 * 8 + WRAP_MARGIN

    def __init__(
        self,
        master: tk.Misc,
        *,
        title: str,
        categories: Sequence[str],
        on_scan: Callable[[], None],
        on_action: Callable[[dict[str, Any], str], None],
        bulk_label: str | None = None,
        on_bulk: Callable[[list[dict[str, Any]]], None] | None = None,
        empty_text: str = "Nothing found.",
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self.categories = tuple(categories)
        self._on_action = on_action
        self._on_bulk = on_bulk
        self._empty_text = empty_text
        self._filter = "all"
        self.warnings: list[str] = []
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(4, weight=1)

        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            header, text=title, font=_font(13, "bold"), text_color=theme.INK_SECONDARY, anchor="w"
        ).grid(row=0, column=0, sticky="w")
        self.bulk_button: ctk.CTkButton | None = None
        if bulk_label:
            self.bulk_button = ctk.CTkButton(
                header,
                text=bulk_label,
                width=170,
                height=30,
                font=_font(12),
                fg_color=theme.BUTTON_NEUTRAL,
                hover_color=theme.BUTTON_NEUTRAL_HOVER,
                state="disabled",
                command=self._bulk,
            )
            self.bulk_button.grid(row=0, column=1, sticky="e", padx=(0, 8))
        self.scan_button = ctk.CTkButton(
            header,
            text="System Scan",
            width=120,
            height=30,
            font=_font(12, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            command=on_scan,
        )
        self.scan_button.grid(row=0, column=2, sticky="e")

        self.summary = ctk.CTkLabel(
            self,
            text="Not scanned yet",
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=PANEL_WRAP,
        )
        self.summary.grid(row=1, column=0, sticky="ew", padx=14, pady=(2, 4))
        self.warning_label = ctk.CTkLabel(
            self,
            text="",
            font=_font(11),
            text_color=theme.WARNING,
            anchor="w",
            justify="left",
            wraplength=PANEL_WRAP,
        )

        self.filter_bar: ctk.CTkSegmentedButton | None = None
        if len(self.categories) > 1:
            labels = ["All"] + [CATEGORY_HEADINGS[c].split(" ")[0] for c in self.categories]
            self._filter_values = dict(zip(labels, ["all", *self.categories], strict=True))
            self.filter_bar = ctk.CTkSegmentedButton(
                self,
                values=labels,
                font=_font(11),
                selected_color=theme.ACCENT,
                selected_hover_color=theme.ACCENT_HOVER,
                unselected_color=theme.SURFACE_RAISED,
                unselected_hover_color=theme.BUTTON_NEUTRAL_HOVER,
                command=self._filter_changed,
            )
            self.filter_bar.set("All")
            self.filter_bar.grid(row=3, column=0, sticky="w", padx=14, pady=(0, 6))

        self.list = ctk.CTkScrollableFrame(
            self,
            fg_color=theme.SURFACE,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.list.grid(row=4, column=0, sticky="nsew", padx=6, pady=(0, 8))
        self.list.grid_columnconfigure(0, weight=1)
        self._widgets: list[tuple[str, tk.Widget, dict[str, Any]]] = []
        self._rows: list[ScanRow] = []
        self.empty_label: ctk.CTkLabel | None = None

        # Wrap lengths for the current width, kept for the rows the next scan builds.
        self._panel_width = 0
        self._list_width = 0
        self.row_wrap = ROW_WRAP
        self._empty_wrap = EMPTY_WRAP
        self.bind("<Configure>", self._panel_resized)
        # The list's inner frame is as wide as the visible list; CustomTkinter binds its own
        # handler there, so this one is added next to it.
        self.list.bind("<Configure>", self._list_resized, add="+")

    @property
    def row_count(self) -> int:
        return len(self._rows)

    @property
    def rows(self) -> list[ScanRow]:
        return list(self._rows)

    def visible_items(self) -> list[dict[str, Any]]:
        return [r.item for r in self._rows if self._filter in ("all", r.item["category"])]

    def _units(self, pixels: int) -> float:
        """`pixels` in CustomTkinter units, which wrap lengths are given in."""
        return pixels / ctk.ScalingTracker.get_widget_scaling(self)

    def _panel_resized(self, event: tk.Event) -> None:
        if event.width == self._panel_width:
            return
        self._panel_width = event.width
        wrap = fitted_wrap(self._units(event.width) - self.PANEL_TEXT_INSET, PANEL_WRAP)
        for label in (self.summary, self.warning_label):
            if label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)

    def _list_resized(self, event: tk.Event) -> None:
        if event.width == self._list_width:
            return
        self._list_width = event.width
        width = self._units(event.width)
        self.row_wrap = fitted_wrap(width - ScanRow.TEXT_INSET, ROW_WRAP)
        self._empty_wrap = fitted_wrap(width - self.EMPTY_TEXT_INSET, EMPTY_WRAP)
        for row in self._rows:
            row.set_wraplength(self.row_wrap)
        if self.empty_label is not None and self.empty_label.cget("wraplength") != self._empty_wrap:
            self.empty_label.configure(wraplength=self._empty_wrap)

    def set_scanning(self) -> None:
        self.scan_button.configure(state="disabled", text="Scanning…")
        # A failed scan left the summary in the error colour.
        self.summary.configure(text=SCANNING_TEXT, text_color=theme.INK_MUTED)

    def set_idle(self) -> None:
        self.scan_button.configure(state="normal", text="System Scan")

    def set_scan_enabled(self, enabled: bool) -> None:
        self.scan_button.configure(state="normal" if enabled else "disabled")

    def show_error(self, message: str) -> None:
        self.set_idle()
        self.summary.configure(text=f"Scan failed: {message}", text_color=theme.CRITICAL)

    def _show_warnings(self, warnings: list[str]) -> None:
        self.warnings = warnings
        if not warnings:
            self.warning_label.configure(text="")
            # grid_forget: CustomTkinter replays the last grid call on a scaling change.
            self.warning_label.grid_forget()
            return
        shown = warnings[:3]
        lines = [f"⚠ {w}" for w in shown]
        if len(warnings) > len(shown):
            lines.append(f"⚠ …and {len(warnings) - len(shown)} more")
        self.warning_label.configure(text="\n".join(lines))
        self.warning_label.grid(row=2, column=0, sticky="ew", padx=14, pady=(0, 4))

    def show(self, report: dict[str, Any], mutations_enabled: bool, when: str) -> None:
        self.set_idle()
        for _, widget, _ in self._widgets:
            widget.destroy()
        self._widgets.clear()
        self._rows.clear()
        self.empty_label = None
        items = [i for i in report["items"] if i["category"] in self.categories]
        item_warnings, panel_warnings = route_warnings(report, self.categories)
        applied = sum(1 for i in items if i["state"] == "applied")
        pending = sum(1 for i in items if i["recommended"] and i["state"] in ("not_applied", "partial"))
        self.summary.configure(
            text=f"{when}  ·  {len(items)} items  ·  {applied} applied  ·  "
            f"{pending} recommended changes pending",
            text_color=theme.INK_MUTED,
        )
        self._show_warnings(panel_warnings)
        row = 0
        for category in self.categories:
            group = [i for i in items if i["category"] == category]
            if not group:
                continue
            heading = ctk.CTkLabel(
                self.list,
                text=CATEGORY_HEADINGS[category].upper(),
                font=_font(10, "bold"),
                text_color=theme.INK_MUTED,
                anchor="w",
            )
            self._widgets.append((category, heading, {"row": row, "pady": (10 if row else 4, 2), "padx": 8}))
            row += 1
            for item in group:
                widget = ScanRow(
                    self.list,
                    item,
                    self._on_action,
                    mutations_enabled,
                    item_warnings.get(item["id"], ()),
                    wraplength=self.row_wrap,
                )
                self._widgets.append((category, widget, {"row": row, "pady": 3, "padx": ScanRow.ROW_PADX}))
                self._rows.append(widget)
                row += 1
        if not items:
            # An empty list after a failed read is unknown, not "nothing installed". The
            # warning itself is shown above the list.
            if panel_warnings:
                text, color = "⚠ This list could not be read; see the warning above.", theme.WARNING
            else:
                text, color = self._empty_text, theme.INK_MUTED
            self.empty_label = ctk.CTkLabel(
                self.list,
                text=text,
                font=_font(11),
                text_color=color,
                justify="left",
                wraplength=self._empty_wrap,
            )
            self._widgets.append(("all", self.empty_label, {"row": 0, "pady": 20, "padx": 8}))
        self._apply_filter()
        if self.bulk_button is not None:
            self.bulk_button.configure(
                state="normal" if mutations_enabled and self._bulk_items() else "disabled"
            )

    def _bulk_items(self) -> list[dict[str, Any]]:
        return [
            i for i in self.visible_items() if i["recommended"] and i["state"] in ("not_applied", "partial")
        ]

    def _bulk(self) -> None:
        if self._on_bulk is not None:
            self._on_bulk(self._bulk_items())

    def _filter_changed(self, label: str) -> None:
        self._filter = self._filter_values.get(label, "all")
        self._apply_filter()

    def _apply_filter(self) -> None:
        for category, widget, grid in self._widgets:
            if self._filter in ("all", category) or category == "all":
                widget.grid(column=0, sticky="ew", **grid)
            else:
                # grid_forget: CustomTkinter replays the last grid call on a scaling change,
                # which would show a filtered-out row again.
                widget.grid_forget()
