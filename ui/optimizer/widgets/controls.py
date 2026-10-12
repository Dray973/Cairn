"""Optimization controls: mode toggles and the scanned item lists."""

from __future__ import annotations

import tkinter as tk
from collections.abc import Callable, Mapping, Sequence
from typing import Any

import customtkinter as ctk

from .. import theme
from .search import SearchEntry, matches, search_terms

MODE_DESCRIPTIONS = {
    "privacy": "Limits Windows and Edge diagnostic data and feedback, turns off the telemetry service "
    "and telemetry scheduled tasks, the advertising ID, suggested apps, lock screen ads and "
    "notifications, and the tips and setup reminders in Start and Settings.",
    "gaming": "Turns on Game Mode.",
    "performance": "Ultimate Performance power plan, SysMain and other background services set to "
    "manual, Edge kept from running in the background, faster menus.",
    "interface": "Shows file extensions, adds End task to the taskbar menu, hides sync app notices in "
    "File Explorer and stops Edge's new tab page news.",
}

CATEGORY_HEADINGS = {
    "privacy": "Privacy and telemetry",
    "performance": "Performance and background services",
    "gaming": "Gaming",
    "interface": "Windows interface",
    "security": "Security",
    "bloatware": "Bloatware apps",
}

# Why a scanned tweak cannot be used on this PC, by the engine's `unavailable` kind.
UNAVAILABLE_LABELS = {
    "not_on_this_pc": "Not on this PC",
    "windows_version": "Not on this version of Windows",
    "edition": "Not on this edition of Windows",
    "retired": "Retired",
}

