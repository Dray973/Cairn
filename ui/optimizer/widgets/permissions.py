"""Permissions section: a guide to the camera, microphone and location permissions of apps.

Windows 11 manages these permissions itself, in Settings › Privacy & security, and on this version
of Windows an app like Cairn cannot change them. So the section changes nothing and shows no
permission state. Each capability gets a card: a short explanation, a button that opens its page
in Windows Settings, and the desktop apps that used the device recently, read-only, from Windows'
own usage record (the engine's `permissions_list`).

The pure helpers at the top shape the engine's guide for display and need no window.
"""

from __future__ import annotations

import logging
import tkinter as tk
from collections.abc import Callable, Mapping, Sequence
from typing import Any

import customtkinter as ctk

from .. import APP_NAME, theme
from .controls import PANEL_WRAP, ROW_WRAP, WRAP_MARGIN, fitted_wrap
from .history import CAPABILITY_LABELS, local_time

__all__ = [
    "BUTTON_TEXT",
    "CAPABILITIES",
    "CAPABILITY_LABELS",
    "CAPABILITY_NOUNS",
    "GUIDE_TEXT",
    "SETTINGS_PAGES",
    "GuideCard",
    "PermissionsPanel",
    "RecentRow",
    "banner_text",
    "button_text",
    "guide_text",
    "recent_time",
    "settings_page",
]

log = logging.getLogger(__name__)

CAPABILITIES = ("camera", "microphone", "location")
CAPABILITY_NOUNS = {"camera": "camera", "microphone": "microphone", "location": "location"}
# capability -> its page in Windows Settings (Privacy & security); the only pages the section opens.
SETTINGS_PAGES = {
    "camera": "ms-settings:privacy-webcam",
    "microphone": "ms-settings:privacy-microphone",
    "location": "ms-settings:privacy-location",
}
PANEL_TITLE = "App permissions"
INTRO_TEXT = (
    "Windows Settings decides which apps may use the camera, the microphone and your location. Each "
    "card below opens its page there and lists the desktop apps that used the device recently."
)
GUIDE_TEXT = (
    "Windows 11 manages which apps may use your {noun} itself, in Settings › Privacy & security › "
    "{label}. On this version of Windows an app like " + APP_NAME + " cannot change these permissions."
)
BUTTON_TEXT = "Open {noun} settings"
RECENT_HEADING = "Recently used by"
RECENT_SOURCE = "Windows' own record of the desktop apps that used the {noun}. Read-only."
NO_RECENT_TEXT = "Windows has no record of a desktop app using the {noun}."
RECENT_LOADING_TEXT = "Reading Windows' record…"
RECENT_ERROR_TEXT = "Windows' record could not be read."
RECENT_UNAVAILABLE_TEXT = "Not available."
LOADING_TEXT = "Reading Windows' record of recent use…"
ERROR_TEXT = "Could not read Windows' record of recent use: {message}"
IN_USE_TEXT = "◐ in use now"
# Banner lines shown at most (read warnings), then "…and N more".
MAX_BANNER_LINES = 3
# Longest desktop program path shown, in characters.
PATH_LIMIT = 110
# Least height of a line of the recent list, its heading and its notes; a wrapped line grows.
ROW_HEIGHT = 20


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def _shorten(text: str, limit: int = PATH_LIMIT) -> str:
    return text if len(text) <= limit else text[: limit - 1] + "…"


def settings_page(capability: str) -> str | None:
    """The Windows Settings page of `capability`, None for anything else."""
    return SETTINGS_PAGES.get(capability)


def button_text(capability: str) -> str:
    """The text of the button that opens the capability's Settings page."""
    return BUTTON_TEXT.format(noun=CAPABILITY_NOUNS[capability])


def guide_text(capability: str) -> str:
    """The card's explanation: what manages the capability's permissions, and that this app
    cannot change them."""
    return GUIDE_TEXT.format(noun=CAPABILITY_NOUNS[capability], label=CAPABILITY_LABELS[capability])


def recent_time(use: Mapping[str, Any]) -> str:
    """When a desktop program used the device: in use now, its last use in local time, or "" when
    Windows' record holds no time."""
    if use.get("in_use"):
        return IN_USE_TEXT
    if use.get("last_used"):
        return f"last used {local_time(str(use['last_used']))}"
    return ""


def banner_text(report: Mapping[str, Any]) -> str:
    """Warning banner of the guide: the usage records that could not be read. "" when there is
    nothing to say."""
    lines = [str(w) for w in report.get("warnings") or []]
    shown = lines[:MAX_BANNER_LINES]
    text = "\n".join(f"⚠ {line}" for line in shown)
    if len(lines) > len(shown):
        text += f"\n⚠ …and {len(lines) - len(shown)} more"
    return text


