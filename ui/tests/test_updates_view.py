"""Pure helpers of the Updates section: status texts, summaries, selection, validation and the
Windows Update state lines. No window is created."""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

import pytest

from optimizer.features import updates as feature
from optimizer.widgets import updates as view

from . import fake_updates

UPGRADES = [
    {
        "id": "Contoso.Editor",
        "name": "Contoso Editor",
        "installed": "1.2.0",
        "available": "1.3.0",
        "source": "winget",
    },
    {
        "id": "Fabrikam.Tool",
        "name": "Fabrikam Tool",
        "installed": "1.0",
        "available": "2.0",
        "source": "winget",
        "explicit_only": True,
    },
    {
        "id": "9NTAILSPIN0001",
        "name": "Tailspin Notes",
        "installed": "1.0",
        "available": "1.1",
        "source": "msstore",
    },
    {
        "id": "Microsoft.AppInstaller",
        "name": "App Installer",
        "installed": "1.0",
        "available": "1.1",
        "source": "winget",
    },
    {
        "id": "Northwind.Cut…",
        "name": "Northwind",
        "installed": "1",
        "available": "2",
        "source": "winget",
        "selectable": False,
        "note": "winget cut this app's id short; update it from the app itself.",
    },
]


def local(text: str) -> datetime:
    return datetime.fromisoformat(text.replace("Z", "+00:00")).astimezone()


def test_version_change_joins_both_sides() -> None:
    assert view.version_change("1.2.0", "1.3.0") == "1.2.0 → 1.3.0"
    assert view.version_change("< 4.0", "4.2.1") == "< 4.0 → 4.2.1"
    assert view.version_change(None, "2.0") == "2.0"
    assert view.version_change("1.0", "") == "1.0"


@pytest.mark.parametrize(
    ("state", "message", "kind", "expected"),
    [
        ("queued", None, "upgrade", ("○ Waiting", "INK_MUTED")),
        ("running", None, "upgrade", ("◐ Updating…", "ACCENT")),
        ("running", None, "install", ("◐ Installing…", "ACCENT")),
        ("succeeded", None, "upgrade", ("✓ Updated", "GOOD")),
        ("succeeded", None, "install", ("✓ Installed", "GOOD")),
        ("already_current", None, "upgrade", ("✓ Already up to date", "GOOD")),
        ("already_installed", None, "install", ("✓ Already installed", "GOOD")),
        (
            "restart_required",
            "Restart Windows to finish.",
            "upgrade",
            ("⚠ Restart Windows to finish", "WARNING"),
        ),
        (
            "failed",
            "The app is open. Close it and try again.",
            "upgrade",
            (
                "⚠ Failed: The app is open. Close it and try again.",
                "SERIOUS",
            ),
        ),
        ("timed_out", None, "upgrade", ("⚠ Still running after 60 min", "WARNING")),
        ("left_running", None, "install", ("◐ Continues after Cairn closed", "INK_SECONDARY")),
        ("not_started", None, "upgrade", ("– Not started", "INK_MUTED")),
        ("mystery", None, "upgrade", ("– mystery", "INK_MUTED")),
    ],
)
def test_item_status_pairs_every_colour_with_an_icon(
    state: str, message: str | None, kind: str, expected: tuple[str, str]
) -> None:
    assert view.item_status(state, message, kind) == expected


def test_running_item_shows_its_progress() -> None:
    assert view.item_status("running", None, "upgrade", "45%") == ("◐ Updating…  ·  45%", "ACCENT")


def test_scan_summary_covers_every_outcome() -> None:
    now = local("2026-09-25T10:30:00Z")
    assert view.scan_summary(None) == "Not checked yet."
    checked = "2026-09-25T10:00:00Z"
    clock = local(checked).strftime("%H:%M")
    result = {"upgrades": UPGRADES[:2], "checked_at": checked, "winget_version": "1.29.380"}
    assert view.scan_summary(result, now) == f"2 updates available  ·  checked {clock}  ·  winget 1.29.380"
    one = {**result, "upgrades": UPGRADES[:1]}
    assert view.scan_summary(one, now).startswith("1 update available  ·  ")
    none = {**result, "upgrades": []}
    assert view.scan_summary(none, now) == f"All apps are up to date  ·  checked {clock}  ·  winget 1.29.380"
    error = {"error": {"message": "winget couldn't reach its sources. Check your internet connection."}}
    assert view.scan_summary(error) == "⚠ winget couldn't reach its sources. Check your internet connection."
    later = local(checked) + timedelta(days=2)
    assert view.scan_summary(result, later).split("  ·  ")[1] == f"checked {local(checked):%Y-%m-%d %H:%M}"


