"""The shared search box and its matching: folding, words, word-prefix matching, and the
SearchEntry's delay, Esc, clear button and test API.

The pure tests create no window; the SearchEntry tests build one box in a root window on the
tests' hidden desktop. Nothing is read from or changed on this PC.
"""

from __future__ import annotations

import gc
import time
from collections.abc import Callable, Iterator
from typing import Any

import customtkinter as ctk
import pytest

from optimizer.widgets.search import (
    CLEAR_TEXT,
    CLEAR_WIDTH,
    DEBOUNCE_MS,
    SearchEntry,
    fold,
    matches,
    search_terms,
    words,
)

# -- folding and words -------------------------------------------------------------------


def test_fold_drops_case_and_accents() -> None:
    assert fold("Télémétrie") == "telemetrie"
    assert fold("ÄÖÜ äöü Ñ ç") == "aou aou n c"
    assert fold("İstanbul") == "istanbul", "the dot above is a combining mark"
    assert fold("Straße") == "strasse"
    assert fold("ﬁle") == "file", "compatibility forms are decomposed"


def test_fold_joins_whitespace_runs() -> None:
    assert fold("  Turn \t off\n\nEdge  ") == "turn off edge"
    assert fold("") == ""
    assert fold(" \t\n") == ""


def test_words_split_on_everything_but_letters_and_digits() -> None:
    assert words("privacy.edge_feedback") == ["privacy", "edge", "feedback"]
    assert words("Windows 11 26H2 (build 26300.9457)") == ["windows", "11", "26h2", "build", "26300", "9457"]
    assert words("Settings › Privacy & security") == ["settings", "privacy", "security"]
    assert words("Edge's new tab page") == ["edge", "s", "new", "tab", "page"]
    assert words("") == []


def test_search_terms_are_the_folded_words_of_the_query() -> None:
    assert search_terms("") == []
    assert search_terms("   ") == []
    assert search_terms("Edge  FEED") == ["edge", "feed"]
    assert search_terms("privacy.edge") == ["privacy", "edge"]
    assert search_terms("télé") == ["tele"]


# -- matching ----------------------------------------------------------------------------


def test_a_term_matches_only_the_start_of_a_word() -> None:
    assert not matches(["Available here"], ["ai"])
    assert matches(["AI features"], ["ai"])
    assert not matches(["Knowledge base"], ["edge"])
    assert matches(["Microsoft Edge"], ["edge"])
    assert matches(["Microsoft Edge"], ["ed"])
    assert not matches(["Microsoft Edge"], ["edges"])


FIELDS = [
    "Turn off Edge feedback",
    "Stops the feedback surveys Microsoft says Edge shows.",
    "privacy.edge_feedback",
    "Microsoft Edge",
]


def test_every_term_must_match_a_word_of_some_field() -> None:
    assert matches(FIELDS, search_terms("edge surv"))
    assert matches(FIELDS, search_terms("surveys microsoft"))
    assert matches(FIELDS, search_terms("privacy.edge_feed"))
    assert not matches(FIELDS, search_terms("edge office"))
    assert not matches(FIELDS, search_terms("feedback telemetry"))


def test_no_terms_match_everything() -> None:
    assert matches(FIELDS, [])
    assert matches([], [])
    assert matches(FIELDS, search_terms("  "))


def test_accents_and_case_are_ignored_on_both_sides() -> None:
    assert matches(["Télémétrie"], search_terms("TELEM"))
    assert matches(["telemetry"], search_terms("Télé"))
    assert matches(["Diagnostics and feedback"], search_terms("DIAG FEED"))


def test_empty_fields_are_ignored() -> None:
    assert matches(["", "Edge"], ["ed"])
    assert not matches([""], ["ed"])
    assert not matches([], ["a"])


# -- SearchEntry -------------------------------------------------------------------------


class Timer:
    """Stands in for Tk's `after` and `after_cancel`: callbacks run only when the test fires them."""

    def __init__(self) -> None:
        self.scheduled: list[tuple[int, int, Callable[[], None]]] = []
        self.cancelled: list[int] = []
        self._next = 0

    def after(self, ms: int, callback: Callable[[], None]) -> int:
        self._next += 1
        self.scheduled.append((self._next, ms, callback))
        return self._next

    def cancel(self, token: Any) -> None:
        self.cancelled.append(token)

    def pending(self) -> list[tuple[int, int, Callable[[], None]]]:
        return [entry for entry in self.scheduled if entry[0] not in self.cancelled]

    def fire(self) -> None:
        """Runs the one callback that is still pending."""
        [(token, _, callback)] = self.pending()
        self.cancelled.append(token)
        callback()


def pump(root: ctk.CTk, seconds: float, until: Callable[[], bool] | None = None) -> None:
    deadline = time.perf_counter() + seconds
    while time.perf_counter() < deadline:
        root.update()
        if until is not None and until():
            return
        time.sleep(0.001)
    if until is not None:
        raise AssertionError("condition not reached while pumping the event loop")