class RecentRow(ctk.CTkFrame):
    """A desktop program that used the device: its path and when. Read-only."""

    TIME_WIDTH = 150
    PATH_PADX = (14, 6)
    TIME_PADX = (6, 12)

    def __init__(self, master: tk.Misc, use: Mapping[str, Any], *, wraplength: int = ROW_WRAP) -> None:
        super().__init__(master, fg_color="transparent")
        self.use = use
        self._wraplength = wraplength
        self.grid_columnconfigure(0, weight=1)
        self.path_label = ctk.CTkLabel(
            self,
            text=_shorten(str(use.get("path", ""))),
            font=_font(10),
            text_color=theme.INK,
            height=ROW_HEIGHT,
            anchor="w",
            justify="left",
            wraplength=wraplength,
        )
        self.path_label.grid(row=0, column=0, sticky="w", padx=self.PATH_PADX, pady=1)
        self.time_label = ctk.CTkLabel(
            self,
            text=recent_time(use),
            font=_font(10),
            text_color=theme.INK_MUTED,
            width=self.TIME_WIDTH,
            height=ROW_HEIGHT,
            anchor="e",
        )
        self.time_label.grid(row=0, column=1, sticky="e", padx=self.TIME_PADX, pady=1)

    def set_wraplength(self, wraplength: int) -> None:
        if wraplength != self._wraplength:
            self._wraplength = wraplength
            self.path_label.configure(wraplength=wraplength)


