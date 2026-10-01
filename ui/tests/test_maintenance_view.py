"""Pure helpers of the Maintenance section: texts built from the engine's status, plans and
runs. No window is created."""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

import pytest

from optimizer import theme
from optimizer.features.maintenance import SAVE_TITLE, TURN_ON_MESSAGE, TURN_ON_TITLE
from optimizer.widgets.maintenance import (
    BATTERY_NOTE,
    DAY_LABELS,
    DAY_VALUES,
    HOUR_LABELS,
    MINUTE_LABELS,
    ON_BATTERY_TEXT,
    TURN_ON_FIRST_TEXT,
    attention_hints,
    clock_text,
    config_differs,
    config_time_to_menus,
    day_label,
    duration_text,
    earlier_lines,
    effective_config,
    local_time,
    local_when,
    menus_to_config_time,
    names_tools,
    needs_attention,
    next_run_text,
    notice_for,
    plan_details,
    run_now_block,
    run_state,
    run_state_line,
    schedule_text,
    short_date,
    state_label,
    status_bar_text,
    step_lines,
    target_description,
    time_text,
)

from .fake_maintenance import DEFAULTS, NEXT_RUN, TASK_PATH, run_dict, target_dicts


def test_menus_cover_the_week_and_the_day() -> None:
    assert DAY_LABELS == ("Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday")
    assert [day_label(d) for d in DAY_VALUES] == list(DAY_LABELS)
    assert len(HOUR_LABELS) == 24
    assert (HOUR_LABELS[0], HOUR_LABELS[1], HOUR_LABELS[12], HOUR_LABELS[23]) == (
        "12 AM",
        "1 AM",
        "12 PM",
        "11 PM",
    )
    assert MINUTE_LABELS == (":00", ":15", ":30", ":45")


@pytest.mark.parametrize(
    ("hour", "minute", "time"),
    [
        ("12 AM", ":00", "00:00"),
        ("1 PM", ":30", "13:30"),
        ("12 PM", ":45", "12:45"),
        ("11 PM", ":15", "23:15"),
    ],
)
def test_menus_and_config_times_round_trip(hour: str, minute: str, time: str) -> None:
    assert menus_to_config_time(hour, minute) == time
    assert config_time_to_menus(time) == (hour, minute)


def test_a_minute_off_the_quarter_hours_keeps_its_value() -> None:
    assert config_time_to_menus("07:05") == ("7 AM", ":05")
    assert menus_to_config_time("7 AM", ":05") == "07:05"
    assert config_time_to_menus("nonsense") == ("12 PM", ":00")


def test_times_and_schedules_read_as_twelve_hour_text() -> None:
    assert time_text("13:30") == "1:30 PM"
    assert time_text("00:05") == "12:05 AM"
    assert time_text("12:00") == "12:00 PM"
    assert time_text("25:00") == "25:00"
    assert schedule_text({"day": "sunday", "time": "12:00"}) == "every Sunday at 12:00 PM"
    assert schedule_text({"day": "wednesday", "time": "18:45"}) == "every Wednesday at 6:45 PM"


def test_stamps_read_as_local_times() -> None:
    naive = "2026-10-04T12:00:00"
    assert local_when(naive) == "Sun 4 Oct, 12:00 PM"
    assert short_date(naive) == "Sun 4 Oct"
    assert clock_text(naive) == "12:00 PM"
    utc = "2026-09-27T12:14:00Z"
    moment = datetime(2026, 9, 27, 12, 14, tzinfo=UTC).astimezone().replace(tzinfo=None)
    assert local_time(utc) == moment
    assert local_when(utc).endswith(
        f"{(moment.hour % 12) or 12}:{moment.minute:02d} {'AM' if moment.hour < 12 else 'PM'}"
    )
    assert local_when(None) == "at an unknown time"
    assert local_when("garbage") == "at an unknown time"
    assert short_date(None) == ""


def test_durations() -> None:
    assert duration_text(45_000) == "45 s"
    assert duration_text(18 * 60_000) == "18 min"
    assert duration_text(65 * 60_000) == "1 h 5 min"
    assert duration_text(None) == ""
    assert duration_text(-1) == ""


