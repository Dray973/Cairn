"""Profiles section widgets: the starter cards, the sheet that previews a profile row by row
with a check box per change, the export form and the card of the last applied profile.

The helpers at the top are pure (no Tk), so tests check them without a window. The widgets
never raise on malformed engine data: they show it as an error in the sheet instead.
"""

from __future__ import annotations

import logging
import re
import tkinter as tk
from collections.abc import Callable, Iterable, Mapping, Sequence
from typing import Any

import customtkinter as ctk

from .. import theme
from .controls import PANEL_WRAP, ROW_WRAP, WRAP_STEP, fitted_wrap

log = logging.getLogger(__name__)

# Sections of a profile in apply order, with their headings in the sheet.
SECTION_ORDER = ("tweaks", "startup", "dns", "windows_update", "maintenance", "apps")
SECTION_HEADINGS = {
    "tweaks": "Optimize settings",
    "startup": "Startup apps to turn off",
    "dns": "DNS servers",
    "windows_update": "Windows Update",
    "maintenance": "Scheduled maintenance",
    "apps": "Store apps to remove",
}
# Why a row is skipped, as the engine's `reason` names it.
REASON_TEXT = {
    "unsupported": "Not supported by this version",
    "unreadable": "Couldn't be read",
    "cannot_change": "Can't be changed here",
    "other_account": "Belongs to another account",
    "edition": "Not on this edition of Windows",
    "not_on_this_pc": "Not on this PC",
    "unknown_id": "Unknown to this version of Cairn",
}
# Plan row status -> (icon, text, colour); a skipped row shows its reason as the text.
STATUS_STYLE = {
    "change": ("◐", "Will change", theme.ACCENT),
    "already": ("✓", "Already set", theme.GOOD),
    "skipped": ("–", "Skipped", theme.INK_MUTED),
}
# Apply outcome -> (icon, text, colour).
RESULT_STYLE = {
    "applied": ("✓", "Applied", theme.GOOD),
    "already_set": ("✓", "Already set", theme.GOOD),
    "skipped": ("–", "Skipped", theme.INK_MUTED),
    "failed": ("⚠", "Failed", theme.CRITICAL),
}
NOT_SELECTED_STYLE = ("○", "Not selected", theme.INK_MUTED)
# The same texts as `app.RESTART_TEXT`, which the feature mixins cannot import.
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
RESTART_ORDER = ("none", "explorer", "sign_out", "restart")

INTRO_TEXT = (
    "A profile is a list of Cairn settings you can reuse or take to another PC. Opening one changes "
    "nothing: you see what it would change first, and every change it makes is recorded, so you can undo it."
)
EMPTY_TEXT = (
    "Pick a starter profile or open a profile file to see what it would change on this PC. "
    "Nothing changes until you apply it."
)
FOOTER_NOTE = "Every change is recorded first; undo it here, from History or with Revert All Changes."
EXPORT_NOTE = (
    "Profiles hold only settings Cairn manages. They never include your computer name, user name, "
    "network adapter names, app versions or custom DNS server addresses."
)
EXPORT_TITLE = "Export this PC's settings"
ADMIN_NOTE = "⚠ Applying a profile needs administrator rights. Previewing and exporting don't."
NAME_PLACEHOLDER = "Profile name, for example My gaming setup"
DESCRIPTION_PLACEHOLDER = "Optional description"
NO_CANDIDATES_TEXT = "Cairn has changed no settings on this PC that a profile can hold yet."
STARTER_SOURCE = "Starter profile"
THIS_PC_SOURCE = "This PC"
FALLBACK_FILE_NAME = "Cairn profile"
# Characters Windows does not allow in file names.
_RESERVED_CHARS = re.compile(r'[\\/:*?"<>|\x00-\x1f\x7f]')
_RESERVED_NAMES = {"CON", "PRN", "AUX", "NUL"} | {f"{p}{n}" for p in ("COM", "LPT") for n in range(1, 10)}
MAX_FILE_STEM = 60
# Longest title and source text shown in the sheet's header, in characters.
MAX_TITLE_CHARS = 60
MAX_SOURCE_CHARS = 80

# Widths in CustomTkinter units.
STARTER_COLUMN = 264
STARTER_WRAP = 232
# Room the check box column and the status column take from a row's text.
ROW_CHECK_WIDTH = 24
ROW_STATUS_WIDTH = 150
# The narrowest a wrapped footer or header text is made.
MIN_TEXT_WRAP = 60
# Room the All and None buttons take from the sheet's title, and the gap between footer buttons.
HEADER_BUTTONS_WIDTH = 100
FOOTER_BUTTON_GAP = 8
# Heights of the sheet's title and of its other header lines; a wrapped text grows past them.
TITLE_HEIGHT = 24
LINE_HEIGHT = 18


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def plural(count: int, noun: str) -> str:
    return f"{count} {noun}{'' if count == 1 else 's'}"


def _shortened(text: str, limit: int) -> str:
    return text if len(text) <= limit else text[: limit - 1].rstrip() + "…"