class GuideCard(ctk.CTkFrame):
    """One capability: what manages its permissions, a button that opens its Settings page, and
    the desktop apps Windows recorded using the device. The button needs no engine and changes
    nothing in this app, so it is always usable."""

    TEXT_PADX = 14
    BUTTON_WIDTH = 190
    # Horizontal padding of a card in its list.
    ROW_PADX = 6
    # Width of a list that a card's text does not get.
    TEXT_INSET = 2 * ROW_PADX + 2 * TEXT_PADX + WRAP_MARGIN
    # Width of a list that a recent program's path does not get.
    PATH_INSET = (
        2 * ROW_PADX
        + sum(RecentRow.PATH_PADX)
        + RecentRow.TIME_WIDTH
        + sum(RecentRow.TIME_PADX)
        + WRAP_MARGIN
    )

    def __init__(
        self,
        master: tk.Misc,
        capability: str,
        *,
        on_open_settings: Callable[[str], None],
        text_wrap: int = PANEL_WRAP,
        path_wrap: int = ROW_WRAP,
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.capability = capability
        self._on_open_settings = on_open_settings
        self._text_wrap = text_wrap
        self._path_wrap = path_wrap
        # What the recent list shows: the uses as (path, time) pairs, or a note.
        self._shown: tuple[Any, ...] | None = None
        self.grid_columnconfigure(0, weight=1)

        self.title_label = ctk.CTkLabel(
            self,
            text=CAPABILITY_LABELS[capability],
            font=_font(13, "bold"),
            text_color=theme.INK,
            anchor="w",
        )
        self.title_label.grid(row=0, column=0, sticky="w", padx=(self.TEXT_PADX, 6), pady=(10, 0))
        self.button = ctk.CTkButton(
            self,
            text=button_text(capability),
            width=self.BUTTON_WIDTH,
            height=30,
            font=_font(12),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            command=self._open,
        )
        self.button.grid(row=0, column=1, sticky="e", padx=(6, 12), pady=(10, 0))
        self.text_label = ctk.CTkLabel(
            self,
            text=guide_text(capability),
            font=_font(11),
            text_color=theme.INK_SECONDARY,
            anchor="w",
            justify="left",
            wraplength=text_wrap,
        )
        self.text_label.grid(row=1, column=0, columnspan=2, sticky="w", padx=self.TEXT_PADX, pady=(4, 8))
        self.recent_heading = ctk.CTkLabel(
            self,
            text=RECENT_HEADING,
            font=_font(12, "bold"),
            text_color=theme.INK_SECONDARY,
            height=ROW_HEIGHT,
            anchor="w",
        )
        self.recent_heading.grid(row=2, column=0, columnspan=2, sticky="w", padx=self.TEXT_PADX)
        self.source_label = ctk.CTkLabel(
            self,
            text=RECENT_SOURCE.format(noun=CAPABILITY_NOUNS[capability]),
            font=_font(10),
            text_color=theme.INK_MUTED,
            height=ROW_HEIGHT,
            anchor="w",
            justify="left",
            wraplength=text_wrap,
        )
        self.source_label.grid(row=3, column=0, columnspan=2, sticky="w", padx=self.TEXT_PADX, pady=(0, 4))
        self.recent = ctk.CTkFrame(self, fg_color="transparent")
        self.recent.grid(row=4, column=0, columnspan=2, sticky="ew", pady=(0, 8))
        self.recent.grid_columnconfigure(0, weight=1)
        self.recent_rows: list[RecentRow] = []
        self.note_label: ctk.CTkLabel | None = None

    @property
    def wrapped_labels(self) -> list[ctk.CTkLabel]:
        """The card's labels that wrap to the width it has."""
        labels = [self.text_label, self.source_label]
        if self.note_label is not None:
            labels.append(self.note_label)
        return labels + [row.path_label for row in self.recent_rows]

    def show_recent(self, uses: Sequence[Mapping[str, Any]]) -> None:
        """Lists `uses` (newest first, as the engine orders them), or says that Windows has no
        record. Rows are rebuilt only when what they show changed."""
        shown = tuple((str(u.get("path", "")), recent_time(u)) for u in uses)
        if shown == self._shown:
            return
        self._clear()
        self._shown = shown
        if not uses:
            self._note(NO_RECENT_TEXT.format(noun=CAPABILITY_NOUNS[self.capability]))
            return
        for row, use in enumerate(uses):
            widget = RecentRow(self.recent, use, wraplength=self._path_wrap)
            widget.grid(row=row, column=0, sticky="ew")
            self.recent_rows.append(widget)

    def show_note(self, text: str) -> None:
        """Shows `text` in place of the list, such as why it could not be read."""
        if self._shown == (text,):
            return
        self._clear()
        self._shown = (text,)
        self._note(text)

    def set_wraplength(self, text_wrap: int, path_wrap: int) -> None:
        """Wraps the explanation and notes at `text_wrap`, the program paths at `path_wrap`."""
        if text_wrap != self._text_wrap:
            self._text_wrap = text_wrap
            for label in (self.text_label, self.source_label, self.note_label):
                if label is not None:
                    label.configure(wraplength=text_wrap)
        if path_wrap != self._path_wrap:
            self._path_wrap = path_wrap
            for row in self.recent_rows:
                row.set_wraplength(path_wrap)

    def _note(self, text: str) -> None:
        self.note_label = ctk.CTkLabel(
            self.recent,
            text=text,
            font=_font(10),
            text_color=theme.INK_MUTED,
            height=ROW_HEIGHT,
            anchor="w",
            justify="left",
            wraplength=self._text_wrap,
        )
        self.note_label.grid(row=0, column=0, sticky="w", padx=self.TEXT_PADX)

    def _clear(self) -> None:
        for row in self.recent_rows:
            row.destroy()
        self.recent_rows.clear()
        if self.note_label is not None:
            self.note_label.destroy()
            self.note_label = None
        self._shown = None

    def _open(self) -> None:
        self._on_open_settings(self.capability)


class PermissionsPanel(ctk.CTkFrame):
    """The Permissions section: a title with Refresh, a summary line, a warning banner and one
    card per capability.

    The cards and their Settings buttons exist from the start and need no engine; `show` fills
    in the desktop apps that used each device. Refresh is enabled in `_apply_state`. Wrapped text
    follows the width, as in the Optimize list.
    """

    # Width of the panel that the summary and banner do not get (padding and margin).
    PANEL_TEXT_INSET = 2 * 14 + WRAP_MARGIN

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_refresh: Callable[[], None],
        on_open_settings: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(3, weight=1)

        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(header, text=PANEL_TITLE, font=_font(13, "bold"), text_color=theme.INK_SECONDARY).grid(
            row=0, column=0, sticky="w"
        )
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
        self.refresh_button.grid(row=0, column=1, sticky="e")

        self.summary = ctk.CTkLabel(
            self,
            text=INTRO_TEXT,
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=PANEL_WRAP,
        )
        self.summary.grid(row=1, column=0, sticky="ew", padx=14, pady=(4, 4))
        self.banner = ctk.CTkLabel(
            self,
            text="",
            font=_font(11),
            text_color=theme.WARNING,
            anchor="w",
            justify="left",
            wraplength=PANEL_WRAP,
        )
        self.list = ctk.CTkScrollableFrame(
            self,
            fg_color=theme.SURFACE,
            corner_radius=10,
            border_width=1,
            border_color=theme.BORDER,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.list.grid(row=3, column=0, sticky="nsew", padx=4, pady=(0, 6))
        self.list.grid_columnconfigure(0, weight=1)
        self.cards: dict[str, GuideCard] = {}
        for row, capability in enumerate(CAPABILITIES):
            card = GuideCard(self.list, capability, on_open_settings=on_open_settings)
            card.grid(row=row, column=0, sticky="ew", padx=GuideCard.ROW_PADX, pady=(6 if row == 0 else 3, 3))
            self.cards[capability] = card

        # A report was shown or failed to read.
        self.loaded = False
        self.loading = False
        self.supported = True
        self._report: dict[str, Any] | None = None
        self._actions_enabled = True

        # Wrap lengths for the current width.
        self._panel_width = 0
        self._list_width = 0
        self.bind("<Configure>", self._panel_resized)
        # The list's inner frame is as wide as the visible list; CustomTkinter binds its own
        # handler there, so this one is added next to it.
        self.list.bind("<Configure>", self._list_resized, add="+")
        self._apply_state()

    # -- state -----------------------------------------------------------------------

    @property
    def report(self) -> dict[str, Any] | None:
        """The guide last shown, None before the first and after a failed read."""
        return self._report

    @property
    def recent_rows(self) -> list[RecentRow]:
        """Every listed desktop program, card by card."""
        return [row for card in self.cards.values() for row in card.recent_rows]

    @property
    def wrapped_labels(self) -> list[ctk.CTkLabel]:
        """Every label of the panel that wraps to the width it has."""
        labels = [self.summary, self.banner]
        for card in self.cards.values():
            labels += card.wrapped_labels
        return labels

    def set_loading(self) -> None:
        """A read is in flight: Refresh waits for it. Lists already shown stay until it returns."""
        self.loading = True
        self.summary.configure(text=LOADING_TEXT, text_color=theme.INK_MUTED)
        if self._report is None:
            for card in self.cards.values():
                card.show_note(RECENT_LOADING_TEXT)
        self._apply_state()

    def show(self, report: dict[str, Any]) -> None:
        """Keeps `report` and lists each capability's desktop apps; malformed data is reported in
        the panel, never raised."""
        self.loading = False
        self.loaded = True
        self._report = report
        try:
            self._render(report)
        except Exception as exc:  # noqa: BLE001 - malformed engine data is reported in the panel, never raised
            log.exception("the permissions guide could not be shown")
            self.show_error(f"unexpected data ({exc})")
            return
        self._apply_state()

    def show_error(self, message: str) -> None:
        self.loading = False
        self.loaded = True
        self._report = None
        self._set_banner("")
        self.summary.configure(text=ERROR_TEXT.format(message=message), text_color=theme.CRITICAL)
        for card in self.cards.values():
            card.show_note(RECENT_ERROR_TEXT)
        self._apply_state()

    def set_unsupported(self, text: str) -> None:
        """The engine cannot read Windows' record: says why. The Settings buttons stay usable."""
        self.supported = False
        self.loading = False
        self._report = None
        self._set_banner("")
        self.summary.configure(text=f"⚠ {text}", text_color=theme.WARNING)
        for card in self.cards.values():
            card.show_note(RECENT_UNAVAILABLE_TEXT)
        self._apply_state()

    def set_actions_enabled(self, enabled: bool) -> None:
        """Enables Refresh while no other operation holds the window."""
        self._actions_enabled = enabled
        self._apply_state()

    def _apply_state(self) -> None:
        idle = self.supported and not self.loading and self._actions_enabled
        self.refresh_button.configure(state="normal" if idle else "disabled")

    # -- rendering -------------------------------------------------------------------

    def _render(self, report: Mapping[str, Any]) -> None:
        guides = {str(g["capability"]): g for g in report["capabilities"]}
        for capability, card in self.cards.items():
            guide = guides.get(capability)
            if guide is None:
                raise KeyError(f"the guide has no {capability} entry")
            card.show_recent(list(guide["recent_desktop_apps"]))
        self.summary.configure(text=INTRO_TEXT, text_color=theme.INK_MUTED)
        self._set_banner(banner_text(report))

    def _set_banner(self, text: str) -> None:
        self.banner.configure(text=text)
        if text:
            self.banner.grid(row=2, column=0, sticky="ew", padx=14, pady=(0, 6))
        else:
            # grid_forget: CustomTkinter replays the last grid call on a scaling change.
            self.banner.grid_forget()

    # -- width -----------------------------------------------------------------------

    def _units(self, pixels: int) -> float:
        """`pixels` in CustomTkinter units, which wrap lengths are given in."""
        return pixels / ctk.ScalingTracker.get_widget_scaling(self)

    def _panel_resized(self, event: tk.Event) -> None:
        if event.width == self._panel_width:
            return
        self._panel_width = event.width
        wrap = fitted_wrap(self._units(event.width) - self.PANEL_TEXT_INSET, PANEL_WRAP)
        for label in (self.summary, self.banner):
            if label.cget("wraplength") != wrap:
                label.configure(wraplength=wrap)

    def _list_resized(self, event: tk.Event) -> None:
        if event.width == self._list_width:
            return
        self._list_width = event.width
        width = self._units(event.width)
        text_wrap = fitted_wrap(width - GuideCard.TEXT_INSET, PANEL_WRAP)
        path_wrap = fitted_wrap(width - GuideCard.PATH_INSET, ROW_WRAP)
        for card in self.cards.values():
            card.set_wraplength(text_wrap, path_wrap)
