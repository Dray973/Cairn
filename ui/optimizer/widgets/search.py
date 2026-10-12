"""Search box and word-prefix matching shared by the lists that can be searched.

Matching works on folded text: compatibility-decomposed (NFKD) with the combining marks
dropped, casefolded, whitespace runs as one space, so accents and case are ignored
("Télémétrie" matches "telemetrie"). A query matches when every one of its words is the
start of some word of the searched fields: "ai" never matches "available", "edge" never
matches "knowledge". Words are runs of letters and digits, so an id such as
`privacy.edge_feedback` has the words `privacy`, `edge` and `feedback`.
"""

from __future__ import annotations

import re
import tkinter as tk
import unicodedata
from collections.abc import Callable, Sequence
from typing import Any

import customtkinter as ctk

from .. import theme

# Delay between the last edit and the search, so typing a word filters once.
DEBOUNCE_MS = 200
# Width of the clear button, in CustomTkinter units.
CLEAR_WIDTH = 28
CLEAR_TEXT = "✕"

# Runs of letters and digits (`\w` without the underscore).
_WORD = re.compile(r"[^\W_]+")

# `after(ms, callback) -> token` and `after_cancel(token)`, as Tk widgets provide them.
Scheduler = Callable[[int, Callable[[], None]], Any]
Canceller = Callable[[Any], None]


def fold(text: str) -> str:
    """`text` for matching: NFKD with the combining marks dropped, casefolded, every run of
    whitespace as one space and none at either end."""
    decomposed = unicodedata.normalize("NFKD", text)
    bare = "".join(c for c in decomposed if not unicodedata.combining(c))
    return " ".join(bare.casefold().split())


def words(text: str) -> list[str]:
    """The folded words of `text`: its runs of letters and digits, in order."""
    return _WORD.findall(fold(text))


def search_terms(query: str) -> list[str]:
    """The words a query must match, folded; empty for a blank query."""
    return words(query)


def matches(fields: Sequence[str], terms: Sequence[str]) -> bool:
    """Whether every term (from `search_terms`) is the start of some word of some field.
    Empty fields are ignored; no terms match everything."""
    if not terms:
        return True
    found = [w for field in fields if field for w in words(field)]
    return all(any(w.startswith(term) for w in found) for term in terms)


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


class SearchEntry(ctk.CTkFrame):
    """A search box: an entry with a placeholder and a clear button that is shown only while
    there is text.

    `on_change(query)` receives the entry's text `debounce_ms` after the last edit; Esc and the
    clear button empty the box and report the empty query at once. `after` and `after_cancel`
    schedule and cancel that delay (the widget's own by default).
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        placeholder: str,
        on_change: Callable[[str], None],
        width: int = 140,
        debounce_ms: int = DEBOUNCE_MS,
        after: Scheduler | None = None,
        after_cancel: Canceller | None = None,
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_change = on_change
        self._debounce_ms = debounce_ms
        self._after: Scheduler = after if after is not None else self.after
        self._after_cancel: Canceller = after_cancel if after_cancel is not None else self.after_cancel
        self._pending: Any = None
        self._text = ""
        self.grid_columnconfigure(0, weight=1)
        self.entry = ctk.CTkEntry(
            self,
            width=width,
            height=28,
            font=_font(12),
            placeholder_text=placeholder,
            placeholder_text_color=theme.INK_MUTED,
            fg_color=theme.PAGE,
            border_color=theme.BORDER,
            text_color=theme.INK,
        )
        self.entry.grid(row=0, column=0, sticky="ew")
        self.clear_button = ctk.CTkButton(
            self,
            text=CLEAR_TEXT,
            width=CLEAR_WIDTH,
            height=28,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            text_color=theme.INK_SECONDARY,
            command=self.clear,
        )
        self.entry.bind("<KeyRelease>", lambda _e: self._text_edited())
        self.entry.bind("<Escape>", self._escape)

    @property
    def query(self) -> str:
        """The text in the box (empty while the placeholder shows)."""
        return self._text

    @property
    def clear_shown(self) -> bool:
        """Whether the clear button is laid out (there is text)."""
        return bool(self.clear_button.grid_info())

    def _text_edited(self) -> None:
        """Restarts the delay when the text changed; keys that change nothing are ignored."""
        text = self.entry.get()
        if text == self._text:
            return
        self._text = text
        self._show_clear()
        self._cancel_pending()
        self._pending = self._after(self._debounce_ms, self._fire)

    def _fire(self) -> None:
        self._pending = None
        self._on_change(self._text)

    def _escape(self, _event: tk.Event) -> str:
        self.clear()
        return "break"

    def _cancel_pending(self) -> None:
        if self._pending is not None:
            pending, self._pending = self._pending, None
            try:
                self._after_cancel(pending)
            except (tk.TclError, ValueError):
                pass

    def _show_clear(self) -> None:
        if self._text:
            self.clear_button.grid(row=0, column=1, padx=(4, 0))
        else:
            # grid_forget: CustomTkinter replays the last grid call on a scaling change, which
            # would show a removed widget again.
            self.clear_button.grid_forget()

    def _replace(self, text: str) -> None:
        self.entry.delete(0, "end")
        if text:
            self.entry.insert(0, text)

    def set_query(self, text: str) -> None:
        """Puts `text` in the box and reports it at once, without the delay."""
        self._cancel_pending()
        self._replace(text)
        self._text = text
        self._show_clear()
        self._on_change(text)

    def clear(self) -> None:
        """Empties the box and reports the empty query at once."""
        self.set_query("")

    def focus_and_select(self) -> None:
        """Moves the keyboard focus to the box and selects its text, so typing replaces it."""
        self.entry.focus_set()
        if self._text:
            self.entry.select_range(0, "end")
            self.entry.icursor("end")

    def destroy(self) -> None:
        self._cancel_pending()
        super().destroy()