def test_run_state_lines_carry_an_icon_and_a_colour() -> None:
    running = run_dict(
        3,
        "running",
        progress={"step": "system_files", "title": "Checking system files… 45%", "index": 2, "count": 3},
    )
    text, color = run_state_line(running)
    assert text.startswith("◐ Running since ") and text.endswith("  ·  step 2 of 3")
    assert color == theme.ACCENT
    expected = {
        "completed": ("✓ Finished ", theme.GOOD),
        "attention": ("⚠ Finished ", theme.WARNING),
        "failed": ("⚠ Failed ", theme.CRITICAL),
        "stopped": ("○ Stopped early ", theme.INK_SECONDARY),
        "skipped": ("○ Skipped ", theme.INK_SECONDARY),
        "interrupted": ("○ Didn't finish (started ", theme.INK_SECONDARY),
    }
    for state, (start, colour) in expected.items():
        text, color = run_state_line(run_dict(1, state))
        assert text.startswith(start), (state, text)
        assert color == colour
    assert run_state_line(run_dict(1, "completed"))[0].endswith("  ·  14 min")
    stale = dict(running, stale=True)
    assert run_state(stale) == "interrupted"
    assert run_state_line(stale)[0].startswith("○ Didn't finish")


def test_step_lines_show_each_step() -> None:
    run = run_dict(
        1,
        "attention",
        attention=["Windows found damaged system files. Open Tools and run Repair system files."],
    )
    lines = step_lines(run["report"])
    assert lines == [
        ("✓ Clean up: freed 1.2 GB", theme.GOOD),
        ("⚠ Check system files: Windows found damaged system files.", theme.WARNING),
    ]
    assert step_lines(None) == []
    report = dict(run["report"], cleanup={"outcome": "failed", "error": "journal locked", "freed_bytes": 0})
    assert step_lines(report)[0] == ("⚠ Clean up: failed (journal locked)", theme.CRITICAL)
    assert names_tools(run["report"])
    assert not names_tools(run_dict(2)["report"])
    assert attention_hints(run["report"]) == run["report"]["attention"]


def test_notices_follow_the_run_state() -> None:
    when = local_when("2026-09-27T12:14:00Z")
    assert notice_for(run_dict(1, "completed")) == (f"✓ Maintenance ran {when}: Freed 1.2 GB.", theme.GOOD)
    assert notice_for(run_dict(1, "attention", attention=["Windows found damaged system files."])) == (
        f"⚠ Maintenance ran {when} and found something to look at: Windows found damaged system files.",
        theme.WARNING,
    )
    assert notice_for(run_dict(1, "failed")) == (
        f"⚠ Maintenance ran {when}, but a step failed",
        theme.WARNING,
    )
    assert notice_for(run_dict(1, "failed", attention=["the cleanup failed"])) == (
        f"⚠ Maintenance ran {when}, but the cleanup failed",
        theme.WARNING,
    )
    assert notice_for(run_dict(1, "stopped", stopped_reason="the PC switched to battery power")) == (
        f"○ Maintenance stopped early {when}: the PC switched to battery power",
        theme.INK_SECONDARY,
    )
    started = local_when("2026-09-27T12:00:00Z")
    assert notice_for(run_dict(1, "interrupted")) == (
        f"○ The maintenance run of {started} didn't finish (the PC shut down or you signed out).",
        theme.INK_SECONDARY,
    )
    assert notice_for(run_dict(1, "running")) is None
    assert needs_attention(run_dict(1, "attention")) and needs_attention(run_dict(1, "failed"))
    assert not needs_attention(run_dict(1, "completed"))


def test_config_comparison() -> None:
    assert not config_differs(DEFAULTS, dict(DEFAULTS))
    assert config_differs(DEFAULTS, dict(DEFAULTS, time="13:00"))
    assert config_differs(DEFAULTS, dict(DEFAULTS, targets=["user_temp"]))
    assert config_differs(None, DEFAULTS)
    assert not config_differs(None, None)


def test_plan_details_list_every_location_whose_files_are_deleted() -> None:
    plan = {
        "config": dict(DEFAULTS, targets=["user_temp", "error_reports"]),
        "task_path": TASK_PATH,
        "program": "C:\\Program Files\\Cairn\\cairn-maintenance.exe",
        "arguments": "--journal x",
        "account": "TEST-PC\\Test",
        "next_run": NEXT_RUN,
        "creates": True,
        "unchanged": False,
        "blocked_reason": None,
        "notes": ["This PC has a battery: maintenance waits until it's plugged in."],
    }
    details = plan_details(plan, target_dicts())
    assert details[0] == "• Runs every Sunday at 12:00 PM; next run Sun 4 Oct, 12:00 PM"
    assert "TEST-PC\\Test" in details[1]
    assert details[2] == "• Checks, read-only: System File Checker (sfc /verifyonly), DISM CheckHealth"
    assert "• Each run permanently deletes the files in:" in details
    assert "      Temporary files" in details and "      Error reports" in details
    assert "• This PC has a battery: maintenance waits until it's plugged in." in details
    assert details[-1] == "• Task Scheduler: \\Cairn\\Maintenance (your account)"
    nothing = plan_details(dict(plan, config=dict(DEFAULTS, targets=[])), target_dicts())
    assert "• Cleans nothing" in nothing


