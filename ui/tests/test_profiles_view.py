"""Pure helpers of the Profiles section: row and result statuses, grouping by section, counts
texts, the apply dialog's details, the default selection and the suggested file name.

No window is created.
"""

from __future__ import annotations

from typing import Any

import pytest

from optimizer import app, theme
from optimizer.features import profiles as feature
from optimizer.widgets.profiles import (
    ADMIN_NOTE,
    NOT_SELECTED_STYLE,
    REASON_TEXT,
    RESTART_TEXT,
    RESULT_STYLE,
    REVERT_RESTART_TEXT,
    SECTION_HEADINGS,
    SECTION_ORDER,
    apply_details,
    counts_text,
    default_selection,
    filter_is_empty,
    fold_text,
    group_rows,
    plan_counts_text,
    restart_max,
    result_status,
    result_summary,
    row_status,
    section_counts_text,
    selected_restart,
    suggested_file_name,
)


def row(key: str, status: str = "change", **fields: Any) -> dict[str, Any]:
    base: dict[str, Any] = {
        "key": key,
        "section": key.split(":", 1)[0] + "s" if key.startswith(("tweak", "app")) else key.split(":", 1)[0],
        "title": key,
        "status": status,
        "detail": "",
        "reason": None,
        "caution": None,
        "risk": None,
        "restart": "none",
        "per_user": False,
        "selected": status == "change",
    }
    base.update(fields)
    return base


def test_row_status_names_every_skip_reason() -> None:
    assert row_status(row("tweak:a")) == ("◐", "Will change", theme.ACCENT)
    assert row_status(row("tweak:a", "already")) == ("✓", "Already set", theme.GOOD)
    for reason, text in REASON_TEXT.items():
        assert row_status(row("tweak:a", "skipped", reason=reason)) == ("–", text, theme.INK_MUTED)
    assert row_status(row("tweak:a", "skipped", reason="from_the_future")) == (
        "–",
        "Skipped",
        theme.INK_MUTED,
    )
    assert set(REASON_TEXT) == {
        "unsupported",
        "unreadable",
        "cannot_change",
        "other_account",
        "edition",
        "not_on_this_pc",
        "unknown_id",
    }
    assert REASON_TEXT["unknown_id"] == "Unknown to this version of Cairn"


def test_result_status_prefers_the_outcome_then_marks_unselected_changes() -> None:
    results = {"tweak:a": {"key": "tweak:a", "outcome": "failed", "details": []}}
    assert result_status(row("tweak:a"), results, {"tweak:a"}) == RESULT_STYLE["failed"]
    assert result_status(row("tweak:b"), results, {"tweak:a"}) == NOT_SELECTED_STYLE
    assert result_status(row("tweak:c", "already"), results, {"tweak:a"}) == row_status(
        row("tweak:c", "already")
    )
    assert RESULT_STYLE["applied"] == ("✓", "Applied", theme.GOOD)
    assert RESULT_STYLE["failed"] == ("⚠", "Failed", theme.CRITICAL)


def test_group_rows_follows_the_apply_order_and_puts_unknown_sections_last() -> None:
    rows = [
        {"key": "app:X", "section": "apps"},
        {"key": "future:1", "section": "future"},
        {"key": "tweak:a", "section": "tweaks"},
        {"key": "maintenance", "section": "maintenance"},
        {"key": "dns:x", "section": "dns"},
        {"key": "tweak:b", "section": "tweaks"},
        {"key": "startup:x", "section": "startup"},
        {"key": "windows_update:active_hours", "section": "windows_update"},
    ]
    groups = group_rows(rows)
    assert [s for s, _ in groups] == [*SECTION_ORDER, "future"]
    assert [r["key"] for r in groups[0][1]] == ["tweak:a", "tweak:b"]
    assert SECTION_ORDER == ("tweaks", "startup", "dns", "windows_update", "maintenance", "apps")
    assert SECTION_HEADINGS["apps"] == "Store apps to remove"


def test_counts_texts_leave_out_zero_parts() -> None:
    assert (
        plan_counts_text({"changes": 7, "already": 2, "skipped": 1})
        == "7 to change · 2 already set · 1 skipped"
    )
    assert plan_counts_text({"changes": 0, "already": 2, "skipped": 0}) == "2 already set"
    assert plan_counts_text({"changes": 0, "already": 0, "skipped": 0}) == "Nothing to change"
    assert (
        plan_counts_text({"changes": 1, "already": 1, "skipped": 0}, sep=", ") == "1 to change, 1 already set"
    )
    rows = [row("tweak:a"), row("tweak:b", "already"), row("tweak:c", "already"), row("tweak:d", "skipped")]
    assert section_counts_text(rows) == "1 to change · 2 already set · 1 skipped"
    assert fold_text(rows[1:], expanded=False) == "▸ 2 already set · 1 skipped"
    assert fold_text(rows[3:], expanded=True) == "▾ 1 skipped"


