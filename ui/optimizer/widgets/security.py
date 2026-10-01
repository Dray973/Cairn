"""Security section: the read-only security checkup.

A header card holds the score, its grade, what needs fixing, when the checkup ran and the
"Check again" action, plus the notes of checks that could not run; below it a scrollable list
in three blocks: "To fix" (most severe first), "Could not check" and the collapsed "Passed"
list. The engine renders every text and fix; this module only lays them out and decides which
fix buttons are enabled.
"""

from __future__ import annotations

import tkinter as tk
from collections.abc import Callable, Mapping, Sequence
from datetime import UTC, datetime
from typing import Any

import customtkinter as ctk

from .. import theme
from .history import local_time

TITLE = "Security checkup"
PLACEHOLDER = "Checks how well Windows is protected. Nothing is changed."
LOADING_TEXT = "Checking Windows security…"
READ_ONLY_TEXT = "Read-only: nothing on this PC is changed."
LEFT_OUT_TEXT = "The score leaves out checks that could not run."
PER_USER_NOTE = "Your own settings can only be changed when Cairn runs as your account."
ADMIN_TOOL_NOTE = "Opens only with administrator rights."
BUSY_NOTE = "Wait for the running operation to finish."
STOP_TEXT = "Stop"

# severity -> (chip text, icon, colour) of an attention finding.
SEVERITY_CHIPS: dict[str, tuple[str, str, str]] = {
    "critical": ("Critical", "⚠", theme.CRITICAL),
    "high": ("Important", "⚠", theme.SERIOUS),
    "medium": ("Recommended", "⚠", theme.WARNING),
    "low": ("Optional", "○", theme.INK_SECONDARY),
    "info": ("Info", "–", theme.INK_MUTED),
}
SEVERITY_RANK = {"info": 0, "low": 1, "medium": 2, "high": 3, "critical": 4}
# state -> (icon, colour) of every state but attention.
STATE_STYLES: dict[str, tuple[str, str]] = {
    "good": ("✓", theme.GOOD),
    "unknown": ("○", theme.INK_MUTED),
    "checking": ("◐", theme.ACCENT),
    "not_applicable": ("–", theme.INK_MUTED),
}
GROUP_TITLES = {
    "protection": "Virus & threat protection",
    "network": "Firewall & network",
    "updates": "Windows Update",
    "device": "Device security",
    "accounts": "Accounts & sign-in",
    "apps": "Apps & browser",
}
# grade -> (text, colour).
GRADES = {
    "good": ("✓ Well protected", theme.GOOD),
    "fair": ("⚠ Needs attention", theme.WARNING),
    "at_risk": ("⚠ At risk", theme.CRITICAL),
}
# Windows Security pages that only navigate; pages that start scans, updates or restarts are
# never opened.
SAFE_DEFENDER_PAGES = frozenset(
    {
        "threat",
        "threatsettings",
        "history",
        "network",
        "appbrowser",
        "smartscreenpua",
        "smartapp",
        "devicesecurity",
        "coreisolation",
        "securityprocessor",
        "providers",
        "accountprotection",
        "administratorprotection",
    }
)
BITLOCKER_URI = "shell:::{D9EF8727-CAC2-4E60-809E-86F80A666C91}"
PENDING_UPDATES = "pending_updates"

CARD_PAD = 14
ROW_PAD = 12
FIX_HEIGHT = 28


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def allowed_uri(uri: str) -> bool:
    """Whether a fix may open `uri`: an `ms-settings:` page, a Windows Security page that
    only navigates, or the BitLocker Control Panel page."""
    if uri == BITLOCKER_URI:
        return True
    lower = uri.lower()
    if lower.startswith("windowsdefender://"):
        rest = lower[len("windowsdefender://") :]
        page = rest.replace("?", "/").replace("#", "/").split("/", 1)[0]
        return page in SAFE_DEFENDER_PAGES
    return lower.startswith("ms-settings:") and len(lower) > len("ms-settings:")


def state_style(check: Mapping[str, Any]) -> tuple[str, str]:
    """(icon, colour) of a check: by severity for a finding, by state otherwise."""
    state = str(check.get("state", ""))
    if state == "attention":
        _chip, icon, color = SEVERITY_CHIPS.get(str(check.get("severity")), SEVERITY_CHIPS["medium"])
        return icon, color
    return STATE_STYLES.get(state, STATE_STYLES["unknown"])