def test_the_turn_on_message_says_runs_delete_permanently() -> None:
    assert TURN_ON_MESSAGE.endswith(" Each run permanently deletes the files in the selected locations.")
    assert "{day}" in TURN_ON_MESSAGE and "{time}" in TURN_ON_MESSAGE
    assert TURN_ON_TITLE == "Turn on scheduled maintenance?"
    assert SAVE_TITLE == "Save the maintenance schedule?"


def _status(**changes: object) -> dict[str, object]:
    task = {
        "enabled": True,
        "state": "ready",
        "config": dict(DEFAULTS),
        "drift": [],
        "next_run_time": NEXT_RUN,
    }
    status: dict[str, object] = {
        "task": task,
        "recorded": True,
        "running": False,
        "on_battery": False,
        "has_battery": False,
        "blocked_reason": None,
        "defaults": dict(DEFAULTS),
    }
    status.update(changes)
    return status


def test_state_labels() -> None:
    assert state_label(_status()) == ("✓ On · every Sunday at 12:00 PM", theme.GOOD)
    assert state_label(_status(task=None)) == ("○ Off", theme.INK_MUTED)
    assert state_label(_status(running=True)) == ("◐ Running…", theme.ACCENT)
    assert state_label(_status(recorded=False)) == ("⚠ Not recorded by Cairn", theme.WARNING)
    disabled = _status()
    disabled["task"]["enabled"] = False  # type: ignore[index]
    assert state_label(disabled) == ("⚠ Disabled in Task Scheduler", theme.WARNING)
    drifted = _status()
    drifted["task"]["drift"] = ["it may wake the PC"]  # type: ignore[index]
    assert state_label(drifted) == ("⚠ Changed outside Cairn", theme.WARNING)


def test_next_run_and_run_now_availability() -> None:
    assert next_run_text(_status()) == (
        "Next run: Sun 4 Oct, 12:00 PM  ·  waits until the PC is idle and plugged in"
    )
    assert next_run_text(_status(task=None)) == ""
    assert run_now_block(_status()) is None
    assert run_now_block(_status(task=None)) == TURN_ON_FIRST_TEXT
    assert run_now_block(_status(recorded=False)) == TURN_ON_FIRST_TEXT
    assert run_now_block(_status(on_battery=True)) == ON_BATTERY_TEXT
    assert run_now_block(_status(blocked_reason="unsafe")) == "unsafe"
    assert run_now_block(_status(running=True)) is not None
    assert BATTERY_NOTE.startswith("This PC has a battery")


def test_the_controls_show_the_recorded_schedule_else_the_defaults() -> None:
    custom = dict(DEFAULTS, day="friday")
    status = _status()
    status["task"]["config"] = custom  # type: ignore[index]
    assert effective_config(status) == custom
    assert effective_config(_status(task=None)) == DEFAULTS
    assert effective_config(_status(recorded=False)) == DEFAULTS


def test_earlier_runs_and_the_status_bar() -> None:
    runs = [run_dict(3), run_dict(2, "attention"), run_dict(1, "failed", headline="Cleanup failed")]
    # The runs start at a UTC time and are shown on the local day.
    day = short_date(runs[1]["started_at"])
    assert day in ("Sun 27 Sep", "Mon 28 Sep")
    assert earlier_lines(runs) == [f"{day}  ·  ⚠ Freed 1.2 GB", f"{day}  ·  ⚠ Cleanup failed"]
    assert earlier_lines(runs[:1]) == []
    running = {
        "running": True,
        "waiting_for_start": False,
        "run": run_dict(3, "running", progress={"title": "Checking system files… 45%"}),
    }
    assert status_bar_text(running) == "◐ Maintenance: Checking system files… 45%"
    assert (
        status_bar_text({"running": False, "waiting_for_start": True, "run": None})
        == "◐ Maintenance: Starting…"
    )
    assert status_bar_text({"running": False, "waiting_for_start": False, "run": None}) == ""
    assert status_bar_text(None) == ""


def test_target_descriptions_carry_their_notes() -> None:
    by_id = {t["id"]: t for t in target_dicts()}
    assert target_description(by_id["user_temp"]).endswith("  ·  your account's files")
    assert target_description(by_id["update_cache"]).endswith("  ·  skipped while Windows Update is working")
    assert "  ·  " not in target_description(by_id["windows_temp"])


def test_recent_runs_are_announced_and_old_ones_are_not() -> None:
    from optimizer.features.maintenance import _recent

    now = datetime.now(UTC)
    recent = run_dict(1, ended_at=(now - timedelta(days=2)).isoformat().replace("+00:00", "Z"))
    old = run_dict(2, ended_at=(now - timedelta(days=30)).isoformat().replace("+00:00", "Z"))
    assert _recent(recent)
    assert not _recent(old)