def _wrap_for(available: float, widest: int) -> int:
    """Wrap length for text in `available` units: `widest` when it fits, else the available
    width rounded down to `WRAP_STEP`, but at least `MIN_TEXT_WRAP`."""
    if available >= widest:
        return widest
    return max(MIN_TEXT_WRAP, int(available) // WRAP_STEP * WRAP_STEP)


# -- pure helpers -------------------------------------------------------------------------


def row_status(row: Mapping[str, Any]) -> tuple[str, str, str]:
    """(icon, text, colour) of a plan row's status; a skipped row shows its reason."""
    status = row.get("status")
    if status == "skipped":
        icon, _, color = STATUS_STYLE["skipped"]
        return icon, REASON_TEXT.get(str(row.get("reason") or ""), "Skipped"), color
    return STATUS_STYLE.get(str(status), ("–", str(status or "unknown"), theme.INK_MUTED))


def result_status(
    row: Mapping[str, Any], results_by_key: Mapping[str, Mapping[str, Any]], selected: Iterable[str]
) -> tuple[str, str, str]:
    """(icon, text, colour) of a row after an apply: its outcome when it has one, "Not
    selected" for a change row that was left out, else its plan status."""
    result = results_by_key.get(str(row.get("key")))
    if result is not None:
        outcome = str(result.get("outcome"))
        return RESULT_STYLE.get(outcome, ("–", outcome, theme.INK_MUTED))
    if row.get("status") == "change" and row.get("key") not in set(selected):
        return NOT_SELECTED_STYLE
    return row_status(row)


def group_rows(rows: Iterable[Mapping[str, Any]]) -> list[tuple[str, list[dict[str, Any]]]]:
    """Rows grouped by section in `SECTION_ORDER`; sections this version does not know
    follow, in the order they first appear."""
    groups: dict[str, list[dict[str, Any]]] = {}
    for row in rows:
        groups.setdefault(str(row.get("section") or ""), []).append(dict(row))
    ordered = [s for s in SECTION_ORDER if s in groups]
    ordered += [s for s in groups if s not in SECTION_ORDER]
    return [(s, groups[s]) for s in ordered]


def _status_counts_text(changes: int, already: int, skipped: int, sep: str) -> str:
    parts = []
    if changes:
        parts.append(f"{changes} to change")
    if already:
        parts.append(f"{already} already set")
    if skipped:
        parts.append(f"{skipped} skipped")
    return sep.join(parts) if parts else "Nothing to change"


def plan_counts_text(plan: Mapping[str, Any], sep: str = " · ") -> str:
    """ "7 to change · 2 already set · 1 skipped", without the parts that are zero; "Nothing
    to change" when all are."""
    return _status_counts_text(
        int(plan.get("changes") or 0), int(plan.get("already") or 0), int(plan.get("skipped") or 0), sep
    )


def section_counts_text(rows: Iterable[Mapping[str, Any]]) -> str:
    """The counts of `plan_counts_text` over one section's rows."""
    rows = list(rows)
    return _status_counts_text(
        len([r for r in rows if r.get("status") == "change"]),
        len([r for r in rows if r.get("status") == "already"]),
        len([r for r in rows if r.get("status") == "skipped"]),
        " · ",
    )


def fold_text(rows: Iterable[Mapping[str, Any]], expanded: bool) -> str:
    """Label of the button that shows or hides a section's rows that change nothing."""
    rows = list(rows)
    already = len([r for r in rows if r.get("status") == "already"])
    skipped = len([r for r in rows if r.get("status") != "already"])
    parts = []
    if already:
        parts.append(f"{already} already set")
    if skipped:
        parts.append(f"{skipped} skipped")
    return f"{'▾' if expanded else '▸'} {' · '.join(parts)}"


def counts_text(counts: Mapping[str, Any]) -> str:
    """What a profile holds: "10 settings · 21 apps · 2 startup apps · DNS · Windows Update ·
    maintenance"."""
    parts = []
    tweaks = int(counts.get("tweaks") or 0)
    apps = int(counts.get("apps") or 0)
    startup = int(counts.get("startup") or 0)
    if tweaks:
        parts.append(plural(tweaks, "setting"))
    if apps:
        parts.append(plural(apps, "app"))
    if startup:
        parts.append(plural(startup, "startup app"))
    if counts.get("dns"):
        parts.append("DNS")
    if counts.get("windows_update"):
        parts.append("Windows Update")
    if counts.get("maintenance"):
        parts.append("maintenance")
    return " · ".join(parts) if parts else "No settings"


def default_selection(rows: Iterable[Mapping[str, Any]], keep_unchecked: Iterable[str] = ()) -> set[str]:
    """Keys of the change rows that start checked: every change without a caution, except the
    keys the user unchecked before."""
    unchecked = set(keep_unchecked)
    return {
        str(r["key"])
        for r in rows
        if r.get("status") == "change" and not r.get("caution") and r.get("key") not in unchecked
    }


def apply_details(rows: Sequence[Mapping[str, Any]], limit: int = 30) -> list[str]:
    """Detail lines of the apply dialog: one "• <title>" per row, at most `limit`."""
    lines = [f"• {r.get('title') or r.get('key')}" for r in rows[:limit]]
    if len(rows) > limit:
        lines.append(f"• …and {len(rows) - limit} more")
    return lines


def restart_max(values: Iterable[str]) -> str:
    """The strongest of the restart needs `values` ("none" when there is none)."""
    best = 0
    for value in values:
        if value in RESTART_ORDER:
            best = max(best, RESTART_ORDER.index(value))
    return RESTART_ORDER[best]


def selected_restart(rows: Iterable[Mapping[str, Any]], keys: Iterable[str]) -> str:
    """The strongest restart need of the rows whose key is in `keys`."""
    chosen = set(keys)
    return restart_max(str(r.get("restart") or "none") for r in rows if r.get("key") in chosen)


def result_summary(report: Mapping[str, Any]) -> str:
    """ "7 applied, 1 already set, 1 skipped, 1 failed"; parts that are zero are left out,
    except the applied count."""
    parts = [f"{int(report.get('applied') or 0)} applied"]
    for key, text in (("already", "already set"), ("skipped", "skipped"), ("failed", "failed")):
        count = int(report.get(key) or 0)
        if count:
            parts.append(f"{count} {text}")
    return ", ".join(parts)


def filter_is_empty(target_filter: Mapping[str, Any] | None) -> bool:
    """Whether a `revert_targets` filter selects nothing."""
    if not target_filter:
        return True
    return not any(value for value in target_filter.values())


def suggested_file_name(name: str) -> str:
    """A file name for a profile named `name`: whitespace runs become one space, characters
    Windows refuses become "-", leading and trailing dots and spaces go, at most 60
    characters, "Cairn profile" when nothing is left or the name is a device name, then
    ".json"."""
    stem = _RESERVED_CHARS.sub("-", " ".join(name.split())).strip(" .")
    stem = stem[:MAX_FILE_STEM].strip(" .")
    if not stem or stem.split(".")[0].upper() in _RESERVED_NAMES:
        stem = FALLBACK_FILE_NAME
    return f"{stem}.json"


# -- widgets ------------------------------------------------------------------------------


class StarterCard(ctk.CTkFrame):
    """A built-in profile: its name, description, what it holds and a Preview button."""

    def __init__(self, master: tk.Misc, starter: Mapping[str, Any], on_preview: Callable[[], None]) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self.starter = dict(starter)
        self.starter_id = str(starter.get("id") or "")
        self.grid_columnconfigure(0, weight=1)
        self.name_label = ctk.CTkLabel(
            self,
            text=str(starter.get("name") or self.starter_id),
            font=_font(12, "bold"),
            text_color=theme.INK,
        )
        self.name_label.grid(row=0, column=0, sticky="w", padx=12, pady=(10, 0))
        self.description_label = ctk.CTkLabel(
            self,
            text=str(starter.get("description") or ""),
            font=_font(11),
            text_color=theme.INK_SECONDARY,
            anchor="w",
            justify="left",
            wraplength=STARTER_WRAP,
        )
        self.description_label.grid(row=1, column=0, sticky="w", padx=12, pady=(2, 0))
        counts = starter.get("counts")
        self.counts_label = ctk.CTkLabel(
            self,
            text=counts_text(counts) if isinstance(counts, Mapping) else "",
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
        )
        self.counts_label.grid(row=2, column=0, sticky="w", padx=12, pady=(2, 0))
        self.preview_button = ctk.CTkButton(
            self,
            text="Preview",
            width=90,
            height=28,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_preview,
        )
        self.preview_button.grid(row=3, column=0, sticky="w", padx=12, pady=(6, 10))

    def set_current(self, current: bool) -> None:
        """Marks the card whose profile the sheet shows."""
        self.configure(border_color=theme.ACCENT if current else theme.BORDER)


class LastAppliedCard(ctk.CTkFrame):
    """The profile applied last in this window, with Undo these changes."""

    def __init__(self, master: tk.Misc, on_undo: Callable[[], None]) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(self, text="Last applied", font=_font(11, "bold"), text_color=theme.INK_MUTED).grid(
            row=0, column=0, sticky="w", padx=12, pady=(10, 0)
        )
        self.name_label = ctk.CTkLabel(
            self,
            text="",
            font=_font(12, "bold"),
            text_color=theme.INK,
            anchor="w",
            justify="left",
            wraplength=STARTER_WRAP,
        )
        self.name_label.grid(row=1, column=0, sticky="w", padx=12)
        self.detail_label = ctk.CTkLabel(self, text="", font=_font(11), text_color=theme.INK_MUTED)
        self.detail_label.grid(row=2, column=0, sticky="w", padx=12)
        self.undo_button = ctk.CTkButton(
            self,
            text="Undo these changes",
            width=150,
            height=28,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_undo,
        )
        self.undo_button.grid(row=3, column=0, sticky="w", padx=12, pady=(6, 10))

    def show(self, name: str, when: str, count: int) -> None:
        self.name_label.configure(text=name)
        self.detail_label.configure(text=f"{plural(count, 'change')}  ·  {when}")


class ProfileRow(ctk.CTkFrame):
    """One row of the sheet: a check box for a change (a spacer otherwise), the title, the
    detail, an optional caution and the status.

    A plan can hold hundreds of rows, so the texts are plain Tk labels in the font and size
    CustomTkinter would use at the window's scaling; the row updates them itself when the
    scaling changes.
    """

    def __init__(
        self,
        master: tk.Misc,
        row: Mapping[str, Any],
        *,
        checkable: bool,
        checked: bool,
        status: tuple[str, str, str],
        detail: str,
        wrap: int,
        on_toggle: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self.row = dict(row)
        self.key = str(row.get("key") or "")
        self._scale = ctk.ScalingTracker.get_widget_scaling(self)
        self._wrap = wrap
        self._labels: list[tuple[tk.Label, int, str, bool]] = []
        self.grid_columnconfigure(1, weight=1)

        self.checkbox: ctk.CTkCheckBox | None = None
        if checkable:
            self.checkbox = ctk.CTkCheckBox(
                self,
                text="",
                width=ROW_CHECK_WIDTH,
                checkbox_width=18,
                checkbox_height=18,
                border_width=2,
                fg_color=theme.ACCENT,
                hover_color=theme.ACCENT_HOVER,
                border_color=theme.INK_MUTED,
                command=lambda: on_toggle(self.key),
            )
            if checked:
                self.checkbox.select()
            self.checkbox.grid(row=0, column=0, rowspan=3, sticky="nw", pady=(1, 0))
        else:
            ctk.CTkFrame(self, width=ROW_CHECK_WIDTH, height=1, fg_color="transparent").grid(
                row=0, column=0, sticky="nw"
            )

        self.title_label = self._text(str(row.get("title") or self.key), 11, theme.INK, wraps=True)
        self.title_label.grid(row=0, column=1, sticky="nw", padx=(self._px(6), 0))
        self.detail_label = self._text(detail, 10, theme.INK_MUTED, wraps=True)
        self.detail_label.grid(row=1, column=1, sticky="nw", padx=(self._px(6), 0))
        self.caution_label: tk.Label | None = None
        caution = row.get("caution")
        if caution:
            self.caution_label = self._text(f"⚠ {caution}", 10, theme.WARNING, wraps=True)
            self.caution_label.grid(row=2, column=1, sticky="nw", padx=(self._px(6), 0))
        icon, text, color = status
        self.status_label = self._text(f"{icon} {text}", 10, color, wraps=False)
        self.status_label.configure(anchor="ne", justify="right", wraplength=self._px(ROW_STATUS_WIDTH))
        self.status_label.grid(row=0, column=2, rowspan=2, sticky="ne", padx=(self._px(8), self._px(4)))

    @property
    def checked(self) -> bool:
        return self.checkbox is not None and bool(self.checkbox.get())

    def set_checked(self, checked: bool) -> None:
        if self.checkbox is None:
            return
        if checked:
            self.checkbox.select()
        else:
            self.checkbox.deselect()

    def set_status(self, status: tuple[str, str, str]) -> None:
        icon, text, color = status
        self.status_label.configure(text=f"{icon} {text}", fg=color)

    def set_wraplength(self, wrap: int) -> None:
        if wrap == self._wrap:
            return
        self._wrap = wrap
        for label, _, _, wraps in self._labels:
            if wraps:
                label.configure(wraplength=self._px(wrap))

    def _px(self, value: float) -> int:
        return round(value * self._scale)

    def _text(self, text: str, size: int, color: str, *, wraps: bool, weight: str = "normal") -> tk.Label:
        label = tk.Label(
            self,
            text=text,
            font=(theme.FONT_FAMILY, -self._px(size), weight),
            fg=color,
            bg=theme.SURFACE,
            anchor="nw",
            justify="left",
            wraplength=self._px(self._wrap) if wraps else 0,
            bd=0,
            padx=0,
            pady=0,
            highlightthickness=0,
        )
        self._labels.append((label, size, weight, wraps))
        return label

    def _set_scaling(self, new_widget_scaling: float, new_window_scaling: float) -> None:
        super()._set_scaling(new_widget_scaling, new_window_scaling)
        if new_widget_scaling == self._scale:
            return
        self._scale = new_widget_scaling
        for label, size, weight, wraps in self._labels:
            label.configure(font=(theme.FONT_FAMILY, -self._px(size), weight))
            if wraps:
                label.configure(wraplength=self._px(self._wrap))
        self.status_label.configure(wraplength=self._px(ROW_STATUS_WIDTH))


class _Section:
    """One section of the sheet's list: its heading, the rows that are always shown and the
    rows behind the fold button, built the first time they are shown."""

    def __init__(
        self, frame: ctk.CTkFrame, shown: list[dict[str, Any]], folded: list[dict[str, Any]]
    ) -> None:
        self.frame = frame
        self.shown = shown
        self.folded = folded
        self.fold_button: ctk.CTkButton | None = None
        self.fold_frame: ctk.CTkFrame | None = None
        self.expanded = False
        self.built = False


class ProfileSheet(ctk.CTkFrame):
    """The right-hand card: a profile's rows in plan, result or export mode, or a message
    (empty, loading, error).

    Change rows carry check boxes that start as the plan selects them; rows that change nothing
    fold behind a button per section. In export mode every candidate row is checkable. Each
    `show_*` method replaces the whole content; malformed data becomes an error message.
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_apply: Callable[[list[str]], None],
        on_undo: Callable[[], None],
        on_again: Callable[[], None],
        on_retry: Callable[[], None],
        on_save: Callable[[str, str, list[str]], None],
        on_cancel: Callable[[], None],
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_apply = on_apply
        self._on_save = on_save
        self.mode = "empty"
        self.rows: list[ProfileRow] = []
        self._data: list[dict[str, Any]] = []
        self._checked: dict[str, bool] = {}
        self._sections: list[_Section] = []
        self._results: dict[str, dict[str, Any]] = {}
        # Where the shown plan came from, kept for its result.
        self._source = ""
        self._chosen: set[str] = set()
        self._undo_available = False
        self._elevated = False
        self._engine_ready = True
        self._reads = True
        self._apply_allowed = True
        self._width = 0
        self._list_width = 0
        self._footer_width = 0
        self._row_wrap = ROW_WRAP
        self._warnings: list[str] = []
        # A message (empty, loading, error, nothing to export) is shown instead of the list.
        self._message_shown = True
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(5, weight=1)

        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(0, weight=1)
        self._header = header
        self.title_label = ctk.CTkLabel(
            header,
            text="",
            height=TITLE_HEIGHT,
            font=_font(13, "bold"),
            text_color=theme.INK,
            anchor="w",
            justify="left",
        )
        self.title_label.grid(row=0, column=0, sticky="w")
        self.select_all_button = ctk.CTkButton(
            header,
            text="All",
            width=44,
            height=24,
            font=_font(11),
            fg_color="transparent",
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            text_color=theme.ACCENT,
            command=lambda: self._select_all(True),
        )
        self.select_none_button = ctk.CTkButton(
            header,
            text="None",
            width=44,
            height=24,
            font=_font(11),
            fg_color="transparent",
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            text_color=theme.ACCENT,
            command=lambda: self._select_all(False),
        )
        self.source_label = self._line(header, theme.INK_MUTED)
        self.summary_label = self._line(self, theme.INK_SECONDARY)
        self.description_label = self._line(self, theme.INK_MUTED)
        self.warning_label = self._line(self, theme.WARNING)

        self.form = ctk.CTkFrame(self, fg_color="transparent")
        self.form.grid_columnconfigure(0, weight=1)
        self.name_entry = ctk.CTkEntry(
            self.form,
            width=260,
            height=30,
            font=_font(12),
            placeholder_text=NAME_PLACEHOLDER,
            fg_color=theme.PAGE,
            border_color=theme.BORDER,
            text_color=theme.INK,
        )
        self.name_entry.grid(row=0, column=0, sticky="w")
        self.description_entry = ctk.CTkEntry(
            self.form,
            width=360,
            height=30,
            font=_font(12),
            placeholder_text=DESCRIPTION_PLACEHOLDER,
            fg_color=theme.PAGE,
            border_color=theme.BORDER,
            text_color=theme.INK,
        )
        self.description_entry.grid(row=1, column=0, sticky="w", pady=(6, 0))
        self.name_error_label = ctk.CTkLabel(
            self.form, text="", font=_font(11), text_color=theme.CRITICAL, anchor="w", justify="left"
        )
        self.export_note_label = ctk.CTkLabel(
            self.form,
            text=EXPORT_NOTE,
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
        )
        self.export_note_label.grid(row=3, column=0, sticky="w", pady=(4, 0))

        self.list = ctk.CTkScrollableFrame(
            self,
            fg_color=theme.SURFACE,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.list.grid_columnconfigure(0, weight=1)
        # The list's inner frame is as wide as the visible list; CustomTkinter binds its own
        # handler there, so this one is added next to it.
        self.list.bind("<Configure>", self._list_resized, add="+")

        self.message = ctk.CTkFrame(self, fg_color="transparent")
        self.message.grid_columnconfigure(0, weight=1)
        self.message_label = ctk.CTkLabel(
            self.message, text=EMPTY_TEXT, font=_font(12), text_color=theme.INK_MUTED, justify="center"
        )
        self.message_label.grid(row=0, column=0)
        self.retry_button = ctk.CTkButton(
            self.message,
            text="Try again",
            width=110,
            height=30,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_retry,
        )

        footer = ctk.CTkFrame(self, fg_color="transparent")
        footer.grid(row=6, column=0, sticky="ew", padx=14, pady=(6, 10))
        footer.grid_columnconfigure(0, weight=1)
        self._footer = footer
        self.note_label = ctk.CTkLabel(
            footer, text="", font=_font(11), text_color=theme.INK_MUTED, anchor="w", justify="left"
        )
        self.note_label.grid(row=0, column=0, sticky="w", padx=(0, 12))
        # A 1x1 base size, so the frame takes no room while the mode shows no button.
        self.buttons = ctk.CTkFrame(footer, fg_color="transparent", width=1, height=1)
        self.buttons.grid(row=0, column=1, sticky="e")
        self.apply_button = ctk.CTkButton(
            self.buttons,
            text="Apply 0 changes",
            width=170,
            height=32,
            font=_font(12, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            command=self._apply,
        )
        self.undo_button = ctk.CTkButton(
            self.buttons,
            text="Undo these changes",
            width=150,
            height=32,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_undo,
        )
        self.again_button = ctk.CTkButton(
            self.buttons,
            text="Preview again",
            width=120,
            height=32,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_again,
        )
        self.cancel_button = ctk.CTkButton(
            self.buttons,
            text="Cancel",
            width=90,
            height=32,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_cancel,
        )
        self.save_button = ctk.CTkButton(
            self.buttons,
            text="Save profile…",
            width=130,
            height=32,
            font=_font(12, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            command=self._save,
        )

        self.bind("<Configure>", self._resized)
        tk.Frame.bind(footer, "<Configure>", self._footer_resized, add="+")
        self._show_message(EMPTY_TEXT, theme.INK_MUTED, retry=False)
        self._layout()

    @staticmethod
    def _line(master: tk.Misc, color: str) -> ctk.CTkLabel:
        """A left-aligned 11 px text line of the header, as tall as its text."""
        return ctk.CTkLabel(
            master,
            text="",
            height=LINE_HEIGHT,
            font=_font(11),
            text_color=color,
            anchor="w",
            justify="left",
        )

    # -- queries ---------------------------------------------------------------------

    def selected_keys(self) -> list[str]:
        """Checked keys, in the order of the rows."""
        return [str(r["key"]) for r in self._data if self._checked.get(str(r["key"]))]

    def unchecked_keys(self) -> set[str]:
        """Change rows of the shown plan that are not checked."""
        if self.mode != "plan":
            return set()
        return {key for key, checked in self._checked.items() if not checked}

    def export_fields(self) -> tuple[str, str]:
        return self.name_entry.get().strip(), self.description_entry.get().strip()

    def row(self, key: str) -> ProfileRow | None:
        """The built row of `key`, if it is built."""
        return next((r for r in self.rows if r.key == key), None)

    # -- states ----------------------------------------------------------------------

    def set_empty(self) -> None:
        self._reset("empty")
        self._show_message(EMPTY_TEXT, theme.INK_MUTED, retry=False)
        self._layout()

    def set_loading(self, text: str) -> None:
        self._reset("loading")
        self._show_message(text, theme.INK_MUTED, retry=False)
        self._layout()

    def show_error(self, text: str, retry: bool = True) -> None:
        self._reset("error")
        self._show_message(f"⚠ {text}", theme.CRITICAL, retry=retry)
        self._layout()

    def set_access(self, engine_ready: bool, elevated: bool) -> None:
        self._engine_ready = engine_ready
        self._elevated = elevated
        self._layout()

    def set_actions_enabled(self, *, reads: bool, apply: bool) -> None:
        self._reads = reads
        self._apply_allowed = apply
        self._apply_state()

    def show_plan(
        self,
        plan: Mapping[str, Any],
        *,
        source: str,
        keep_unchecked: Iterable[str] = (),
        description: str = "",
    ) -> None:
        """Lists a plan's rows; change rows start checked as `default_selection` says."""
        try:
            rows = [dict(r) for r in plan["rows"]]
            self._reset("plan")
            self._data = rows
            selection = default_selection(rows, keep_unchecked)
            self._checked = {
                str(r["key"]): str(r["key"]) in selection for r in rows if r.get("status") == "change"
            }
            self._source = source
            self._set_header(str(plan.get("name") or ""), source, plan_counts_text(plan), description)
            warnings = [str(w) for w in plan.get("warnings") or []]
            if plan.get("other_account"):
                warnings.insert(0, str(plan["other_account"]))
            self._set_warnings(warnings)
            self._build_sections(rows, checkable=lambda r: r.get("status") == "change")
            self._layout()
        except Exception as exc:  # noqa: BLE001 - malformed engine data is shown in the sheet, never raised
            log.exception("profile plan could not be shown")
            self.show_error(f"unexpected profile data ({exc})")

    def show_result(self, plan: Mapping[str, Any], report: Mapping[str, Any]) -> None:
        """Lists the plan's rows with what the apply did to each."""
        try:
            rows = [dict(r) for r in plan["rows"]]
            results = {str(r["key"]): dict(r) for r in report["results"]}
            self._reset("result")
            self._data = rows
            self._results = results
            self._chosen = set(results)
            self._undo_available = not filter_is_empty(report.get("undo"))
            self._set_header(
                str(report.get("name") or plan.get("name") or ""),
                self._source,
                result_summary(report),
                "",
            )
            self._set_warnings([str(w) for w in report.get("warnings") or []])
            self._build_sections(rows, checkable=lambda _r: False)
            self._layout()
        except Exception as exc:  # noqa: BLE001 - malformed engine data is shown in the sheet, never raised
            log.exception("profile result could not be shown")
            self.show_error(f"unexpected profile data ({exc})", retry=False)

    def show_export(self, candidates: Mapping[str, Any]) -> None:
        """Lists this PC's exportable settings with the name and description fields."""
        try:
            rows = [dict(r) for r in candidates["rows"]]
            self._reset("export")
            for row in rows:
                row.setdefault("status", "change")
            self._data = rows
            self._checked = {str(r["key"]): bool(r.get("selected")) for r in rows}
            summary = plural(len(rows), "setting") + " to choose from" if rows else "Nothing to export yet"
            self._set_header(EXPORT_TITLE, THIS_PC_SOURCE, summary, "")
            warnings = [str(w) for w in candidates.get("warnings") or []]
            if candidates.get("other_account"):
                warnings.insert(0, str(candidates["other_account"]))
            self._set_warnings(warnings)
            self.name_error_label.grid_forget()
            # get() is "" while the placeholder shows; deleting then would remove the placeholder.
            for entry in (self.name_entry, self.description_entry):
                if entry.get():
                    entry.delete(0, "end")
            if rows:
                self._build_sections(rows, checkable=lambda _r: True)
            else:
                self._show_message(NO_CANDIDATES_TEXT, theme.INK_MUTED, retry=False)
            self._layout()
        except Exception as exc:  # noqa: BLE001 - malformed engine data is shown in the sheet, never raised
            log.exception("export candidates could not be shown")
            self.show_error(f"unexpected settings data ({exc})")

    def show_name_error(self, text: str) -> None:
        self.name_error_label.configure(text=f"⚠ {text}")
        self.name_error_label.grid(row=2, column=0, sticky="w", pady=(4, 0))
        self.name_entry.focus_set()

    def add_warning(self, text: str) -> None:
        """Adds a line to the sheet's warnings."""
        self._set_warnings([*self._warnings, text])

    # -- building --------------------------------------------------------------------

    def _reset(self, mode: str) -> None:
        self.mode = mode
        for child in self.list.winfo_children():
            child.destroy()
        self.rows = []
        self._data = []
        self._checked = {}
        self._sections = []
        self._results = {}
        self._chosen = set()
        self._undo_available = False
        self._warnings = []

    def _show_message(self, text: str, color: str, *, retry: bool) -> None:
        self.message_label.configure(text=text, text_color=color)
        if retry:
            self.retry_button.grid(row=1, column=0, pady=(10, 0))
        else:
            self.retry_button.grid_forget()
        self._message_shown = True

    def _set_header(self, title: str, source: str, summary: str, description: str) -> None:
        self.title_label.configure(text=_shortened(title, MAX_TITLE_CHARS))
        self.source_label.configure(text=_shortened(source, MAX_SOURCE_CHARS))
        if source:
            self.source_label.grid(row=1, column=0, columnspan=3, sticky="w")
        else:
            self.source_label.grid_forget()
        self.summary_label.configure(text=summary)
        self.description_label.configure(text=description)

    def _set_warnings(self, lines: list[str]) -> None:
        self._warnings = lines
        self.warning_label.configure(text="\n".join(f"⚠ {w}" for w in lines))

    def _build_sections(
        self, rows: list[dict[str, Any]], *, checkable: Callable[[dict[str, Any]], bool]
    ) -> None:
        self._message_shown = False
        for index, (section, section_rows) in enumerate(group_rows(rows)):
            frame = ctk.CTkFrame(self.list, fg_color="transparent")
            frame.grid(row=index, column=0, sticky="ew", padx=4, pady=(4, 6))
            frame.grid_columnconfigure(0, weight=1)
            if self.mode == "export":
                shown, folded = section_rows, []
            else:
                shown = [r for r in section_rows if r.get("status") == "change"]
                folded = [r for r in section_rows if r.get("status") != "change"]
            part = _Section(frame, shown, folded)
            self._sections.append(part)
            heading = ctk.CTkFrame(frame, fg_color="transparent")
            heading.grid(row=0, column=0, sticky="ew", pady=(0, 2))
            ctk.CTkLabel(
                heading,
                text=SECTION_HEADINGS.get(section, section or "Other settings"),
                font=_font(12, "bold"),
                text_color=theme.INK_SECONDARY,
            ).grid(row=0, column=0, sticky="w")
            counts = (
                plural(len(section_rows), "setting")
                if self.mode == "export"
                else section_counts_text(section_rows)
            )
            ctk.CTkLabel(heading, text=counts, font=_font(11), text_color=theme.INK_MUTED).grid(
                row=0, column=1, sticky="w", padx=(10, 0)
            )
            next_row = 1
            for row in shown:
                built = self._make_row(frame, row, checkable(row))
                built.grid(row=next_row, column=0, sticky="ew", pady=2)
                next_row += 1
            if folded:
                part.fold_button = ctk.CTkButton(
                    frame,
                    text=fold_text(folded, expanded=False),
                    height=24,
                    font=_font(11),
                    fg_color="transparent",
                    hover_color=theme.BUTTON_NEUTRAL_HOVER,
                    text_color=theme.ACCENT,
                    anchor="w",
                    command=lambda p=part: self._toggle_fold(p),
                )
                part.fold_button.grid(row=next_row, column=0, sticky="w", pady=(2, 0))
                part.fold_frame = ctk.CTkFrame(frame, fg_color="transparent")
                part.fold_frame.grid_columnconfigure(0, weight=1)
                part.built = False

    def _make_row(self, master: tk.Misc, row: dict[str, Any], checkable: bool) -> ProfileRow:
        key = str(row.get("key") or "")
        detail = str(row.get("detail") or "")
        if self.mode == "result":
            status = result_status(row, self._results, self._chosen)
            result = self._results.get(key)
            if (
                result is not None
                and result.get("outcome") in ("failed", "skipped")
                and result.get("details")
            ):
                detail = "; ".join(str(d) for d in result["details"])
        elif self.mode == "export":
            status = ("", "", theme.INK_MUTED)
        else:
            status = row_status(row)
        built = ProfileRow(
            master,
            row,
            checkable=checkable,
            checked=self._checked.get(key, False),
            status=status,
            detail=detail,
            wrap=self._row_wrap,
            on_toggle=self._toggled,
        )
        self.rows.append(built)
        return built

    def _toggle_fold(self, part: _Section) -> None:
        if part.fold_frame is None or part.fold_button is None:
            return
        part.expanded = not part.expanded
        if part.expanded:
            if not part.built:
                for index, row in enumerate(part.folded):
                    self._make_row(part.fold_frame, row, checkable=False).grid(
                        row=index, column=0, sticky="ew", pady=2
                    )
                part.built = True
            part.fold_frame.grid(row=len(part.shown) + 2, column=0, sticky="ew")
        else:
            part.fold_frame.grid_forget()
        part.fold_button.configure(text=fold_text(part.folded, expanded=part.expanded))

    def expand_all(self) -> None:
        """Shows every folded row."""
        for part in self._sections:
            if not part.expanded:
                self._toggle_fold(part)

    def _toggled(self, key: str) -> None:
        built = self.row(key)
        if built is not None:
            self._checked[key] = built.checked
        self._apply_state()

    def _select_all(self, checked: bool) -> None:
        for key in self._checked:
            self._checked[key] = checked
        for built in self.rows:
            if built.checkbox is not None:
                built.set_checked(checked)
        self._apply_state()

    def _apply(self) -> None:
        keys = self.selected_keys()
        if keys:
            self._on_apply(keys)

    def _save(self) -> None:
        name, description = self.export_fields()
        self._on_save(name, description, self.selected_keys())

    # -- layout ----------------------------------------------------------------------

    def _layout(self) -> None:
        """Grids what the mode shows and forgets the rest."""
        mode = self.mode
        listing = mode in ("plan", "result") or (mode == "export" and not self._message_shown)
        rows_mode = mode in ("plan", "result", "export")
        if mode in ("plan", "export") and self._data:
            self.select_all_button.grid(row=0, column=1, sticky="e")
            self.select_none_button.grid(row=0, column=2, sticky="e")
        else:
            self.select_all_button.grid_forget()
            self.select_none_button.grid_forget()
        for label, row, shown in (
            (self.summary_label, 1, rows_mode),
            (self.description_label, 2, rows_mode and bool(self.description_label.cget("text"))),
            (self.warning_label, 3, rows_mode and bool(self._warnings)),
        ):
            if shown:
                label.grid(row=row, column=0, sticky="ew", padx=14, pady=(2, 0))
            else:
                label.grid_forget()
        if mode == "export":
            self.form.grid(row=4, column=0, sticky="ew", padx=14, pady=(6, 2))
        else:
            self.form.grid_forget()
        if listing:
            self.message.grid_forget()
            self.list.grid(row=5, column=0, sticky="nsew", padx=6, pady=(6, 0))
        else:
            self.list.grid_forget()
            self.message.grid(row=5, column=0, sticky="", padx=14, pady=14)
        for button in (
            self.apply_button,
            self.undo_button,
            self.again_button,
            self.cancel_button,
            self.save_button,
        ):
            button.grid_forget()
        if mode == "plan":
            self.apply_button.grid(row=0, column=0)
            if self._engine_ready and not self._elevated:
                self.note_label.configure(text=ADMIN_NOTE, text_color=theme.WARNING)
            else:
                self.note_label.configure(text=FOOTER_NOTE, text_color=theme.INK_MUTED)
        elif mode == "result":
            column = 0
            if self._undo_available:
                self.undo_button.grid(row=0, column=column, padx=(0, FOOTER_BUTTON_GAP))
                column += 1
            self.again_button.grid(row=0, column=column)
            self.note_label.configure(
                text=FOOTER_NOTE if self._undo_available else "", text_color=theme.INK_MUTED
            )
        elif mode == "export":
            self.cancel_button.grid(row=0, column=0, padx=(0, FOOTER_BUTTON_GAP))
            self.save_button.grid(row=0, column=1)
            self.note_label.configure(text="", text_color=theme.INK_MUTED)
        else:
            self.note_label.configure(text="", text_color=theme.INK_MUTED)
        self._apply_state()
        self._fit_texts()

    def _apply_state(self) -> None:
        count = len(self.selected_keys()) if self.mode in ("plan", "export") else 0
        self.apply_button.configure(
            text=f"Apply {plural(count, 'change')}",
            state="normal"
            if count and self._reads and self._apply_allowed and self._engine_ready and self._elevated
            else "disabled",
        )
        self.save_button.configure(state="normal" if count and self._reads else "disabled")
        self.cancel_button.configure(state="normal" if self._reads else "disabled")
        self.undo_button.configure(state="normal" if self._apply_allowed and self._elevated else "disabled")
        for button in (self.again_button, self.retry_button):
            button.configure(state="normal" if self._reads else "disabled")

    def _units(self, pixels: int) -> float:
        return pixels / (ctk.ScalingTracker.get_widget_scaling(self) or 1.0)

    def _resized(self, event: tk.Event) -> None:
        if event.width == self._width:
            return
        self._width = event.width
        self._fit_texts()

    def _footer_resized(self, event: tk.Event) -> None:
        if event.width == self._footer_width:
            return
        self._footer_width = event.width
        self._fit_note()

    def _fit_texts(self) -> None:
        """Wraps the header, summary, warning and message texts at the sheet's width."""
        if not self._width:
            return
        width = self._units(self._width) - 28
        wrap = fitted_wrap(width, PANEL_WRAP)
        # winfo_manager, not winfo_ismapped: a button gridded this moment is mapped only when idle.
        header_buttons = HEADER_BUTTONS_WIDTH if self.select_all_button.winfo_manager() else 0
        title_wrap = _wrap_for(width - header_buttons, PANEL_WRAP)
        for label, value in (
            (self.title_label, title_wrap),
            (self.source_label, wrap),
            (self.summary_label, wrap),
            (self.description_label, wrap),
            (self.warning_label, wrap),
            (self.message_label, _wrap_for(width - 28, 520)),
            (self.name_error_label, wrap),
            (self.export_note_label, wrap),
        ):
            if label.cget("wraplength") != value:
                label.configure(wraplength=value)
        self._fit_note()

    def _buttons_units(self) -> float:
        """Width of the footer buttons the mode shows, in CustomTkinter units. Read from the
        buttons' own widths, since the frame's requested size is updated only when idle."""
        shown = [
            b
            for b in (
                self.apply_button,
                self.undo_button,
                self.again_button,
                self.cancel_button,
                self.save_button,
            )
            if b.winfo_manager()
        ]
        if not shown:
            return 0.0
        widths = [max(float(b.cget("width")), self._units(b.winfo_reqwidth())) for b in shown]
        return sum(widths) + FOOTER_BUTTON_GAP * (len(shown) - 1)

    def _fit_note(self) -> None:
        """Wraps the footer note in the room the footer's buttons leave."""
        if self._footer_width:
            footer = self._units(self._footer_width)
        elif self._width:
            footer = self._units(self._width) - 28
        else:
            return
        wrap = _wrap_for(footer - self._buttons_units() - 12, 520)
        if self.note_label.cget("wraplength") != wrap:
            self.note_label.configure(wraplength=wrap)

    def _list_resized(self, event: tk.Event) -> None:
        if event.width == self._list_width:
            return
        self._list_width = event.width
        self._row_wrap = fitted_wrap(
            self._units(event.width) - ROW_CHECK_WIDTH - ROW_STATUS_WIDTH - 24, ROW_WRAP
        )
        for built in self.rows:
            built.set_wraplength(self._row_wrap)


class ProfilesPanel(ctk.CTkFrame):
    """The Profiles section: header buttons, the intro, the starter cards and the last
    applied profile on the left, and the sheet on the right.

    `mode` is the sheet's ("empty", "loading", "error", "plan", "result", "export") or
    "unsupported" once the section is turned off. Enablement: reads (open, export, preview,
    retry) follow `set_actions_enabled(reads=…)`; Apply also needs `apply`, a loaded engine,
    administrator rights and a checked row; Undo needs `apply` and administrator rights.
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        starters: Sequence[Mapping[str, Any]] = (),
        on_preview_starter: Callable[[dict[str, Any]], None],
        on_open_file: Callable[[], None],
        on_export: Callable[[], None],
        on_apply: Callable[[list[str]], None],
        on_save_export: Callable[[str, str, list[str]], None],
        on_cancel_export: Callable[[], None],
        on_undo: Callable[[], None],
        on_retry: Callable[[], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_preview_starter = on_preview_starter
        self._unsupported = False
        self._reads = True
        self._engine_ready = True
        self._width = 0
        self.starter_cards: list[StarterCard] = []
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(2, weight=1)

        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(1, weight=1)
        ctk.CTkLabel(header, text="Profiles", font=_font(13, "bold"), text_color=theme.INK_SECONDARY).grid(
            row=0, column=0, sticky="w"
        )
        self.export_button = ctk.CTkButton(
            header,
            text="Export this PC's settings…",
            width=200,
            height=30,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_export,
        )
        self.export_button.grid(row=0, column=2, sticky="e", padx=(0, 8))
        self.open_button = ctk.CTkButton(
            header,
            text="Open profile file…",
            width=150,
            height=30,
            font=_font(12, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            command=on_open_file,
        )
        self.open_button.grid(row=0, column=3, sticky="e")

        self.intro_label = ctk.CTkLabel(
            self,
            text=INTRO_TEXT,
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=PANEL_WRAP,
        )
        self.intro_label.grid(row=1, column=0, sticky="ew", padx=14, pady=(4, 0))

        self.body = ctk.CTkFrame(self, fg_color="transparent")
        self.body.grid(row=2, column=0, sticky="nsew", padx=10, pady=(8, 10))
        self.body.grid_columnconfigure(0, minsize=STARTER_COLUMN)
        self.body.grid_columnconfigure(1, weight=1)
        self.body.grid_rowconfigure(0, weight=1)

        self.starters_list = ctk.CTkScrollableFrame(
            self.body,
            width=STARTER_COLUMN,
            fg_color="transparent",
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.starters_list.grid(row=0, column=0, sticky="nsew", padx=(0, 10))
        self.starters_list.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            self.starters_list,
            text="Starter profiles",
            font=_font(12, "bold"),
            text_color=theme.INK_SECONDARY,
        ).grid(row=0, column=0, sticky="w", pady=(0, 6))
        self.last_applied_card = LastAppliedCard(self.starters_list, on_undo=on_undo)

        self.sheet = ProfileSheet(
            self.body,
            on_apply=on_apply,
            on_undo=on_undo,
            on_again=on_retry,
            on_retry=on_retry,
            on_save=on_save_export,
            on_cancel=on_cancel_export,
        )
        self.sheet.grid(row=0, column=1, sticky="nsew")

        self.unsupported_label = ctk.CTkLabel(self, text="", font=_font(12), text_color=theme.WARNING)
        self.bind("<Configure>", self._resized)
        self.set_starters(starters)

    # -- queries ---------------------------------------------------------------------

    @property
    def mode(self) -> str:
        return "unsupported" if self._unsupported else self.sheet.mode

    @property
    def loaded(self) -> bool:
        """Whether a plan or an apply result is shown."""
        return self.mode in ("plan", "result")

    def selected_keys(self) -> list[str]:
        return self.sheet.selected_keys()

    def unchecked_keys(self) -> set[str]:
        return self.sheet.unchecked_keys()

    def export_fields(self) -> tuple[str, str]:
        return self.sheet.export_fields()

    # -- state -----------------------------------------------------------------------

    def set_starters(self, starters: Sequence[Mapping[str, Any]]) -> None:
        for card in self.starter_cards:
            card.destroy()
        self.starter_cards = []
        for index, starter in enumerate(starters):
            if not isinstance(starter, Mapping):
                continue
            card = StarterCard(
                self.starters_list, starter, on_preview=lambda s=dict(starter): self._on_preview_starter(s)
            )
            card.grid(row=index + 1, column=0, sticky="ew", pady=(0, 8))
            self.starter_cards.append(card)
        self._place_last_applied()
        self._apply_state()

    def set_unsupported(self, text: str) -> None:
        """Turns the section off with `text` as the reason (engine missing or outdated)."""
        self._unsupported = True
        self.intro_label.grid_forget()
        self.body.grid_forget()
        self.unsupported_label.configure(text=f"⚠ {text}")
        self.unsupported_label.grid(row=2, column=0, padx=14, pady=40)
        self._apply_state()

    def set_access(self, engine_ready: bool, elevated: bool) -> None:
        self._engine_ready = engine_ready
        self.sheet.set_access(engine_ready, elevated)
        self._apply_state()

    def set_actions_enabled(self, *, reads: bool, apply: bool) -> None:
        self._reads = reads
        self.sheet.set_actions_enabled(reads=reads, apply=apply)
        self.last_applied_card.undo_button.configure(
            state="normal" if apply and self.sheet._elevated else "disabled"
        )
        self._apply_state()

    def set_loading(self, text: str) -> None:
        self.sheet.set_loading(text)

    def show_error(self, text: str, retry: bool = True) -> None:
        self.sheet.show_error(text, retry=retry)

    def show_plan(
        self,
        plan: Mapping[str, Any],
        *,
        source: str,
        keep_unchecked: Iterable[str] = (),
        description: str = "",
    ) -> None:
        self.sheet.show_plan(plan, source=source, keep_unchecked=keep_unchecked, description=description)

    def show_result(self, plan: Mapping[str, Any], report: Mapping[str, Any]) -> None:
        self.sheet.show_result(plan, report)

    def show_export(self, candidates: Mapping[str, Any]) -> None:
        self.sheet.show_export(candidates)

    def show_name_error(self, text: str) -> None:
        self.sheet.show_name_error(text)

    def set_empty(self) -> None:
        self.sheet.set_empty()

    def set_last_applied(self, name: str, when: str, count: int) -> None:
        self.last_applied_card.show(name, when, count)
        self._last_applied_shown = True
        self._place_last_applied()

    def clear_last_applied(self) -> None:
        self._last_applied_shown = False
        self._place_last_applied()

    def mark_current_starter(self, starter_id: str | None) -> None:
        for card in self.starter_cards:
            card.set_current(card.starter_id == starter_id)

    # -- layout ----------------------------------------------------------------------

    def _place_last_applied(self) -> None:
        if getattr(self, "_last_applied_shown", False):
            self.last_applied_card.grid(row=len(self.starter_cards) + 1, column=0, sticky="ew", pady=(4, 8))
        else:
            self.last_applied_card.grid_forget()

    def _apply_state(self) -> None:
        enabled = not self._unsupported and self._engine_ready and self._reads
        state = "normal" if enabled else "disabled"
        self.open_button.configure(state=state)
        self.export_button.configure(state=state)
        for card in self.starter_cards:
            card.preview_button.configure(state=state)

    def _resized(self, event: tk.Event) -> None:
        if event.width == self._width:
            return
        self._width = event.width
        width = event.width / (ctk.ScalingTracker.get_widget_scaling(self) or 1.0)
        wrap = fitted_wrap(width - 28, PANEL_WRAP)
        if self.intro_label.cget("wraplength") != wrap:
            self.intro_label.configure(wraplength=wrap)
        unsupported_wrap = fitted_wrap(width - 28, 560)
        if self.unsupported_label.cget("wraplength") != unsupported_wrap:
            self.unsupported_label.configure(wraplength=unsupported_wrap)