def test_scan_warnings_add_unread_rows_and_the_inventory() -> None:
    result = {"warnings": ["A source was skipped."], "unparsed_rows": 2, "inventory_complete": False}
    assert view.scan_warnings(result) == [
        "A source was skipped.",
        "2 lines of winget's list could not be read; those apps are not shown.",
        "Installed apps could not be read, so some apps may be missing from the list.",
    ]
    assert view.scan_warnings({"unparsed_rows": 1, "inventory_complete": True}) == [
        "1 line of winget's list could not be read; those apps are not shown."
    ]
    assert view.scan_warnings({"error": {"message": "x"}, "unparsed_rows": 3}) == []


def test_default_selection_leaves_out_store_explicit_and_unselectable_apps() -> None:
    assert view.default_selection(UPGRADES) == {"Contoso.Editor"}


def test_default_selection_leaves_out_apps_whose_failure_a_retry_cannot_change() -> None:
    rows = [UPGRADES[0], {**UPGRADES[0], "id": "Fabrikam.Player"}, {**UPGRADES[0], "id": "Northwind.Viewer"}]
    last = {
        "contoso.editor": {"state": "failed", "retry": False, "message": "x"},
        "fabrikam.player": {"state": "failed", "retry": True, "message": "x"},
        "northwind.viewer": {"state": "failed", "message": "x"},
    }
    assert view.default_selection(rows, last) == {"Fabrikam.Player", "Northwind.Viewer"}
    assert view.default_selection(rows) == {"Contoso.Editor", "Fabrikam.Player", "Northwind.Viewer"}
    updated = {"contoso.editor": {"state": "succeeded", "retry": False}}
    assert "Contoso.Editor" in view.default_selection(rows, updated), "only a failure leaves it out"
    assert view.retry_futile(last["contoso.editor"])
    assert not view.retry_futile(last["fabrikam.player"])
    assert not view.retry_futile(last["northwind.viewer"]), "a result without the flag can change"
    assert not view.retry_futile(None)


def test_row_notes_explain_each_kind_of_row() -> None:
    assert view.row_note(UPGRADES[0]) == ""
    assert view.row_note(UPGRADES[1]) == (
        "winget updates this app only when it is picked by name; it is not included in Update all."
    )
    assert (
        view.row_note(UPGRADES[2])
        == "Microsoft Store app  ·  if this fails, update it in the Microsoft Store."
    )
    assert view.row_note(UPGRADES[3]) == "Updated by the Microsoft Store."
    assert view.row_note(UPGRADES[4]) == "winget cut this app's id short; update it from the app itself."
    last = {"state": "failed", "message": "The app is open. Close it and try again."}
    assert view.row_note(UPGRADES[0], last) == "Last attempt: The app is open. Close it and try again."
    assert view.row_note(UPGRADES[0], {**last, "retry": True}) == view.row_note(UPGRADES[0], last)
    assert view.row_note(UPGRADES[0], {"state": "succeeded", "message": "x"}) == ""
    lasting = {"state": "failed", "retry": False, "message": "winget can't update this copy."}
    assert view.row_note(UPGRADES[0], lasting) == (
        "Last attempt: winget can't update this copy.  ·  Update all leaves it out; tick it to try again."
    )


def test_the_fake_names_a_batch_as_the_engine_does() -> None:
    assert fake_updates.batch_command_line("upgrade", ["Contoso.Editor", "Fabrikam.Player"]) == (
        "winget upgrade --id <id> --exact … for 2 apps: Contoso.Editor, Fabrikam.Player"
    )
    assert fake_updates.batch_command_line("install", ["Contoso.Editor"]) == (
        "winget install --id <id> --exact … for 1 app: Contoso.Editor"
    )
    ids = [f"Contoso.App{n:03}" for n in range(fake_updates.MAX_BATCH_ITEMS)]
    line = fake_updates.batch_command_line("upgrade", ids)
    assert len(line) <= fake_updates.MAX_COMMAND_LINE
    named = line.count("Contoso.App")
    assert line.endswith(f"Contoso.App{named - 1:03} and {len(ids) - named} more")