# Topic ids as the engine serializes them -> labels (Topic::label in catalog.rs).
TOPIC_LABELS = {
    "telemetry": "Diagnostics and feedback",
    "ads": "Ads and suggestions",
    "activity": "Activity and personal data",
    "ai": "AI features",
    "search": "Search",
    "start": "Start, lock screen and notifications",
    "sync": "Clipboard, sync and phone",
    "edge": "Microsoft Edge",
    "office": "Microsoft Office",
    "security": "Security",
    "updates": "Windows Update",
    "services": "Background services",
    "power": "Power and startup",
    "visuals": "Look and feel",
    "taskbar": "Taskbar",
    "explorer": "File Explorer",
    "files": "Files and storage",
    "input": "Mouse and keyboard",
    "games": "Games",
    "network": "Network",
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

# The search row of a searchable list: the box's placeholder and the Show menu's width.
SEARCH_PLACEHOLDER = "Search tweaks  (Ctrl+F)"
SHOW_MENU_WIDTH = 160
# Show menu choices as (label, value), in menu order, and the choice a list starts with.
SHOW_CHOICES = (
    ("Available here", "available"),
    ("Changed since applied", "changed"),
    ("Not available here", "unavailable"),
    ("All tweaks", "all"),
)
SHOW_DEFAULT = "available"
# What an empty list says when a search or a Show choice leaves no row. A search whose matches
# the filters hide says so instead of denying them, and when its only matches are tweaks this
# PC cannot use, which "Available here" hides, it says how to list them. The Show texts speak
# only for the tweaks of the category chosen in the filter bar: the scan reads no startup
# entry, DNS server or other recorded change, which History checks.
NO_MATCH_TEXT = 'No tweak matches "{query}".'
NO_MATCH_HIDDEN_TEXT = 'Only tweaks that the filters hide match "{query}".'
NO_MATCH_UNUSABLE_ONE = (
    'No tweak available here matches "{query}", but 1 tweak this PC cannot use does. Choose All '
    "tweaks instead of Available here to see it."
)
NO_MATCH_UNUSABLE_MANY = (
    'No tweak available here matches "{query}", but {n} tweaks this PC cannot use do. Choose All '
    "tweaks instead of Available here to see them."
)
NOTHING_CHANGED_TEXT = "✓ No tweak Cairn applied has been changed since."
NOTHING_CHANGED_IN_CATEGORY_TEXT = "✓ No tweak Cairn applied in this category has been changed since."
ALL_USABLE_TEXT = "Every tweak can be used on this PC."
ALL_USABLE_IN_CATEGORY_TEXT = "Every tweak in this category can be used on this PC."
# "Changed since applied" with nothing listed while the scan could not read a recorded value of
# some tweaks in the chosen category: only what it read is said to be unchanged.
NOTHING_CHANGED_UNREAD_ONE = (
    "No tweak Cairn applied that the scan could read has been changed since. The scan could not read "
    "1 tweak Cairn applied; see the warnings above."
)
NOTHING_CHANGED_UNREAD_MANY = (
    "No tweak Cairn applied that the scan could read has been changed since. The scan could not read "
    "{n} tweaks Cairn applied; see the warnings above."
)
# Caveat of a value an earlier version of Cairn set for a tweak that this version no longer sets.
FORMER_CAVEAT = (
    "⚠ Set by an earlier version of Cairn: {target}. {reason} Apply this tweak to restore it to "
    "{original}; Undo in History restores everything."
)
# The status line of a mode: every recommended change applied, some of them, or none.
MODE_ON_ONE = "✓ On  ·  1 recommended change applied"
MODE_ON_MANY = "✓ On  ·  all {total} recommended changes applied"
MODE_PARTLY = "◐ Partly on  ·  {done} of {total} recommended changes applied"
MODE_OFF_ONE = "○ Off  ·  1 recommended change available"
MODE_OFF_MANY = "○ Off  ·  {total} recommended changes available"
# The part of the tweak list's summary that counts the recommended changes not applied yet.
PENDING_ONE = "1 recommended change pending"
PENDING_MANY = "{n} recommended changes pending"

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


def topic_label(item: Mapping[str, Any]) -> str:
    """Label of the item's topic; empty for an item without one (a Store package)."""
    return TOPIC_LABELS.get(str(item.get("topic") or ""), "")


def item_matches(item: Mapping[str, Any], terms: Sequence[str]) -> bool:
    """Whether every search term (`search.search_terms`) starts a word of the item's title,
    description, id or topic label; no terms match every item."""
    fields = (
        str(item.get("title") or ""),
        str(item.get("description") or ""),
        str(item.get("id") or ""),
        topic_label(item),
    )
    return matches(fields, terms)


def never_shown(item: Mapping[str, Any]) -> bool:
    """A retired tweak without journal records: it offers nothing to apply or undo, so no
    Show choice lists it."""
    return item.get("unavailable") == "retired" and not item.get("revertible")


def item_changed(item: Mapping[str, Any]) -> bool:
    """Whether a recorded value of the item no longer holds: the scan lists its drift, or read a
    recorded target as not held (`held` false), which is all a retired tweak's row says, since
    it lists no drift."""
    return bool(item.get("drift")) or any(a.get("held") is False for a in item.get("actions") or [])


def item_unread(item: Mapping[str, Any], warnings: Sequence[str]) -> bool:
    """Whether the scan could not read every recorded value of the item: it has journal records,
    read warnings and an action the scan could not read."""
    return (
        bool(item.get("revertible"))
        and bool(warnings)
        and any(a.get("state") == "unavailable" for a in item.get("actions") or [])
    )


def item_shown(item: Mapping[str, Any], show: str) -> bool:
    """Whether the Show choice `show` (a value of `SHOW_CHOICES`) includes `item`.

    Items this PC can use are "available"; so are items it cannot use that have journal
    records, because they offer Undo. "changed" lists the items whose recorded values no
    longer hold (`item_changed`), retired ones included, "unavailable" the items this PC
    cannot use, and "all" every item; a retired tweak without records is never listed
    (`never_shown`)."""
    if never_shown(item):
        return False
    unavailable = item.get("unavailable") is not None
    if show == "available":
        return not unavailable or bool(item.get("revertible"))
    if show == "changed":
        return item_changed(item)
    if show == "unavailable":
        return unavailable
    return True


def former_caveat(record: Mapping[str, Any]) -> str:
    """The caveat of a value an earlier version of Cairn set (a scan item's `former` entry)."""
    return FORMER_CAVEAT.format(
        target=record.get("target", ""), reason=record.get("reason", ""), original=record.get("original", "")
    )


def mode_status(active: bool, done: int, total: int) -> tuple[str, str]:
    """A mode's status line and its colour: on when `active`, partly on when `done` of its
    `total` recommended changes are applied, otherwise off."""
    if active:
        return (MODE_ON_ONE if total == 1 else MODE_ON_MANY.format(total=total)), theme.GOOD
    if done:
        return MODE_PARTLY.format(done=done, total=total), theme.WARNING
    return (MODE_OFF_ONE if total == 1 else MODE_OFF_MANY.format(total=total)), theme.INK_SECONDARY


def pending_text(count: int) -> str:
    """ "1 recommended change pending" or "12 recommended changes pending"."""
    return PENDING_ONE if count == 1 else PENDING_MANY.format(n=count)


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
            text, color = mode_status(active, done, total)
            self.status.configure(text=text, text_color=color)
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
    shows an "Already set" tag instead of an Undo button. An item this PC cannot use
    (`unavailable`) shows the reason's label and the scan's note in muted text, and offers
    only Undo, when it has journal records.

    The caveats under the description are, in order: the scan's note, one line per value an
    earlier version of Cairn set for the tweak (`former`), what Undo cannot bring back
    (`undo_limit`) and the read errors. The description and caveats wrap at `wraplength`
    CustomTkinter units; the owning list passes the width its text column has (see
    `TEXT_INSET`) and updates it with `set_wraplength` when the list is resized.
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
        self.former_labels: list[ctk.CTkLabel] = []
        self.undo_limit_label: ctk.CTkLabel | None = None
        self.warning_labels: list[ctk.CTkLabel] = []
        self._wraplength = wraplength
        self.grid_columnconfigure(1, weight=1)
        unavailable = item.get("unavailable")
        icon, label, color = STATE_STYLE.get(item["state"], STATE_STYLE["not_applied"])
        if unavailable:
            icon, label, color = "–", UNAVAILABLE_LABELS.get(unavailable, label), theme.INK_MUTED
        elif item["state"] == "unavailable" and warnings:
            icon, label, color = "⚠", "Could not read", theme.WARNING
        ctk.CTkLabel(self, text=icon, font=_font(14, "bold"), text_color=color, width=self.ICON_WIDTH).grid(
            row=0, column=0, rowspan=2, sticky="n", padx=self.ICON_PADX, pady=(3, 0)
        )
        ctk.CTkLabel(self, text=item["title"], font=_font(12, "bold"), text_color=theme.INK, anchor="w").grid(
            row=0, column=1, sticky="w"
        )
        tags = [label]
        # A tweak this PC cannot use is not offered, whatever its catalog recommends.
        if item["recommended"] and not unavailable:
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
            # Why an item cannot be used on this PC is said in muted text; every other note
            # is a caveat.
            if unavailable or (item["state"] == "unavailable" and not warnings):
                self.note_label = self._caveat(f"– {note}", row, theme.INK_MUTED)
            else:
                self.note_label = self._caveat(f"⚠ {note}", row)
            row += 1
        if not unavailable:
            # Both concern applying the tweak, which an unavailable item does not offer.
            for record in item.get("former") or []:
                self.former_labels.append(self._caveat(former_caveat(record), row))
                row += 1
            limit = item.get("undo_limit")
            if limit:
                self.undo_limit_label = self._caveat(f"⚠ {limit}", row)
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
        elif item["state"] == "applied" and not unavailable:
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
        caveats += self.former_labels
        if self.undo_limit_label is not None:
            caveats.append(self.undo_limit_label)
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
        """(button text, action) the item offers, or None.

        An item this PC cannot use offers only Undo, and only while it has journal records. A
        tweak with values an earlier version of Cairn set offers Apply, which restores them.
        Otherwise the state decides.
        """
        if item.get("unavailable"):
            return ("Undo", "revert") if item.get("revertible") else None
        appx = item["kind"] == "appx"
        if item.get("former"):
            return ("Apply", "apply")
        if item["state"] in ("not_applied", "partial"):
            return ("Remove" if appx else "Apply", "apply")
        # Undo needs a journal record; the field is absent from engines that predate it.
        if item["state"] == "applied" and item.get("revertible", True):
            return ("Restore" if appx else "Undo", "revert")
        return None


class _Slot:
    """A widget of the list with its grid options, its category, its item (None for a heading
    or a message) and whether it is gridded."""

    __slots__ = ("widget", "category", "item", "grid", "shown")

    def __init__(
        self, widget: tk.Widget, category: str, item: dict[str, Any] | None, grid: dict[str, Any]
    ) -> None:
        self.widget = widget
        self.category = category
        self.item = item
        self.grid = grid
        self.shown = False


class ItemListPanel(ctk.CTkFrame):
    """Scanned items of some categories, grouped by category with an optional filter bar.

    Rows are built once per scan; the filters only show or hide them, and a filter change
    grids or forgets only the rows whose visibility changed. An optional bulk button hands the
    visible recommended, not-yet-applied items to `on_bulk`. Scan warnings that concern the
    panel's categories are shown under the summary, and read errors of single items on their
    rows. A retired tweak without journal records gets no row.

    With `search`, a row under the filter bar holds a search box and a Show menu, and each
    category heading collapses or expands its rows when clicked. A row is shown when its
    category passes the filter bar, the Show choice includes it (`item_shown`), it matches
    every search word (`item_matches`) and its heading is expanded. A search shows every
    category that has matches without changing which headings are collapsed; clearing it
    brings that state back. The search, the Show choice and the collapsed headings stay
    across scans. Headings count their rows, and the summary says how many rows the filters
    show; a list the filters leave empty says why, without denying the matches they hide.

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
        search: bool = False,
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self.categories = tuple(categories)
        self._on_action = on_action
        self._on_bulk = on_bulk
        self._empty_text = empty_text
        self._filter = "all"
        self._searchable = search
        self._query = ""
        self._terms: list[str] = []
        self._show = SHOW_DEFAULT if search else "all"
        # Collapsed categories, and those collapsed while the current search is shown.
        self._collapsed: set[str] = set()
        self._search_collapsed: set[str] = set()
        self.warnings: list[str] = []
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(5, weight=1)

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

        self.search_entry: SearchEntry | None = None
        self.show_menu: ctk.CTkOptionMenu | None = None
        self._show_values = {label: value for label, value in SHOW_CHOICES}
        if search:
            search_row = ctk.CTkFrame(self, fg_color="transparent")
            search_row.grid(row=4, column=0, sticky="ew", padx=14, pady=(0, 6))
            search_row.grid_columnconfigure(0, weight=1)
            self.search_entry = SearchEntry(
                search_row, placeholder=SEARCH_PLACEHOLDER, on_change=self._query_changed
            )
            self.search_entry.grid(row=0, column=0, sticky="ew")
            self.show_menu = ctk.CTkOptionMenu(
                search_row,
                values=[label for label, _ in SHOW_CHOICES],
                width=SHOW_MENU_WIDTH,
                height=28,
                dynamic_resizing=False,
                font=_font(11),
                fg_color=theme.BUTTON_NEUTRAL,
                button_color=theme.BASELINE,
                button_hover_color=theme.INK_MUTED,
                text_color=theme.INK,
                dropdown_fg_color=theme.SURFACE_RAISED,
                dropdown_hover_color=theme.BUTTON_NEUTRAL_HOVER,
                dropdown_text_color=theme.INK,
                dropdown_font=_font(11),
                command=self._show_menu_changed,
            )
            self.show_menu.set(self._show_label(self._show))
            self.show_menu.grid(row=0, column=1, sticky="e", padx=(8, 0))

        self.list = ctk.CTkScrollableFrame(
            self,
            fg_color=theme.SURFACE,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.list.grid(row=5, column=0, sticky="nsew", padx=6, pady=(0, 8))
        self.list.grid_columnconfigure(0, weight=1)
        self._slots: list[_Slot] = []
        self._rows: list[ScanRow] = []
        self._headings: dict[str, ctk.CTkButton | ctk.CTkLabel] = {}
        # The last scan's items that get a row, the summary's scan time, and whether the summary
        # says something else (a scan running or failed) that filtering must not replace.
        self._items: list[dict[str, Any]] = []
        self._when = ""
        self._summary_held = False
        # Recorded items whose values the last scan could not all read.
        self._unread_ids: set[str] = set()
        self.empty_label: ctk.CTkLabel | None = None
        # The message of a list whose items the filters all hide, and whether it is gridded.
        self._filtered_label: ctk.CTkLabel | None = None
        self._filtered_shown = False

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

    @property
    def query(self) -> str:
        """The search box's text ("" without a search box)."""
        return self._query

    @property
    def show_choice(self) -> str:
        """The Show menu's value (see `SHOW_CHOICES`); "all" for a list without the menu."""
        return self._show

    def visible_items(self) -> list[dict[str, Any]]:
        """The items whose rows are shown: they pass every filter and their heading is expanded."""
        return [s.item for s in self._slots if s.item is not None and s.shown]

    def visible_ids(self) -> list[str]:
        return [str(i["id"]) for i in self.visible_items()]

    def heading_text(self, category: str) -> str | None:
        """The text of `category`'s heading, None when the last scan listed none of its items."""
        heading = self._headings.get(category)
        return None if heading is None else str(heading.cget("text"))

    def heading_shown(self, category: str) -> bool:
        """Whether `category`'s heading is gridded."""
        return any(
            s.shown for s in self._slots if s.item is None and s.widget is self._headings.get(category)
        )

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
        for label in (self.empty_label, self._filtered_label):
            if label is not None and label.cget("wraplength") != self._empty_wrap:
                label.configure(wraplength=self._empty_wrap)

    def set_scanning(self) -> None:
        self.scan_button.configure(state="disabled", text="Scanning…")
        # A failed scan left the summary in the error colour.
        self.summary.configure(text=SCANNING_TEXT, text_color=theme.INK_MUTED)
        # Filtering the last scan's rows meanwhile leaves this summary in place.
        self._summary_held = True

    def set_idle(self) -> None:
        self.scan_button.configure(state="normal", text="System Scan")

    def set_scan_enabled(self, enabled: bool) -> None:
        self.scan_button.configure(state="normal" if enabled else "disabled")

    def show_error(self, message: str) -> None:
        self.set_idle()
        self.summary.configure(text=f"Scan failed: {message}", text_color=theme.CRITICAL)
        self._summary_held = True

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
        """Builds the rows of `report`'s items in the panel's categories, then applies the
        filter bar, the search, the Show choice and the collapsed headings."""
        self.set_idle()
        for slot in self._slots:
            slot.widget.destroy()
        self._slots.clear()
        self._rows.clear()
        self._headings.clear()
        for label in (self.empty_label, self._filtered_label):
            if label is not None:
                label.destroy()
        self.empty_label = None
        self._filtered_label = None
        self._filtered_shown = False
        self._summary_held = False
        items = [i for i in report["items"] if i["category"] in self.categories and not never_shown(i)]
        self._items = items
        self._when = when
        item_warnings, panel_warnings = route_warnings(report, self.categories)
        self._unread_ids = {str(i["id"]) for i in items if item_unread(i, item_warnings.get(i["id"], ()))}
        self._show_warnings(panel_warnings)
        row = 0
        for category in self.categories:
            group = [i for i in items if i["category"] == category]
            if not group:
                continue
            heading = self._make_heading(category)
            self._headings[category] = heading
            self._slots.append(
                _Slot(heading, category, None, {"row": row, "pady": (10 if row else 4, 2), "padx": 8})
            )
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
                self._slots.append(
                    _Slot(widget, category, item, {"row": row, "pady": 3, "padx": ScanRow.ROW_PADX})
                )
                self._rows.append(widget)
                row += 1
        if not items:
            # An empty list after a failed read is unknown, not "nothing installed". The
            # warning itself is shown above the list.
            if panel_warnings:
                text, color = "⚠ This list could not be read; see the warning above.", theme.WARNING
            else:
                text, color = self._empty_text, theme.INK_MUTED
            self.empty_label = self._message(text, color)
            self.empty_label.grid(row=0, column=0, sticky="ew", pady=20, padx=8)
        self._apply_filter()
        if self.bulk_button is not None:
            self.bulk_button.configure(
                state="normal" if mutations_enabled and self._bulk_items() else "disabled"
            )

    def _message(self, text: str, color: str) -> ctk.CTkLabel:
        return ctk.CTkLabel(
            self.list,
            text=text,
            font=_font(11),
            text_color=color,
            justify="left",
            wraplength=self._empty_wrap,
        )

    def _make_heading(self, category: str) -> ctk.CTkButton | ctk.CTkLabel:
        text = CATEGORY_HEADINGS[category].upper()
        if not self._searchable:
            return ctk.CTkLabel(
                self.list, text=text, font=_font(10, "bold"), text_color=theme.INK_MUTED, anchor="w"
            )
        return ctk.CTkButton(
            self.list,
            text=text,
            height=22,
            corner_radius=4,
            font=_font(10, "bold"),
            fg_color="transparent",
            hover_color=theme.SURFACE_RAISED,
            text_color=theme.INK_MUTED,
            anchor="w",
            command=lambda: self.toggle_group(category),
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

    def _show_label(self, value: str) -> str:
        return next((label for label, choice in SHOW_CHOICES if choice == value), SHOW_CHOICES[0][0])

    def _show_menu_changed(self, label: str) -> None:
        self._show = self._show_values.get(label, SHOW_DEFAULT)
        self._apply_filter()

    def _query_changed(self, query: str) -> None:
        terms = search_terms(query)
        self._query = query
        if terms != self._terms:
            self._terms = terms
            # Every category with matches opens for a new search.
            self._search_collapsed = set()
        self._apply_filter()

    def set_query(self, text: str) -> None:
        """Puts `text` in the search box and filters at once, without the typing delay."""
        if self.search_entry is not None:
            self.search_entry.set_query(text)

    def set_show(self, value: str) -> None:
        """Selects the Show choice `value` (a value of `SHOW_CHOICES`) and filters."""
        if value not in self._show_values.values():
            raise ValueError(f"unknown Show choice: {value}")
        self._show = value
        if self.show_menu is not None:
            self.show_menu.set(self._show_label(value))
        self._apply_filter()

    def focus_search(self) -> None:
        """Moves the keyboard focus to the search box and selects its text."""
        if self.search_entry is not None:
            self.search_entry.focus_and_select()

    def collapsed(self, category: str) -> bool:
        """Whether `category`'s rows are folded away under its heading: while a search is shown
        only after a click during that search, otherwise as the headings were last set."""
        if self._terms:
            return category in self._search_collapsed
        return category in self._collapsed

    def toggle_group(self, category: str) -> None:
        """Collapses or expands `category`'s rows, as a click on its heading does."""
        folded = self._search_collapsed if self._terms else self._collapsed
        if category in folded:
            folded.discard(category)
        else:
            folded.add(category)
        self._apply_filter()

    def _passes(self, item: dict[str, Any]) -> bool:
        """The Show choice includes the item and it matches the search."""
        return item_shown(item, self._show) and item_matches(item, self._terms)

    def _apply_filter(self) -> None:
        passing: dict[str, int] = {}
        totals: dict[str, int] = {}
        hidden_by_show = 0
        for item in self._items:
            category = item["category"]
            totals[category] = totals.get(category, 0) + 1
            if self._passes(item):
                passing[category] = passing.get(category, 0) + 1
            elif (
                self._filter in ("all", category)
                and item_matches(item, self._terms)
                and not item_shown(item, self._show)
            ):
                hidden_by_show += 1
        for slot in self._slots:
            in_filter = self._filter in ("all", slot.category)
            if slot.item is None:
                wanted = in_filter and passing.get(slot.category, 0) > 0
                if self._searchable:
                    self._label_heading(slot, passing.get(slot.category, 0), totals.get(slot.category, 0))
            else:
                wanted = in_filter and self._passes(slot.item) and not self.collapsed(slot.category)
            if wanted != slot.shown:
                slot.shown = wanted
                if wanted:
                    slot.widget.grid(column=0, sticky="ew", **slot.grid)
                else:
                    # grid_forget: CustomTkinter replays the last grid call on a scaling change,
                    # which would show a filtered-out row again.
                    slot.widget.grid_forget()
        shown = sum(count for category, count in passing.items() if self._filter in ("all", category))
        self._show_summary(shown, hidden_by_show)
        self._show_filtered_message(shown, hidden_by_show)

    def _label_heading(self, slot: _Slot, passing: int, total: int) -> None:
        arrow = "▸" if self.collapsed(slot.category) else "▾"
        count = f"{passing} of {total}" if passing < total else str(total)
        text = f"{arrow} {CATEGORY_HEADINGS[slot.category].upper()}  ·  {count}"
        if slot.widget.cget("text") != text:
            slot.widget.configure(text=text)

    def _show_summary(self, shown: int, hidden_by_show: int) -> None:
        if not self._when or self._summary_held:
            return
        items = self._items
        applied = sum(1 for i in items if i["state"] == "applied")
        pending = sum(
            1
            for i in items
            if i["recommended"]
            and not i.get("unavailable")
            and (i["state"] in ("not_applied", "partial") or i.get("former"))
        )
        text = f"{self._when}  ·  {len(items)} items  ·  {applied} applied  ·  {pending_text(pending)}"
        if shown < len(items):
            text += f"  ·  showing {shown}"
        if self._show == "available" and hidden_by_show:
            text += f"  ·  {hidden_by_show} not available here"
        self.summary.configure(text=text, text_color=theme.INK_MUTED)

    def _show_filtered_message(self, shown: int, hidden_by_show: int) -> None:
        """Says why the list is empty when every item it has is filtered out."""
        if shown or not self._items:
            if self._filtered_shown and self._filtered_label is not None:
                self._filtered_label.grid_forget()
            self._filtered_shown = False
            return
        text, color = self._filtered_text(hidden_by_show)
        if self._filtered_label is None:
            self._filtered_label = self._message(text, color)
        else:
            self._filtered_label.configure(text=text, text_color=color)
        if not self._filtered_shown:
            self._filtered_label.grid(row=0, column=0, sticky="ew", pady=20, padx=8)
            self._filtered_shown = True

    def _filtered_text(self, hidden_by_show: int) -> tuple[str, str]:
        """Text and colour of the message of a list whose items the filters all hide.

        A search names the filters when they hide its matches, and says how to list the tweaks
        this PC cannot use when only those match; `hidden_by_show` counts the matches in the
        chosen category that the Show choice hides. Without a search, the Show texts speak for
        the chosen category only."""
        query = self._query.strip()
        in_category = self._filter != "all"
        if self._terms:
            matching = [i for i in self._items if item_matches(i, self._terms)]
            elsewhere = any(self._filter not in ("all", i["category"]) for i in matching)
            if self._show == "available" and hidden_by_show and not elsewhere:
                template = NO_MATCH_UNUSABLE_ONE if hidden_by_show == 1 else NO_MATCH_UNUSABLE_MANY
                return template.format(query=query, n=hidden_by_show), theme.INK_MUTED
            if matching:
                return NO_MATCH_HIDDEN_TEXT.format(query=query), theme.INK_MUTED
            return NO_MATCH_TEXT.format(query=query), theme.INK_MUTED
        if self._show == "changed":
            unread = sum(
                1
                for i in self._items
                if i["id"] in self._unread_ids and self._filter in ("all", i["category"])
            )
            if unread:
                template = NOTHING_CHANGED_UNREAD_ONE if unread == 1 else NOTHING_CHANGED_UNREAD_MANY
                return template.format(n=unread), theme.INK_MUTED
            return (NOTHING_CHANGED_IN_CATEGORY_TEXT if in_category else NOTHING_CHANGED_TEXT), theme.GOOD
        if self._show == "unavailable":
            return (ALL_USABLE_IN_CATEGORY_TEXT if in_category else ALL_USABLE_TEXT), theme.INK_MUTED
        return self._empty_text, theme.INK_MUTED

    @property
    def filtered_message(self) -> str | None:
        """The text shown when the filters hide every item, None while rows are shown."""
        if not self._filtered_shown or self._filtered_label is None:
            return None
        return str(self._filtered_label.cget("text"))