def sort_checks(
    checks: Sequence[Mapping[str, Any]],
) -> tuple[list[Mapping[str, Any]], list[Mapping[str, Any]], list[Mapping[str, Any]]]:
    """(to fix, could not check, passed). Findings are sorted by severity, most severe first,
    then in the engine's order; informational findings (severity "info") are listed with the
    passed checks, which keep the engine's order."""
    to_fix: list[tuple[int, int, Mapping[str, Any]]] = []
    not_checked: list[Mapping[str, Any]] = []
    passed: list[Mapping[str, Any]] = []
    for index, check in enumerate(checks):
        state = check.get("state")
        severity = str(check.get("severity", ""))
        if state == "attention" and severity != "info":
            to_fix.append((-SEVERITY_RANK.get(severity, 2), index, check))
        elif state in ("unknown", "checking"):
            not_checked.append(check)
        else:
            passed.append(check)
    to_fix.sort(key=lambda entry: (entry[0], entry[1]))
    return [entry[2] for entry in to_fix], not_checked, passed


def _count(value: Any) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) else 0


def score_texts(score: Mapping[str, Any]) -> tuple[str, str, str, str]:
    """(value, grade text, grade colour, sub line) of the header."""
    value = score.get("value")
    shown = str(value) if isinstance(value, int) and not isinstance(value, bool) else "–"
    grade, color = GRADES.get(str(score.get("grade")), ("", theme.INK_SECONDARY))
    to_fix = _count(score.get("to_fix"))
    unknown = _count(score.get("unknown"))
    parts = [f"{to_fix} to fix" if to_fix else "Nothing to fix"]
    if unknown:
        parts.append(f"{unknown} could not be checked")
        parts.append(LEFT_OUT_TEXT)
    return shown, grade, color, "  ·  ".join(parts)


def meta_text(checkup: Mapping[str, Any]) -> str:
    """The header's meta line: when the checkup ran and how long it took, then the note that
    nothing is changed."""
    parts = []
    taken = checkup.get("taken_at")
    if taken:
        read = f"Checked {local_time(str(taken))}"
        duration = checkup.get("duration_ms")
        if isinstance(duration, int | float) and not isinstance(duration, bool) and duration >= 0:
            seconds = f"{duration / 1000:.1f}"
            read += " in under 0.1 s" if seconds == "0.0" else f" in {seconds} s"
        parts.append(read)
    parts.append(READ_ONLY_TEXT)
    return "  ·  ".join(parts)


def elapsed_text(seconds: float) -> str:
    """A running time: "12 s" below a minute, "1 min 20 s" from one."""
    whole = max(int(seconds), 0)
    if whole < 60:
        return f"{whole} s"
    return f"{whole // 60} min {whole % 60} s"