def test_counts_text_names_each_section() -> None:
    assert counts_text({"tweaks": 10}) == "10 settings"
    assert counts_text({"tweaks": 5, "apps": 21}) == "5 settings · 21 apps"
    assert (
        counts_text({"tweaks": 1, "apps": 1, "startup": 2, "dns": 1, "windows_update": 2, "maintenance": 1})
        == "1 setting · 1 app · 2 startup apps · DNS · Windows Update · maintenance"
    )
    assert counts_text({}) == "No settings"


def test_apply_details_are_truncated() -> None:
    rows = [row(f"tweak:t{i}", title=f"Setting {i}") for i in range(35)]
    lines = apply_details(rows)
    assert len(lines) == 31
    assert lines[0] == "• Setting 0"
    assert lines[-1] == "• …and 5 more"
    assert apply_details(rows[:2]) == ["• Setting 0", "• Setting 1"]
    assert apply_details(rows, limit=3)[-1] == "• …and 32 more"


def test_result_summary_keeps_the_applied_count() -> None:
    assert result_summary({"applied": 7, "already": 1, "skipped": 1, "failed": 1}) == (
        "7 applied, 1 already set, 1 skipped, 1 failed"
    )
    assert result_summary({"applied": 0, "already": 0, "skipped": 2, "failed": 0}) == "0 applied, 2 skipped"
    assert result_summary({"applied": 3}) == "3 applied"


@pytest.mark.parametrize(
    ("name", "expected"),
    [
        ("Gaming PC", "Gaming PC.json"),
        ('a\\b/c:d*e?f"g<h>i|j', "a-b-c-d-e-f-g-h-i-j.json"),
        ("  lots   of\tspace  ", "lots of space.json"),
        ("Café réglages", "Café réglages.json"),
        ("...", "Cairn profile.json"),
        ("", "Cairn profile.json"),
        ("CON", "Cairn profile.json"),
        ("x" * 80, "x" * 60 + ".json"),
        ("tab\there", "tab here.json"),
        ("bell\x07name", "bell-name.json"),
        (". dotted .", "dotted.json"),
    ],
)
def test_suggested_file_name(name: str, expected: str) -> None:
    assert suggested_file_name(name) == expected


def test_default_selection_honours_cautions_and_unchecked_keys() -> None:
    rows = [
        row("tweak:a"),
        row("tweak:b", caution="High risk: …", selected=False),
        row("tweak:c", "already"),
        row("maintenance", caution="Runs every Sunday…", selected=False),
        row("startup:user_run:X"),
    ]
    assert default_selection(rows) == {"tweak:a", "startup:user_run:X"}
    assert default_selection(rows, {"tweak:a"}) == {"startup:user_run:X"}


def test_restart_needs_take_the_strongest() -> None:
    rows = [row("tweak:a", restart="explorer"), row("tweak:b", restart="restart"), row("tweak:c")]
    assert selected_restart(rows, {"tweak:a", "tweak:c"}) == "explorer"
    assert selected_restart(rows, {"tweak:a", "tweak:b"}) == "restart"
    assert selected_restart(rows, set()) == "none"
    assert restart_max(["none", "sign_out", "explorer"]) == "sign_out"


def test_filter_is_empty() -> None:
    assert filter_is_empty(None)
    assert filter_is_empty({"registry": [], "dns": [], "power": False})
    assert not filter_is_empty({"registry": [{"hive": "HKLM"}], "power": False})
    assert not filter_is_empty({"registry": [], "power": True})


def test_texts_match_the_window_and_the_section() -> None:
    assert RESTART_TEXT == app.RESTART_TEXT
    assert REVERT_RESTART_TEXT == app.REVERT_RESTART_TEXT
    assert feature.ADMIN_NOTE == ADMIN_NOTE
    assert feature.OUTDATED_TEXT == feature.UNSUPPORTED_TEXT
    assert "this section" in feature.OUTDATED_TEXT
    assert feature.PROFILE_FILETYPES[0] == ("Cairn profile", "*.json")