def test_update_item_carries_the_versions() -> None:
    assert view.update_item(UPGRADES[0]) == {
        "id": "Contoso.Editor",
        "source": "winget",
        "name": "Contoso Editor",
        "from": "1.2.0",
        "to": "1.3.0",
    }


def test_pause_text_states() -> None:
    now = local("2026-09-25T10:00:00Z")
    until = "2026-10-09T10:00:00Z"
    shown = local(until).strftime("%a %d %b, %H:%M")
    assert view.pause_text({"paused": False}, now) == "○ Updates are on"
    assert view.pause_text({"paused": True, "until": until}, now) == f"◐ Paused until {shown}"
    assert view.pause_text({"paused": True, "until": until}, now, outside=True) == (
        f"◐ Paused until {shown}  ·  set outside Cairn"
    )
    ended = "2026-09-01T00:00:00Z"
    assert view.pause_text({"paused": False, "expired": True, "until": ended}, now) == (
        f"○ The last pause ended on {local(ended):%a %d %b, %H:%M}"
    )
    next_year = local("2027-01-05T10:00:00Z")
    assert view.date_text(until, next_year) == local(until).strftime("%a %d %b %Y, %H:%M")


def test_active_hours_texts_and_limits() -> None:
    assert view.hours_text(8, 17) == "08:00–17:00"
    assert view.active_span(8, 17) == 9
    assert view.active_span(22, 6) == 8
    assert view.active_span(8, 2) == 18
    assert view.active_hours_error(8, 17) is None
    assert view.active_hours_error(5, 5) == "Start and end must differ."
    assert view.active_hours_error(8, 3) == "Active hours can span at most 18 hours."
    assert view.active_hours_text({"automatic": True}) == "○ Windows adjusts them automatically"
    assert view.active_hours_text({"automatic": False, "start": 8, "end": 17}) == "✓ 08:00–17:00"
    assert view.active_hours_text({"automatic": False, "start": 8, "end": 17}, outside=True) == (
        "✓ 08:00–17:00  ·  set outside Cairn"
    )
    policy = {"automatic": False, "start": 9, "end": 17, "policy": True}
    assert view.active_hours_text(policy) == "✓ 09:00–17:00  ·  set by a policy"


GOOD_IDS = ("Mozilla.Firefox", "Notepad++.Notepad++", "Adobe.Acrobat.Reader.64-bit", "a.b", "7zip.7zip")
BAD_IDS = ("", "-h", "a b", 'x"y', "..", "Foo", "Foo.", ".Foo", "a/b", "a:b", "a.b.c.d.e.f.g.h.i", "x" * 129)


@pytest.mark.parametrize("package", GOOD_IDS)
def test_valid_package_ids(package: str) -> None:
    assert view.valid_package_id(package)
    assert fake_updates.valid_package_id(package), "the fake mirrors the engine's rule"


@pytest.mark.parametrize("package", BAD_IDS)
def test_invalid_package_ids(package: str) -> None:
    assert not view.valid_package_id(package)
    assert not fake_updates.valid_package_id(package)


def test_store_ids_follow_the_store_rule() -> None:
    assert view.valid_package_id("9NBLGGH4NNS1", "msstore")
    assert view.valid_package_id("9NTAILSPIN0001", "msstore")
    assert not view.valid_package_id("9NBLGGH4NN", "msstore"), "too short"
    assert not view.valid_package_id("Contoso.Editor", "msstore")
    assert not view.valid_package_id("9NBLGGH4NNS1", "winget"), "winget ids have dotted parts"


def test_group_apps_follows_the_category_order() -> None:
    apps = [
        {"id": "Litware.Dev", "category": "developer"},
        {"id": "Contoso.Browser", "category": "browsers"},
        {"id": "Odd.App", "category": "unknown"},
        {"id": "Fabrikam.Chat", "category": "chat"},
    ]
    groups = view.group_apps(apps)
    assert [title for title, _ in groups] == ["Browsers", "Chat and calls", "Developer tools", "Other"]
    assert [a["id"] for a in groups[0][1]] == ["Contoso.Browser"]
    assert list(view.CATEGORY_TITLES) == list(fake_updates.CATEGORIES)


def test_install_summary_counts_installed_apps() -> None:
    apps = [{"id": "Contoso.Editor"}, {"id": "Northwind.Game"}, {"id": "Tailspin.Media"}]
    assert view.install_summary(apps, None) == "Checking which apps are installed…"
    assert view.install_summary(apps, ["contoso.editor"]) == (
        "Pick apps to install. Apps already on this PC are skipped.  ·  1 of 3 already installed"
    )


