"""Pure helpers of the Boot history section: durations, the chart's axis and bar geometry,
the summary and trend lines, record lines and what a slow item's startup button does.

Nothing here builds a window or calls the engine.
"""

from __future__ import annotations

from typing import Any

import pytest

from optimizer import theme
from optimizer.widgets.boot import (
    ADVICE_MATCHED,
    ADVICE_UNMATCHED,
    FAST_STARTUP_TEXT,
    LEGEND,
    MAX_BARS,
    NO_BOOTS_TEXT,
    bar_geometry,
    boot_line,
    duration_text,
    nice_ceiling_ms,
    notice_lines,
    shutdown_line,
    slow_item_lines,
    slowed_by,
    startup_action,
    summary_text,
    tooltip_text,
    trend_text,
    unexpected_text,
    view_label,
)


def boot(boot_ms: int, *, main_ms: int | None = None, kind: str = "full", **extra: Any) -> dict[str, Any]:
    return {
        "record_id": 1,
        "started_at": "2026-09-27T08:54:00+00:00",
        "logged_at": "2026-09-27T08:55:00+00:00",
        "boot_ms": boot_ms,
        "main_path_ms": boot_ms // 2 if main_ms is None else main_ms,
        "post_boot_ms": boot_ms - (boot_ms // 2 if main_ms is None else main_ms),
        "startup_apps": 12,
        "after_update": False,
        "degraded": False,
        "boot_type": kind,
        "after_unexpected_shutdown": False,
        "slow": [],
        **extra,
    }


@pytest.mark.parametrize(
    ("ms", "text"),
    [(0, "0.0 s"), (999, "1.0 s"), (41_200, "41.2 s"), (65_000, "1 min 5 s"), (None, "–"), (-1, "–")],
)
def test_duration_text(ms: Any, text: str) -> None:
    assert duration_text(ms) == text


@pytest.mark.parametrize(
    ("ms", "top"),
    [
        (0, 10_000),
        (9_000, 10_000),
        (10_000, 10_000),
        (10_001, 20_000),
        (41_200, 60_000),
        (61_000, 90_000),
        (150_000, 180_000),
        (599_000, 600_000),
        (601_000, 1_200_000),
    ],
)
def test_nice_ceiling(ms: int, top: int) -> None:
    assert nice_ceiling_ms(ms) == top


def test_bars_are_proportional_and_oldest_left() -> None:
    boots = [boot(40_000, main_ms=10_000), boot(20_000, main_ms=20_000, kind="fast_startup", degraded=True)]
    bars = bar_geometry(boots, 442, 124, 40_000)
    # boots are newest first; the chart shows the oldest on the left.
    assert [b.boot["boot_ms"] for b in bars] == [20_000, 40_000]
    assert bars[0].x0 < bars[1].x0
    base = bars[0].y_base
    full_height = base - bars[1].y_top
    assert full_height == pytest.approx(2 * (base - bars[0].y_top))
    # The desktop part of the 40 s start is a quarter of its bar.
    assert base - bars[1].y_main == pytest.approx(full_height / 4)
    assert bars[0].fast and not bars[1].fast
    assert bars[0].degraded and not bars[1].degraded
    assert all(b.x1 - b.x0 <= 24 for b in bars)


def test_at_most_thirty_bars() -> None:
    boots = [boot(30_000 + i) for i in range(45)]
    bars = bar_geometry(boots, 800, 170, 60_000)
    assert len(bars) == MAX_BARS
    # The newest 30, oldest of them first.
    assert bars[0].boot["boot_ms"] == 30_029
    assert bars[-1].boot["boot_ms"] == 30_000


def test_bars_never_vanish_or_overflow() -> None:
    bars = bar_geometry([boot(0), boot(900_000)], 300, 100, 60_000)
    assert all(b.y_base - b.y_top >= 1 for b in bars)
    assert all(b.y_top >= 0 for b in bars)


def test_summary_and_empty_history() -> None:
    history = {"boots": [boot(41_200, main_ms=18_000)], "stats": {"count": 24, "median_ms": 38_500}}
    assert (
        summary_text(history)
        == "Last full start 41.2 s (desktop after 18.0 s)  ·  Typical 38.5 s over 24 starts"
    )
    assert summary_text({"boots": [], "stats": {}}) == NO_BOOTS_TEXT
    assert NO_BOOTS_TEXT == "No full starts are recorded yet. Windows records one after each restart."


def test_fast_startup_starts_are_explained_not_drawn() -> None:
    # Windows times only full starts, so no bar stands for a Fast Startup start.
    assert FAST_STARTUP_TEXT == (
        "Fast Startup is on: Windows times only full starts (restarts), so most starts are not listed."
    )
    assert "Fast Startup" not in [text for _mark, _color, text in LEGEND]


@pytest.mark.parametrize(
    ("change", "text", "color"),
    [
        (24.0, "⚠ Starts are getting slower: 24% slower than earlier starts", theme.WARNING),
        (15.0, "⚠ Starts are getting slower: 15% slower than earlier starts", theme.WARNING),
        (14.9, "✓ Start times are steady", theme.GOOD),
        (-14.9, "✓ Start times are steady", theme.GOOD),
        (-15.0, "✓ Starts are getting faster: 15% faster", theme.GOOD),
        (-18.2, "✓ Starts are getting faster: 18% faster", theme.GOOD),
    ],
)
def test_trend_boundaries(change: float, text: str, color: str) -> None:
    stats = {
        "trend": {"boot_type": "full", "recent_median_ms": 1, "earlier_median_ms": 1, "change_pct": change}
    }
    assert trend_text(stats) == (text, color)


def test_no_trend_without_enough_starts() -> None:
    assert trend_text({"trend": None})[0] == ""


def test_record_lines() -> None:
    line = boot_line(boot(41_200, main_ms=18_000, degraded=True, after_update=True))
    assert line.endswith(
        "  ·  Full start  ·  41.2 s (desktop 18.0 s)  ·  12 startup apps  ·  ⚠ slower than usual"
        "  ·  after an update"
    )
    assert "Fast Startup" in boot_line(boot(10_000, kind="fast_startup"))
    assert "Resume" in boot_line(boot(10_000, kind="hibernate"))
    assert "after an unexpected shutdown" in boot_line(boot(10_000, after_unexpected_shutdown=True))
    shutdown = {
        "started_at": "2026-09-27T20:00:00+00:00",
        "shutdown_ms": 12_300,
        "degraded": True,
        "slow": [],
    }
    assert shutdown_line(shutdown).endswith("  ·  12.3 s  ·  ⚠ slower than usual")
    slowed = boot(40_000, slow=[{"title": "OneDrive"}, {"title": "Contoso Driver"}, {"title": "OneDrive"}])
    assert slowed_by(slowed) == "Slowed by: OneDrive, Contoso Driver"
    assert slowed_by(boot(40_000)) == ""
    tip = tooltip_text(slowed)
    assert tip.endswith("  ·  Full start  ·  40.0 s (desktop 20.0 s)  ·  12 startup apps  ·  3 slow items")


def item(kind: str = "app", ids: tuple[str, ...] = (), phase: str = "startup") -> dict[str, Any]:
    return {
        "key": f"{phase}:{kind}:x",
        "phase": phase,
        "kind": kind,
        "name": "x",
        "title": "X",
        "path": None,
        "company": None,
        "count": 3,
        "last_seen": "2026-09-27T08:55:00+00:00",
        "median_degradation_ms": 2500,
        "max_degradation_ms": 3000,
        "startup_ids": list(ids),
    }


def test_slow_item_lines_and_advice() -> None:
    line, advice = slow_item_lines(item(ids=("user_run:Discord",)), 12)
    assert line.startswith("3 of the last 12 starts  ·  usually adds 2.5 s  ·  last on ")
    assert advice == ADVICE_MATCHED
    assert slow_item_lines(item(), 12)[1] == ADVICE_UNMATCHED
    # Several entries: none of them is turned off, so none is named as the one that starts it.
    assert slow_item_lines(item(ids=("user_run:Contoso", "user_run:Fabrikam")), 12)[1] == ADVICE_UNMATCHED
    assert slow_item_lines(item("driver"), 12)[1].startswith("Took long to initialize.")
    assert slow_item_lines(item("device"), 12)[1].startswith("Took long to initialize.")
    assert slow_item_lines(item("service"), 12)[1] == "A service took long to start."
    assert slow_item_lines(item("prefetch"), 12)[1].startswith("Windows itself took longer")
    line, _ = slow_item_lines(item("service", phase="shutdown"), 12, 1)
    assert line.startswith("3 of the last 1 shutdown  ·  ")


def entry(entry_id: str, *, enabled: bool = True, can_toggle: bool = True, note: str = "") -> dict[str, Any]:
    return {"id": entry_id, "name": entry_id, "enabled": enabled, "can_toggle": can_toggle, "note": note}


def test_startup_actions() -> None:
    discord = item(ids=("user_run:Discord",))
    assert startup_action(discord, [entry("user_run:Discord")]) == ("turn_off", entry("user_run:Discord"))
    off = entry("user_run:Discord", enabled=False)
    assert startup_action(discord, [off]) == ("already_off", off)
    assert startup_action(discord, [entry("user_run:Spotify")]) == ("none", None)
    assert startup_action(item(), [entry("user_run:Discord")]) == ("none", None)
    # A policy entry still gets the button; the card disables it and shows the entry's note.
    policy = entry("user_run:Discord", can_toggle=False, note="Set by Group Policy.")
    kind, found = startup_action(discord, [policy])
    assert kind == "turn_off"
    assert found is not None and found["note"] == "Set by Group Policy."
    # An item that names several entries is not acted on: which one starts it is unknown.
    several = item(ids=("user_run:Contoso", "user_run:Fabrikam"))
    entries = [entry("user_run:Contoso"), entry("user_run:Fabrikam")]
    assert startup_action(several, entries) == ("none", None)


def test_the_turn_off_button_names_its_entry() -> None:
    from optimizer.widgets.boot import TURN_OFF_TEXT, turn_off_text

    assert turn_off_text(entry("user_run:Discord") | {"name": "Discord"}) == "Turn off “Discord” at startup"
    long_name = "Contoso Background Synchronization Assistant for Teams"
    text = turn_off_text({"id": "user_run:x", "name": long_name})
    assert text.startswith("Turn off “Contoso Background Synchronization") and text.endswith("…” at startup")
    assert len(text) <= len("Turn off “” at startup") + 40
    assert turn_off_text({"id": "user_run:x", "name": " "}) == TURN_OFF_TEXT


def test_slow_items_are_split_by_phase() -> None:
    from optimizer.widgets.boot import slow_items

    history = {
        "slow_items": [
            item("app", ("user_run:Discord",)),
            item("service", phase="shutdown"),
            item("driver"),
            "not an item",
        ]
    }
    assert [i["key"] for i in slow_items(history, "startup")] == ["startup:app:x", "startup:driver:x"]
    assert [i["key"] for i in slow_items(history, "shutdown")] == ["shutdown:service:x"]
    assert slow_items({}, "startup") == []
    # An item without a phase is a startup item.
    assert len(slow_items({"slow_items": [{"key": "app:x"}]}, "startup")) == 1


def test_notice_lines_list_notes_then_errors() -> None:
    history = {
        "notes": ["The startup list could not be read.", ""],
        "errors": ["Could not read the start types: no access", None],
    }
    assert notice_lines(history) == [
        "The startup list could not be read.",
        "Could not read the start types: no access",
    ]
    assert notice_lines({"notes": [], "errors": []}) == []
    # An older engine's history has neither key.
    assert notice_lines({}) == []
    assert notice_lines({"notes": None, "errors": "broken"}) == []


def test_unexpected_shutdowns_and_view_labels() -> None:
    history = {"unexpected_shutdowns": ["2026-09-27T20:00:00+00:00", "2026-09-20T20:00:00+00:00"]}
    assert unexpected_text(history).startswith(
        "⚠ Windows was shut down unexpectedly 2 times (crash or power loss)"
    )
    assert unexpected_text({"unexpected_shutdowns": []}) == ""
    assert view_label("slow", 3) == "Slows startup (3)"
    assert view_label("starts", 3) == "Recent starts"
    assert view_label("shutdowns", 3) == "Shutdowns"