@pytest.fixture(scope="module")
def window() -> Iterator[ctk.CTk]:
    """One root window for the module's boxes."""
    window = ctk.CTk()
    pump(window, 0.3)  # past CustomTkinter's start-up timers
    try:
        yield window
    finally:
        # CustomTkinter's own timers would otherwise fire once the window is gone, when a later
        # window's event loop runs, and print Tcl errors.
        for job in window.tk.splitlist(window.tk.call("after", "info")):
            window.after_cancel(job)
        window.destroy()
        # Tk objects are finalized here, on the Tk thread.
        gc.collect()


@pytest.fixture
def root(window: ctk.CTk) -> Iterator[ctk.CTk]:
    """The root window; the boxes a test puts in it are destroyed after the test."""
    try:
        yield window
    finally:
        for child in window.winfo_children():
            child.destroy()
        pump(window, 0.05)


def box(root: ctk.CTk, seen: list[str], timer: Timer | None = None, **options: Any) -> SearchEntry:
    entry = SearchEntry(
        root,
        placeholder="Search tweaks",
        on_change=seen.append,
        after=timer.after if timer is not None else None,
        after_cancel=timer.cancel if timer is not None else None,
        **options,
    )
    entry.pack(fill="x")
    return entry


def type_text(entry: SearchEntry, text: str) -> None:
    """Appends `text` and runs what the box's key binding runs."""
    entry.entry.insert("end", text)
    entry._text_edited()


def test_search_entry_layout(root: ctk.CTk) -> None:
    seen: list[str] = []
    entry = box(root, seen)
    assert entry.entry.cget("placeholder_text") == "Search tweaks"
    assert entry.entry.cget("width") == 140
    assert entry.clear_button.cget("text") == CLEAR_TEXT == "✕"
    assert entry.clear_button.cget("width") == CLEAR_WIDTH == 28
    assert not entry.clear_shown, "nothing to clear"
    assert entry.query == ""
    assert "<KeyRelease>" in entry.entry._entry.bind(), "edits are noticed as they are typed"
    assert "<Key-Escape>" in entry.entry._entry.bind()
    assert seen == []


def test_typing_reports_once_after_the_delay(root: ctk.CTk) -> None:
    seen: list[str] = []
    timer = Timer()
    entry = box(root, seen, timer)
    type_text(entry, "ed")
    assert seen == [], "nothing is reported while the delay runs"
    assert [ms for _, ms, _ in timer.pending()] == [DEBOUNCE_MS] == [200]
    assert entry.clear_shown
    type_text(entry, "ge")
    assert timer.cancelled == [1], "each edit restarts the delay"
    assert len(timer.pending()) == 1
    timer.fire()
    assert seen == ["edge"]
    assert entry.query == "edge"


def test_keys_that_change_nothing_do_not_restart_the_delay(root: ctk.CTk) -> None:
    seen: list[str] = []
    timer = Timer()
    entry = box(root, seen, timer)
    type_text(entry, "ai")
    timer.fire()
    entry._text_edited()  # an arrow key: the text is unchanged
    assert timer.pending() == []
    assert seen == ["ai"]


def test_escape_clears_at_once(root: ctk.CTk) -> None:
    seen: list[str] = []
    timer = Timer()
    entry = box(root, seen, timer)
    type_text(entry, "edge")
    assert entry._escape(None) == "break"  # type: ignore[arg-type]
    assert seen == [""], "the empty query is reported without the delay"
    assert timer.pending() == [], "the pending report of the old text is cancelled"
    assert entry.entry.get() == ""
    assert entry.query == ""
    assert not entry.clear_shown


def test_clear_button_clears_at_once(root: ctk.CTk) -> None:
    seen: list[str] = []
    timer = Timer()
    entry = box(root, seen, timer)
    type_text(entry, "office")
    timer.fire()
    entry.clear_button.invoke()
    assert seen == ["office", ""]
    assert entry.entry.get() == ""
    assert not entry.clear_shown


def test_set_query_applies_at_once(root: ctk.CTk) -> None:
    seen: list[str] = []
    timer = Timer()
    entry = box(root, seen, timer)
    entry.set_query("Edge feed")
    assert seen == ["Edge feed"]
    assert timer.scheduled == [], "no delay"
    assert entry.entry.get() == "Edge feed"
    assert entry.query == "Edge feed"
    assert entry.clear_shown
    type_text(entry, "x")
    entry.set_query("ai")
    assert timer.pending() == [], "a pending report is replaced"
    assert seen == ["Edge feed", "ai"]
    entry.set_query("")
    assert seen == ["Edge feed", "ai", ""]
    assert entry.entry.get() == ""
    assert not entry.clear_shown


def test_focus_and_select_selects_the_text(root: ctk.CTk) -> None:
    seen: list[str] = []
    entry = box(root, seen, Timer())
    entry.set_query("edge")
    entry.focus_and_select()
    assert entry.entry.select_present()
    assert entry.entry.index("insert") == len("edge")


def test_the_widgets_own_timer_reports_and_stops_with_the_box(root: ctk.CTk) -> None:
    seen: list[str] = []
    entry = box(root, seen, debounce_ms=10)
    type_text(entry, "game")
    pump(root, 2.0, until=lambda: bool(seen))
    assert seen == ["game"]

    other: list[str] = []
    closing = box(root, other, debounce_ms=10)
    type_text(closing, "power")
    closing.destroy()
    pump(root, 0.1)
    assert other == [], "a box that is gone reports nothing"