def test_batch_line_sums_the_outcomes() -> None:
    items = [
        {"state": "succeeded"},
        {"state": "succeeded"},
        {"state": "succeeded"},
        {"state": "failed"},
        {"state": "restart_required"},
    ]
    assert view.batch_line({"kind": "upgrade", "items": items}) == (
        "✓ 3 updated  ·  ⚠ 1 failed  ·  restart needed for 1"
    )
    installs = [{"state": "succeeded"}, {"state": "succeeded"}, {"state": "already_installed"}] + [
        {"state": "already_installed"}
    ]
    assert view.batch_line({"kind": "install", "items": installs}) == "✓ 2 installed  ·  2 already installed"
    stopped = [{"state": "succeeded"}, {"state": "not_started"}, {"state": "left_running"}]
    assert view.batch_line({"kind": "upgrade", "items": stopped}) == (
        "✓ 1 updated  ·  1 still running  ·  1 not started"
    )
    assert view.batch_line(None) == ""
    assert view.batch_line({"kind": "upgrade", "items": []}) == "Nothing was changed."


def test_edition_and_menu_helpers() -> None:
    edition = {"name": "Windows 11 Home", "version": "25H2", "build": "26200.9457"}
    assert view.edition_text(edition) == "Windows 11 Home 25H2  ·  build 26200.9457"
    assert view.edition_text({"name": "Windows 11 Pro"}) == "Windows 11 Pro"
    assert [view.pause_days(c) for c in view.PAUSE_CHOICES] == [7, 14, 21, 28, 35]
    assert view.defer_choice(None) == "Don't delay"
    assert view.defer_choice(90) == "90 days"
    assert view.defer_days("Don't delay") is None
    assert view.defer_days("180 days") == 180
    assert len(view.HOUR_CHOICES) == 24


def test_checking_and_clock_texts() -> None:
    assert view.checking_text() == "Checking for app updates…"
    assert view.checking_text(12_000) == "Checking for app updates…  ·  12 s"
    assert view.clock_text("not a time") == "not a time"


def test_user_texts_match_the_engine_and_name_the_app() -> None:
    assert view.WINGET_MISSING_TEXT == fake_updates.WINGET_MISSING_TEXT
    assert view.OTHER_USER_TEXT == fake_updates.OTHER_USER_TEXT
    assert view.USER_UNKNOWN_TEXT == fake_updates.USER_UNKNOWN_TEXT
    assert view.HOME_DEFER_TEXT == fake_updates.HOME_DEFER_TEXT
    assert view.POLICY_CAVEAT == fake_updates.POLICY_CAVEAT
    assert feature.HOME_DEFER_TEXT == view.HOME_DEFER_TEXT
    texts = [
        feature.UNSUPPORTED_TEXT,
        feature.IRREVERSIBLE_NOTE,
        feature.AGREEMENTS_NOTE,
        feature.RESTORE_POINT_TIP,
        feature.UPDATE_CONFIRM_MESSAGE,
        feature.INSTALL_CONFIRM_MESSAGE,
        feature.ALL_INSTALLED_TEXT,
        feature.BATCH_RUNNING_TEXT,
        feature.LOST_TEXT,
        feature.STOPPING_TEXT,
        feature.WINGET_OUTDATED_TEXT,
    ]
    for text in texts:
        assert "PC" + " " + "Optimizer" not in text
        assert " tab" not in text
    assert "Cairn" in feature.UPDATE_CONFIRM_MESSAGE
    assert feature.WINGET_OUTDATED_TEXT.format(version="1.5.0", minimum="1.6.0") == (
        "winget 1.5.0 is too old; Cairn needs 1.6.0 or newer. Update App Installer from the Microsoft "
        "Store, then choose Check again."
    )


def test_setting_descriptions_cover_every_setting() -> None:
    assert set(view.SETTING_DESCRIPTIONS) == set(fake_updates.WU_SETTINGS)
    assert view.SETTING_TITLES == fake_updates.WU_SETTING_TITLES


def test_date_text_uses_local_time() -> None:
    stamp = datetime(2026, 10, 12, 8, 0, tzinfo=UTC)
    text = view.date_text(stamp.isoformat(), stamp.astimezone())
    assert text == stamp.astimezone().strftime("%a %d %b, %H:%M")