def _parse_time(stamp: Any) -> datetime | None:
    if not isinstance(stamp, str) or not stamp:
        return None
    try:
        parsed = datetime.fromisoformat(stamp.replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed if parsed.tzinfo is not None else parsed.replace(tzinfo=UTC)


def scan_text(view: Mapping[str, Any], now: datetime) -> str:
    """Progress of a running Windows Update search: "Checking for waiting updates…  ·  12 s"
    or "Checking Windows Update online…  ·  1 min 20 s". The time counts from the search's
    start when it is known, so it moves on between two polls."""
    what = "Checking Windows Update online…" if view.get("online") else "Checking for waiting updates…"
    elapsed_ms = view.get("elapsed_ms")
    seconds = 0.0
    if isinstance(elapsed_ms, int | float) and not isinstance(elapsed_ms, bool):
        seconds = elapsed_ms / 1000
    started = _parse_time(view.get("started_at"))
    if started is not None:
        seconds = max(seconds, (now - started).total_seconds())
    return f"{what}  ·  {elapsed_text(seconds)}"


def fix_state(
    check: Mapping[str, Any],
    fix: Mapping[str, Any],
    *,
    elevated: bool,
    other_user: bool | None,
    busy: bool,
) -> tuple[bool, str]:
    """(enabled, note) of a fix button. A tweak fix changes Windows through the journaled apply
    flow: it waits while another change runs, and a per-user one needs Cairn to run as the
    signed-in user. A tool that needs administrator rights says so when Cairn is not
    elevated (clicking it explains how to get them)."""
    action = fix.get("action") or {}
    kind = action.get("kind") if isinstance(action, Mapping) else None
    if kind == "tweak":
        if check.get("per_user") and other_user is not False:
            return False, PER_USER_NOTE
        if busy:
            return False, BUSY_NOTE
        return True, ""
    if kind == "windows_tool" and action.get("requires_admin") and not elevated:
        return True, ADMIN_TOOL_NOTE
    return True, ""


def passed_prefix(check: Mapping[str, Any]) -> str:
    """The group of a passed check as the muted prefix of its line: "Firewall & network  ·  "."""
    group = GROUP_TITLES.get(str(check.get("group")), "")
    return f"{group}  ·  " if group else ""


def _mappings(value: Any) -> list[Mapping[str, Any]]:
    if not isinstance(value, list | tuple):
        return []
    return [item for item in value if isinstance(item, Mapping)]


class ScaledFrame(ctk.CTkFrame):
    """A frame whose text is plain Tk labels in the theme font at CustomTkinter's widget
    scaling. Many labels build much faster this way; CustomTkinter rescales only its own
    widgets, so the content is built again (`_build`) when the scaling changes."""

    def __init__(self, master: tk.Misc, **kwargs: Any) -> None:
        kwargs.setdefault("fg_color", theme.SURFACE)
        super().__init__(master, **kwargs)
        self._scale = ctk.ScalingTracker.get_widget_scaling(self)
        self._bg = str(kwargs["fg_color"])
        self._content: list[tk.Misc] = []

    def _build(self) -> None:
        raise NotImplementedError

    def _set_scaling(self, new_widget_scaling: float, new_window_scaling: float) -> None:
        super()._set_scaling(new_widget_scaling, new_window_scaling)
        if new_widget_scaling == self._scale:
            return
        self._scale = new_widget_scaling
        self._rebuild()

    def _rebuild(self) -> None:
        for widget in self._content:
            widget.destroy()
        self._content = []
        self._build()

    def _px(self, value: float) -> int:
        return round(value * self._scale)

    def _label(
        self,
        master: tk.Misc,
        text: str,
        size: int,
        color: str,
        *,
        weight: str = "normal",
        wrap: int = 0,
    ) -> tk.Label:
        label = tk.Label(
            master,
            text=text,
            font=(theme.FONT_FAMILY, -self._px(size), weight),
            fg=color,
            bg=self._bg,
            anchor="nw",
            justify="left",
            wraplength=wrap,
            bd=0,
            padx=0,
            pady=0,
            highlightthickness=0,
        )
        self._content.append(label)
        return label


class CheckRow(ScaledFrame):
    """One check. Expanded (findings and checks that could not run): icon, title, severity chip,
    summary, detail, facts, fix buttons and their notes. Compact (passed checks): one line. An
    informational finding is listed with the passed checks (`informational`): its line carries
    the severity chip and its fix buttons, with the detail, facts and fix notes under it.

    `fix_buttons` pairs each fix with its button; `stop_button` replaces the "Check online now"
    button while a Windows Update search runs (`set_scanning`). `chip_label` and `detail_label`
    are the severity chip and the detail of a finding (None when the row has none).
    """

    def __init__(
        self,
        master: tk.Misc,
        check: Mapping[str, Any],
        *,
        compact: bool,
        on_fix: Callable[[Mapping[str, Any], Mapping[str, Any]], None],
        on_stop: Callable[[], None] | None = None,
    ) -> None:
        if compact:
            super().__init__(master, fg_color=theme.SURFACE, corner_radius=0)
        else:
            super().__init__(
                master, fg_color=theme.SURFACE, corner_radius=8, border_width=1, border_color=theme.BORDER
            )
        self.check = check
        self.compact = compact
        # A finding listed with the passed checks: it costs no points but is still explained.
        self.informational = compact and check.get("state") == "attention"
        self.check_id = str(check.get("id", ""))
        self._on_fix = on_fix
        self._on_stop = on_stop
        self.fix_buttons: list[tuple[Mapping[str, Any], ctk.CTkButton]] = []
        self.stop_button: ctk.CTkButton | None = None
        self.chip_label: tk.Label | None = None
        self.detail_label: tk.Label | None = None
        self.scanning: str | None = None
        self._context: dict[str, Any] = {"elevated": True, "other_user": False, "busy": False}
        self._wrapped: list[tk.Label] = []
        # Pixels the wrapped lines start right of the row's padding.
        self._wrap_inset = 0
        self._notes_label: tk.Label | None = None
        self._notes_grid: dict[str, Any] = {}
        self.icon_label: tk.Label
        self.title_label: tk.Label
        self.summary_label: tk.Label
        self.grid_columnconfigure(0, weight=1)
        self._build()
        if not compact or self.informational:
            self.bind("<Configure>", self._on_resize, add="+")

    # -- layout ------------------------------------------------------------------------

    def _build(self) -> None:
        self.fix_buttons = []
        self.stop_button = None
        self.chip_label = None
        self.detail_label = None
        self._wrapped = []
        self._wrap_inset = 0
        self._notes_label = None
        if self.compact:
            self._build_compact()
        else:
            self._build_expanded()
        self._apply_scanning()
        self._refresh_fixes()

    def _build_compact(self) -> None:
        check = self.check
        pad = self._px(ROW_PAD)
        icon, color = state_style(check)
        muted = check.get("state") != "good"
        line = tk.Frame(self, bg=self._bg)
        self._content.append(line)
        line.grid(row=0, column=0, sticky="ew", padx=pad, pady=self._px(3))
        self.icon_label = self._label(line, icon, 11, color)
        self.icon_label.pack(side="left", padx=(0, self._px(8)))
        self._label(line, passed_prefix(check), 11, theme.INK_MUTED).pack(side="left")
        self.title_label = self._label(
            line, str(check.get("title", "")), 11, theme.INK_MUTED if muted else theme.INK
        )
        self.title_label.pack(side="left", padx=(0, self._px(12)))
        if self.informational:
            self.chip_label = self._chip(line)
            self.chip_label.pack(side="left", padx=(0, self._px(12)))
        self.summary_label = self._label(
            line, str(check.get("summary", "")), 11, theme.INK_MUTED if muted else theme.INK_SECONDARY
        )
        self.summary_label.pack(side="left")
        fixes = _mappings(check.get("fixes"))
        if not self.informational:
            # A passed line keeps only the Windows Update search, so it can always be run again.
            fixes = [fix for fix in fixes if self._is_scan_fix(fix)]
        if fixes:
            self.grid_columnconfigure(1, weight=0)
            buttons = self._fix_bar(fixes, height=24, accent=not self.informational)
            buttons.grid(row=0, column=1, sticky="e", padx=(0, pad), pady=self._px(2))
        if not self.informational:
            return
        # What explains the finding starts under the line's text, right of the icon.
        self._wrap_inset = self.icon_label.winfo_reqwidth() + self._px(8)
        left = pad + self._wrap_inset
        row = self._detail_rows(1, left=left, gap=0)
        if fixes:
            row = self._notes_row(row, left=left)
        self.grid_rowconfigure(row, minsize=self._px(3))

    def _chip(self, master: tk.Misc) -> tk.Label:
        """The severity chip of a finding; the caller places it."""
        chip, _icon, color = SEVERITY_CHIPS.get(str(self.check.get("severity")), SEVERITY_CHIPS["medium"])
        return self._label(master, chip, 10, color, weight="bold")

    def _detail_rows(self, row: int, *, left: int, gap: int = 4) -> int:
        """Grids the check's detail and its facts from grid row `row` on, `left` pixels from the
        row's edge; returns the next free grid row."""
        pad = self._px(ROW_PAD)
        detail = str(self.check.get("detail") or "")
        if detail:
            self.detail_label = self._label(self, detail, 10, theme.INK_MUTED)
            self.detail_label.grid(
                row=row, column=0, columnspan=2, sticky="ew", padx=(left, pad), pady=(self._px(gap), 0)
            )
            self._wrapped.append(self.detail_label)
            row += 1
        for fact in _mappings(self.check.get("facts")):
            name = str(fact.get("label") or "")
            value = str(fact.get("value") or "")
            label = self._label(self, f"{name}: {value}" if name else value, 10, theme.INK_SECONDARY)
            label.grid(row=row, column=0, columnspan=2, sticky="ew", padx=(left, pad), pady=(self._px(2), 0))
            self._wrapped.append(label)
            row += 1
        return row

    def _notes_row(self, row: int, *, left: int) -> int:
        """Reserves grid row `row` for the notes of the fix buttons (shown only when there are
        any); returns the next free grid row."""
        self._notes_label = self._label(self, "", 10, theme.INK_MUTED)
        self._notes_grid = {
            "row": row,
            "column": 0,
            "columnspan": 2,
            "sticky": "ew",
            "padx": (left, self._px(ROW_PAD)),
            "pady": (self._px(4), 0),
        }
        self._wrapped.append(self._notes_label)
        return row + 1

    def _fix_bar(
        self, fixes: list[Mapping[str, Any]], *, height: int = FIX_HEIGHT, accent: bool = True
    ) -> ctk.CTkFrame:
        """A row of fix buttons (the first one accented unless `accent` is off) and, for a
        Windows Update search fix, the Stop button that replaces it while a search runs; the
        caller places it."""
        buttons = ctk.CTkFrame(self, fg_color="transparent")
        self._content.append(buttons)
        for index, fix in enumerate(fixes):
            first = accent and index == 0
            button = ctk.CTkButton(
                buttons,
                text=str(fix.get("label", "")),
                height=height,
                width=0,
                font=_font(11, "bold" if first else "normal"),
                fg_color=theme.ACCENT if first else theme.BUTTON_NEUTRAL,
                hover_color=theme.ACCENT_HOVER if first else theme.BUTTON_NEUTRAL_HOVER,
                command=lambda f=fix: self._on_fix(self.check, f),
            )
            button.grid(row=0, column=index, padx=(0, 8))
            self.fix_buttons.append((fix, button))
        if self._on_stop is not None and any(self._is_scan_fix(fix) for fix in fixes):
            self.stop_button = ctk.CTkButton(
                buttons,
                text=STOP_TEXT,
                height=height,
                width=80,
                font=_font(11, "bold"),
                fg_color=theme.BUTTON_NEUTRAL,
                hover_color=theme.BUTTON_NEUTRAL_HOVER,
                command=self._on_stop,
            )
        return buttons

    def _build_expanded(self) -> None:
        check = self.check
        pad = self._px(ROW_PAD)
        icon, color = state_style(check)
        head = tk.Frame(self, bg=self._bg)
        self._content.append(head)
        head.grid(row=0, column=0, sticky="ew", padx=pad, pady=(self._px(10), 0))
        self.icon_label = self._label(head, icon, 12, color, weight="bold")
        self.icon_label.pack(side="left", padx=(0, self._px(8)))
        self.title_label = self._label(head, str(check.get("title", "")), 12, theme.INK, weight="bold")
        self.title_label.pack(side="left")
        if check.get("state") == "attention":
            self.chip_label = self._chip(head)
            self.chip_label.pack(side="left", padx=(self._px(10), 0))
        self.summary_label = self._label(self, str(check.get("summary", "")), 11, theme.INK_SECONDARY)
        self.summary_label.grid(row=1, column=0, sticky="ew", padx=pad, pady=(self._px(2), 0))
        self._wrapped.append(self.summary_label)
        row = self._detail_rows(2, left=pad)
        fixes = _mappings(check.get("fixes"))
        if fixes:
            buttons = self._fix_bar(fixes)
            buttons.grid(row=row, column=0, sticky="w", padx=pad, pady=(self._px(8), 0))
            row = self._notes_row(row + 1, left=pad)
        # Bottom margin.
        self.grid_rowconfigure(row, minsize=self._px(10))

    @staticmethod
    def _is_scan_fix(fix: Mapping[str, Any]) -> bool:
        action = fix.get("action")
        return isinstance(action, Mapping) and action.get("kind") == "update_scan"

    def _on_resize(self, event: tk.Event) -> None:
        wrap = max(event.width - 2 * self._px(ROW_PAD) - self._px(4) - self._wrap_inset, self._px(120))
        for label in self._wrapped:
            if label.winfo_exists() and int(label.cget("wraplength")) != wrap:
                label.configure(wraplength=wrap)

    # -- state -------------------------------------------------------------------------

    def set_context(self, *, elevated: bool, other_user: bool | None, busy: bool) -> None:
        """What the fix buttons depend on: elevation, the account and the busy state."""
        self._context = {"elevated": elevated, "other_user": other_user, "busy": busy}
        self._refresh_fixes()

    @property
    def notes(self) -> str:
        """The notes shown under the fix buttons."""
        return str(self._notes_label.cget("text")) if self._notes_label is not None else ""

    def _refresh_fixes(self) -> None:
        notes: list[str] = []
        for fix, button in self.fix_buttons:
            enabled, note = fix_state(self.check, fix, **self._context)
            button.configure(state="normal" if enabled else "disabled")
            for text in (note, str(fix.get("note") or "")):
                if text and text not in notes:
                    notes.append(text)
        if self._notes_label is None:
            return
        self._notes_label.configure(text="\n".join(notes))
        if notes:
            self._notes_label.grid(**self._notes_grid)
        else:
            self._notes_label.grid_forget()

    def set_scanning(self, text: str | None) -> None:
        """Shows a running Windows Update search on this row (`text`), with Stop in place of
        "Check online now", or the check's own state again (None)."""
        self.scanning = text
        self._apply_scanning()

    def _apply_scanning(self) -> None:
        text = self.scanning
        if text is not None:
            self.icon_label.configure(text="◐", fg=theme.ACCENT)
            self.summary_label.configure(text=text)
        else:
            icon, color = state_style(self.check)
            self.icon_label.configure(text=icon, fg=color)
            self.summary_label.configure(text=str(self.check.get("summary", "")))
        for index, (fix, button) in enumerate(self.fix_buttons):
            if not self._is_scan_fix(fix):
                continue
            if text is not None:
                button.grid_forget()
                if self.stop_button is not None:
                    self.stop_button.grid(row=0, column=index, padx=(0, 8))
            else:
                if self.stop_button is not None:
                    self.stop_button.grid_forget()
                button.grid(row=0, column=index, padx=(0, 8))


class SecurityPanel(ctk.CTkFrame):
    """Header card with the score above the checks in three blocks.

    The first checkup runs when the section is first shown; "Check again" runs a new one while
    the current rows stay. `rows` maps each check id to its row; `passed_button` shows or
    hides the passed checks.
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_refresh: Callable[[], None],
        on_fix: Callable[[Mapping[str, Any], Mapping[str, Any]], None],
        on_elevate: Callable[[], None],
        on_stop: Callable[[], None] | None = None,
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_fix = on_fix
        self._on_stop = on_stop
        self.loaded = False
        self.loading = False
        self.checkup: Mapping[str, Any] | None = None
        self.rows: dict[str, CheckRow] = {}
        self.show_passed = False
        self.scan_view: Mapping[str, Any] | None = None
        self._elevated = True
        self._busy = False
        self._unsupported = False
        self._meta = ""
        self._passed_row = 5
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(1, weight=1)

        header = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        header.grid(row=0, column=0, sticky="ew", pady=(0, 8))
        header.grid_columnconfigure(0, weight=1)
        self.header = header
        ctk.CTkLabel(
            header, text=TITLE, font=_font(13, "bold"), text_color=theme.INK_SECONDARY, anchor="w"
        ).grid(row=0, column=0, sticky="w", padx=CARD_PAD, pady=(10, 0))
        self.refresh_button = ctk.CTkButton(
            header,
            text="Check again",
            width=110,
            height=30,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_refresh,
        )
        self.refresh_button.grid(row=0, column=1, rowspan=2, sticky="ne", padx=CARD_PAD, pady=10)
        score_row = ctk.CTkFrame(header, fg_color="transparent")
        score_row.grid(row=1, column=0, sticky="w", padx=CARD_PAD)
        self.value_label = ctk.CTkLabel(
            score_row, text="", font=_font(30, "bold"), text_color=theme.INK, anchor="w"
        )
        self.grade_label = ctk.CTkLabel(score_row, text="", font=_font(13, "bold"), anchor="w")
        self.sub_label = ctk.CTkLabel(
            header,
            text=PLACEHOLDER,
            font=_font(11),
            text_color=theme.INK_SECONDARY,
            anchor="w",
            justify="left",
        )
        self.sub_label.grid(row=2, column=0, columnspan=2, sticky="w", padx=CARD_PAD)
        self.meta_label = ctk.CTkLabel(
            header, text="", font=_font(10), text_color=theme.INK_MUTED, anchor="w", justify="left"
        )
        self.meta_label.grid(row=3, column=0, columnspan=2, sticky="w", padx=CARD_PAD, pady=(0, 8))
        self.note_row = ctk.CTkFrame(header, fg_color="transparent")
        self.note_row.grid_columnconfigure(0, weight=1)
        self.note_label = ctk.CTkLabel(
            self.note_row, text="", font=_font(10), text_color=theme.WARNING, anchor="w", justify="left"
        )
        self.note_label.grid(row=0, column=0, sticky="w")
        self.elevate_button = ctk.CTkButton(
            self.note_row,
            text="Restart as administrator",
            width=170,
            height=26,
            font=_font(11, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            command=on_elevate,
        )
        header.bind("<Configure>", self._on_header_resize, add="+")

        self.body = ctk.CTkScrollableFrame(
            self,
            fg_color="transparent",
            height=80,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.body.grid(row=1, column=0, sticky="nsew")
        self.body.grid_columnconfigure(0, weight=1)
        self.fix_heading = self._heading()
        self.fix_frame = self._block()
        self.unknown_heading = self._heading()
        self.unknown_frame = self._block()
        self.passed_head = ctk.CTkFrame(self.body, fg_color="transparent")
        self.passed_head.grid_columnconfigure(0, weight=1)
        self.passed_heading = ctk.CTkLabel(
            self.passed_head, text="", font=_font(12, "bold"), text_color=theme.INK_SECONDARY, anchor="w"
        )
        self.passed_heading.grid(row=0, column=0, sticky="w")
        self.passed_button = ctk.CTkButton(
            self.passed_head,
            text="",
            width=0,
            height=26,
            font=_font(11),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=self.toggle_passed,
        )
        self.passed_button.grid(row=0, column=1, sticky="e")
        self.passed_frame = self._block(card=True)

    # -- building blocks ---------------------------------------------------------------

    def _heading(self) -> ctk.CTkLabel:
        return ctk.CTkLabel(
            self.body, text="", font=_font(12, "bold"), text_color=theme.INK_SECONDARY, anchor="w"
        )

    def _block(self, *, card: bool = False) -> ctk.CTkFrame:
        if card:
            frame = ctk.CTkFrame(
                self.body, fg_color=theme.SURFACE, corner_radius=8, border_width=1, border_color=theme.BORDER
            )
        else:
            frame = ctk.CTkFrame(self.body, fg_color="transparent")
        frame.grid_columnconfigure(0, weight=1)
        return frame

    def _on_header_resize(self, event: tk.Event) -> None:
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        wrap = max(int(event.width / scale) - 2 * CARD_PAD - 8, 200)
        for label in (self.sub_label, self.meta_label):
            if label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)
        note_wrap = max(wrap - 190, 160)
        if self.note_label.cget("wraplength") != note_wrap:
            self.note_label.configure(wraplength=note_wrap)

    # -- states ------------------------------------------------------------------------

    def set_loading(self) -> None:
        """Marks a checkup in progress; the current rows stay until the new one arrives."""
        self.loading = True
        self.refresh_button.configure(state="disabled")
        self.meta_label.configure(text=LOADING_TEXT)

    def show(self, checkup: Mapping[str, Any], *, elevated: bool) -> None:
        """Replaces the rows with the checks of `checkup`; malformed data is reported with
        `show_error` and the current rows stay."""
        checks = checkup.get("checks") if isinstance(checkup, Mapping) else None
        score = checkup.get("score") if isinstance(checkup, Mapping) else None
        if not isinstance(checks, list | tuple) or not isinstance(score, Mapping):
            self.show_error("the engine returned no checks")
            return
        self.checkup = checkup
        self._elevated = elevated
        self.loading = False
        self.loaded = True
        value, grade, color, sub = score_texts(score)
        self.value_label.configure(text=value)
        self.grade_label.configure(text=grade, text_color=color)
        self.value_label.grid(row=0, column=0, sticky="w")
        self.grade_label.grid(row=0, column=1, sticky="w", padx=(10, 0))
        self.sub_label.configure(text=sub, text_color=theme.INK_SECONDARY)
        self._meta = meta_text(checkup)
        self.meta_label.configure(text=self._meta)
        self._show_notes([str(n) for n in checkup.get("notes") or [] if n])
        self._show_rows(_mappings(checks))
        self._enable_refresh()

    def _show_notes(self, notes: list[str]) -> None:
        if not notes:
            self.note_row.grid_forget()
            return
        self.note_label.configure(text="\n".join(f"⚠ {note}" for note in notes))
        self.note_row.grid(row=4, column=0, columnspan=2, sticky="ew", padx=CARD_PAD, pady=(0, 10))
        if self._elevated:
            self.elevate_button.grid_forget()
        else:
            self.elevate_button.grid(row=0, column=1, sticky="e", padx=(10, 0))

    def _show_rows(self, checks: list[Mapping[str, Any]]) -> None:
        for frame in (self.fix_frame, self.unknown_frame, self.passed_frame):
            for child in frame.winfo_children():
                child.destroy()
        self.rows = {}
        to_fix, not_checked, passed = sort_checks(checks)
        next_row = 0
        for heading, frame, listed, title in (
            (self.fix_heading, self.fix_frame, to_fix, "To fix"),
            (self.unknown_heading, self.unknown_frame, not_checked, "Could not check"),
        ):
            if not listed:
                heading.grid_forget()
                frame.grid_forget()
                continue
            heading.configure(text=f"{title} ({len(listed)})")
            heading.grid(row=next_row, column=0, sticky="w", pady=(4 if next_row else 0, 6))
            frame.grid(row=next_row + 1, column=0, sticky="ew")
            next_row += 2
            for index, check in enumerate(listed):
                row = CheckRow(frame, check, compact=False, on_fix=self._on_fix, on_stop=self._on_stop)
                row.grid(row=index, column=0, sticky="ew", pady=(0, 8))
                self._add_row(row)
        if not passed:
            self.passed_head.grid_forget()
            self.passed_frame.grid_forget()
            return
        self.passed_heading.configure(text=f"Passed ({len(passed)})")
        self.passed_head.grid(row=next_row, column=0, sticky="ew", pady=(4, 6))
        self._passed_row = next_row + 1
        for index, check in enumerate(passed):
            row = CheckRow(self.passed_frame, check, compact=True, on_fix=self._on_fix, on_stop=self._on_stop)
            row.grid(row=index, column=0, sticky="ew", padx=2, pady=(6 if index == 0 else 0, 0))
            self._add_row(row)
        self.passed_frame.grid_rowconfigure(len(passed), minsize=6)
        self._show_passed_block()

    def _add_row(self, row: CheckRow) -> None:
        self.rows[row.check_id] = row
        row.set_context(elevated=self._elevated, other_user=self._other_user(), busy=self._busy)
        if row.check_id == PENDING_UPDATES:
            self._apply_scan(row)

    def _other_user(self) -> bool | None:
        value = self.checkup.get("other_user") if self.checkup is not None else False
        return value if isinstance(value, bool) else None

    @property
    def passed_count(self) -> int:
        return sum(1 for row in self.rows.values() if row.compact)

    def toggle_passed(self) -> None:
        """Shows or hides the passed checks."""
        self.show_passed = not self.show_passed
        self._show_passed_block()

    def _show_passed_block(self) -> None:
        if self.show_passed:
            self.passed_button.configure(text="Hide passed checks")
            self.passed_frame.grid(row=self._passed_row, column=0, sticky="ew")
        else:
            count = self.passed_count
            self.passed_button.configure(text=f"Show {count} passed check{'' if count == 1 else 's'}")
            self.passed_frame.grid_forget()

    def show_error(self, message: str) -> None:
        """Reports a failed checkup; the rows of an earlier one stay."""
        self.loading = False
        self.sub_label.configure(
            text=f"Could not check Windows security: {message}", text_color=theme.CRITICAL
        )
        self.meta_label.configure(text=self._meta)
        self._enable_refresh()

    def set_unsupported(self, text: str) -> None:
        """Disables the section for an engine build without the checkup."""
        self._unsupported = True
        self.refresh_button.configure(state="disabled")
        self.sub_label.configure(text=f"⚠ {text}", text_color=theme.WARNING)
        self.meta_label.configure(text="")

    def _enable_refresh(self) -> None:
        self.refresh_button.configure(state="disabled" if self._unsupported else "normal")

    def update_scan(self, view: Mapping[str, Any] | None) -> None:
        """Shows the state of the Windows Update search on the "Waiting updates" row."""
        self.scan_view = view
        row = self.rows.get(PENDING_UPDATES)
        if row is not None:
            self._apply_scan(row)

    def _apply_scan(self, row: CheckRow) -> None:
        view = self.scan_view
        if view is not None and view.get("state") == "running":
            row.set_scanning(scan_text(view, datetime.now(UTC)))
        else:
            row.set_scanning(None)

    def set_actions_enabled(self, enabled: bool) -> None:
        """Enables or disables the fixes that change Windows (the window is busy)."""
        self._busy = not enabled
        other_user = self._other_user()
        for row in self.rows.values():
            row.set_context(elevated=self._elevated, other_user=other_user, busy=self._busy)
