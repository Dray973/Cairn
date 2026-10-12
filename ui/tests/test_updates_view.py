"""Pure helpers of the Updates section: status texts, summaries, selection, search, validation
and the Windows Update state lines, and the parsing of `scripts/check_app_ids.py`. No window is
created, no process is started and the network is not used."""

from __future__ import annotations

import functools
import importlib.util
import re
import sys
from datetime import UTC, datetime, timedelta
from pathlib import Path
from types import ModuleType

import pytest

from optimizer.features import updates as feature
from optimizer.widgets import updates as view

from . import fake_updates

ROOT = Path(__file__).resolve().parents[2]
ID_CHECK = ROOT / "scripts" / "check_app_ids.py"
CATALOG_DOC = ROOT / "docs" / "app-catalog.md"

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
        (
            "succeeded",
            None,
            "install",
            ("◐ winget reported success. The next check shows whether Windows lists it.", "INK_SECONDARY"),
        ),
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
        (
            "succeeded_unlisted",
            None,
            "install",
            (
                "◐ winget reported success, but Windows doesn't list it yet. Choose Check again in a minute.",
                "INK_SECONDARY",
            ),
        ),
        # Windows' own pending-restart note on an app that exited cleanly (winget's exit code
        # does not carry it): shown after the report, as a warning.
        (
            "succeeded",
            "Windows reports a pending restart; restart Windows to finish.",
            "install",
            (
                "◐ winget reported success. The next check shows whether Windows lists it.  ·  "
                "⚠ Windows reports a pending restart; restart Windows to finish.",
                "WARNING",
            ),
        ),
        (
            "succeeded",
            "Windows reports a pending restart; restart Windows to finish.",
            "upgrade",
            ("✓ Updated  ·  ⚠ Windows reports a pending restart; restart Windows to finish.", "WARNING"),
        ),
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


def test_the_nine_categories_have_the_engines_titles() -> None:
    assert view.CATEGORY_TITLES == {
        "browsers": "Browsers",
        "chat": "Chat and calls",
        "gaming": "Gaming",
        "media": "Media",
        "creative": "Creative tools",
        "productivity": "Productivity",
        "utilities": "Utilities",
        "security": "Security and privacy",
        "developer": "Developer tools",
    }
    assert {a["category"] for a in fake_updates.DEFAULT_APPS} == set(fake_updates.CATEGORIES), (
        "the fake's default list has an app in every category"
    )


def test_app_sections_keep_the_given_dicts_with_unknown_categories_last() -> None:
    apps = [
        {"id": "Odd.App", "category": "arcade"},
        {"id": "Wingtip.Studio", "category": "creative"},
        {"id": "Fabrikam.Vault", "category": "security"},
        {"id": "Contoso.Paint", "category": "creative"},
    ]
    sections = view.app_sections(apps)
    assert [(key, title, [a["id"] for a in items]) for key, title, items in sections] == [
        ("creative", "Creative tools", ["Wingtip.Studio", "Contoso.Paint"]),
        ("security", "Security and privacy", ["Fabrikam.Vault"]),
        ("other", "Other", ["Odd.App"]),
    ]
    assert sections[0][2][0] is apps[1], "sections hold the list's own dicts"


def test_case_words_split_names_written_together() -> None:
    assert view.case_words("NanaZip") == "Nana Zip"
    assert view.case_words("M2Team.NanaZip") == "M2 Team.Nana Zip"
    assert view.case_words("OBSProject.OBSStudio") == "OBS Project.OBS Studio"
    assert view.case_words("7zip.7zip") == "7 zip.7 zip"
    assert view.case_words("PowerToys") == "Power Toys"
    assert view.case_words("ONLYOFFICE") == "ONLYOFFICE"
    assert view.case_words("Unzipped") == "Unzipped"


@pytest.mark.parametrize(
    ("query", "found"),
    [
        ("zip", ["Contoso.Zip", "Fabrikam.NanoZip"]),
        ("ZIP", ["Contoso.Zip", "Fabrikam.NanoZip"]),
        ("nano", ["Fabrikam.NanoZip"]),
        ("toys", ["Northwind.PowerToys"]),
        ("studio code", ["Litware.StudioCode"]),
        ("code studio", ["Litware.StudioCode"]),
        ("litware", ["Litware.StudioCode"]),
        ("cafe", ["Tailspin.Cafe"]),
        ("ai", ["Adatum.Ai"]),
        ("zipped", []),
        ("", ["Contoso.Zip", "Fabrikam.NanoZip", "Tailspin.Unzipped", "Northwind.PowerToys"]),
    ],
)
def test_app_matches_word_starts_of_the_name_and_id(query: str, found: list[str]) -> None:
    apps = [
        {"id": "Contoso.Zip", "name": "Contoso Zip"},
        {"id": "Fabrikam.NanoZip", "name": "NanoZip"},
        {"id": "Tailspin.Unzipped", "name": "Unzipped"},
        {"id": "Northwind.PowerToys", "name": "PowerToys"},
        {"id": "Litware.StudioCode", "name": "Studio Code"},
        {"id": "Tailspin.Cafe", "name": "Café Corner"},
        {"id": "Adatum.Ai", "name": "Adatum Ai"},
        {"id": "Adatum.Available", "name": "Available Space"},
    ]
    terms = view.search_terms(query)
    matched = [a["id"] for a in apps if view.app_matches(a, terms)]
    if query:
        assert matched == found
    else:
        assert matched == [a["id"] for a in apps], "no words match every app"


def test_find_text_counts_and_limits() -> None:
    assert view.find_text(0, 0) == "No app on the list matches your search."
    assert view.find_text(1, 1) == "1 app matches."
    assert view.find_text(12, 12) == "12 apps match."
    assert view.find_text(57, 40) == "57 apps match. The first 40 are shown; type more to narrow the search."


def test_section_text_counts_installed_and_selected_apps() -> None:
    apps = [{"id": "Contoso.Browser"}, {"id": "Fabrikam.Browser"}, {"id": "Northwind.Browser"}]
    assert view.section_text("Browsers", apps, None, set(), expanded=False) == "▸ Browsers  ·  3 apps"
    assert view.section_text("Browsers", apps[:1], set(), set(), expanded=True) == "▾ Browsers  ·  1 app"
    installed = {"contoso.browser"}
    chosen = {"contoso.browser", "fabrikam.browser"}
    assert view.section_text("Browsers", apps, installed, chosen, expanded=True) == (
        "▾ Browsers  ·  3 apps  ·  1 installed  ·  1 selected"
    ), "a ticked app that is installed is not counted as selected"


def test_missing_defaults_ignore_case() -> None:
    apps = [{"id": "contoso.browser"}, {"id": "Wingtip.Notes"}]
    assert view.missing_defaults(apps, ["Contoso.Browser", "Fabrikam.Chat", "Northwind.Game"]) == 2
    assert view.missing_defaults(apps, []) == 0
    assert view.DEFAULT_LIST_NOTICE.format(apps=view.apps_text(2)) == (
        "The default list has 2 apps your list doesn't. Restore the default list to see them; your own "
        "additions are then removed."
    )


def test_day_text_is_the_local_date() -> None:
    stamp = "2026-09-20T10:00:00+00:00"
    assert view.day_text(stamp) == local(stamp).strftime("%Y-%m-%d")
    assert view.day_text("not a time") == "not a time"
    assert view.day_text(None) == ""


def appx_record(family: str, stamp: str, *, active: bool = True) -> dict[str, object]:
    """A journal export's Appx entry, with generic package names."""
    return {
        "id": 1,
        "session_id": 1,
        "recorded_at": stamp,
        "package_full_name": f"{family.split('_')[0]}_1.0.0.0_x64__{family.split('_')[1]}",
        "package_family": family,
        "install_location": "C:\\Program Files\\WindowsApps\\Package",
        "all_users": False,
        "active": active,
        "reverted_at": None if active else "2026-09-21T10:00:00+00:00",
    }


def test_removed_apps_reads_the_active_removals_of_apps_debloat_also_lists() -> None:
    assert feature.DEBLOAT_OVERLAP == {"Microsoft.Teams": "MSTeams"}
    export = {
        "appx": [
            appx_record("MSTeams_8wekyb3d8bbwe", "2026-09-10T10:00:00+00:00", active=False),
            appx_record("MSTeams_8wekyb3d8bbwe", "2026-09-20T10:00:00+00:00"),
            appx_record("MSTeams_8wekyb3d8bbwe", "2026-09-15T10:00:00+00:00"),
            appx_record("Contoso.Weather_8wekyb3d8bbwe", "2026-09-22T10:00:00+00:00"),
        ]
    }
    items = [{"id": "microsoft.teams"}, {"id": "Contoso.Weather"}, {"id": "Fabrikam.Chat"}]
    assert feature.removed_apps(export, items) == {"microsoft.teams": "2026-09-20T10:00:00+00:00"}
    assert feature.removed_apps({"appx": export["appx"][:1]}, items) == {}, "a reverted removal is no record"
    assert feature.removed_apps({}, items) == {}
    assert feature.DEBLOAT_REMOVED_NOTE.format(date="2026-09-20") == (
        "Cairn removed this app on 2026-09-20 (History). Installing it again doesn't change that record."
    )


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
    unlisted = [{"state": "succeeded"}, {"state": "succeeded_unlisted"}, {"state": "succeeded_unlisted"}]
    assert view.batch_line({"kind": "install", "items": unlisted}) == ("✓ 1 installed  ·  ◐ 2 not listed yet")
    assert view.batch_line(None) == ""
    assert view.batch_line({"kind": "upgrade", "items": []}) == "Nothing was changed."
    # A pending restart that appeared during the batch is noted even when every app succeeded.
    pending = {"kind": "install", "items": [{"state": "succeeded"}], "pending_restart": True}
    assert view.batch_line(pending) == (
        "✓ 1 installed  ·  ⚠ Windows reports a pending restart; restart Windows to finish"
    )
    # Without the flag, nothing is added.
    assert view.batch_line({"kind": "install", "items": [{"state": "succeeded"}]}) == "✓ 1 installed"


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
        feature.DEBLOAT_REMOVED_NOTE,
        feature.DEPENDENCIES_NOTE,
        feature.UNLISTED_TEXT,
        view.REPORTED_TEXT,
        view.DEFAULT_LIST_NOTICE,
        view.FIND_NONE_TEXT,
        view.FIND_LIMIT_TEXT,
    ]
    for text in texts:
        assert "PC" + " " + "Optimizer" not in text
        assert " tab" not in text
    assert "Cairn" in feature.UPDATE_CONFIRM_MESSAGE
    assert feature.UNLISTED_TEXT == (
        "winget reported success, but Windows doesn't list it yet. Choose Check again in a minute."
    )
    for unverified in (feature.UNLISTED_TEXT, view.REPORTED_TEXT):
        assert "✓" not in unverified and "Installed" not in unverified
    assert feature.DEPENDENCIES_NOTE == (
        "When an app needs a runtime that isn't on this PC, such as the Visual C++ runtime or .NET, winget "
        "installs it as part of that app's install; it gets no rows of its own in History › Activity."
    )


def test_install_texts_name_no_cause_and_claim_no_effect() -> None:
    texts = [
        view.UNLISTED_TEXT,
        view.REPORTED_TEXT,
        feature.DEPENDENCIES_NOTE,
        view.DEFAULT_LIST_NOTICE,
        view.FIND_NONE_TEXT,
        view.FIND_ONE_TEXT,
        view.FIND_COUNT_TEXT,
        view.FIND_LIMIT_TEXT,
        feature.DEBLOAT_REMOVED_NOTE,
        view.batch_line({"kind": "install", "items": [{"state": "succeeded_unlisted"}]}),
    ]
    for text in texts:
        lowered = text.lower()
        for refused in ("enforced", "in effect", "works", "blocked by windows", "did not keep", "by windows"):
            assert refused not in lowered, (refused, text)
    assert feature.WINGET_OUTDATED_TEXT.format(version="1.5.0", minimum="1.6.0") == (
        "winget 1.5.0 is too old; Cairn needs 1.6.0 or newer. Update App Installer from the Microsoft "
        "Store, then choose Check again."
    )


def test_the_catalog_document_quotes_the_install_result_texts() -> None:
    doc = CATALOG_DOC.read_text(encoding="utf-8")
    for text in (view.REPORTED_TEXT, view.UNLISTED_TEXT):
        assert f'"{text}"' in doc, text


def test_the_size_rule_of_the_catalog_document_names_its_exceptions() -> None:
    # Rule 7 keeps two or three leading apps of a kind; the default list has more browsers, so
    # the rule names that exception.
    check = id_check()
    apps = check.read_lists(check.APPS_RS.read_text(encoding="utf-8"))
    browsers = [a for a in apps if a.list_name == "DEFAULT_APPS" and a.category == "Browsers"]
    assert len(browsers) > 3
    lines = CATALOG_DOC.read_text(encoding="utf-8").splitlines()
    rule = next(line for line in lines if line.startswith("7. "))
    assert "Browsers and password managers are the exceptions" in rule, rule


def test_setting_descriptions_cover_every_setting() -> None:
    assert set(view.SETTING_DESCRIPTIONS) == set(fake_updates.WU_SETTINGS)
    assert view.SETTING_TITLES == fake_updates.WU_SETTING_TITLES


def test_date_text_uses_local_time() -> None:
    stamp = datetime(2026, 10, 12, 8, 0, tzinfo=UTC)
    text = view.date_text(stamp.isoformat(), stamp.astimezone())
    assert text == stamp.astimezone().strftime("%a %d %b, %H:%M")


# -- scripts/check_app_ids.py ----------------------------------------------------------


@functools.cache
def id_check() -> ModuleType:
    """The id check script, loaded once from its file (registered under its name, as its
    dataclasses need); only its parsing functions are called."""
    spec = importlib.util.spec_from_file_location("check_app_ids", ID_CHECK)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_the_id_check_reads_both_lists_of_the_engine() -> None:
    check = id_check()
    apps = check.read_lists(check.APPS_RS.read_text(encoding="utf-8"))
    defaults = [a for a in apps if a.list_name == "DEFAULT_APPS"]
    held = [a for a in apps if a.list_name == "HELD_APPS"]
    assert len(defaults) == 121 and len(held) == 2
    assert defaults[0].package_id == "Mozilla.Firefox" and defaults[0].category == "Browsers"
    assert len({a.package_id.lower() for a in apps}) == len(apps)
    assert {a.category for a in apps} == {
        "Browsers",
        "Chat",
        "Gaming",
        "Media",
        "Creative",
        "Productivity",
        "Utilities",
        "Security",
        "Developer",
    }
    text = """
pub const DEFAULT_APPS: &[(&str, &str, AppCategory)] = &[
    ("Contoso.Editor", "Contoso Editor", AppCategory::Productivity),
    (
        "Fabrikam.Player.64-bit",
        "Fabrikam Player",
        AppCategory::Media,
    ),
];
pub const OTHER: &[(&str, &str, AppCategory)] = &[
    ("Northwind.Game", "Northwind Game", AppCategory::Gaming),
];
"""
    assert [(a.list_name, a.package_id, a.name) for a in check.read_lists(text)] == [
        ("DEFAULT_APPS", "Contoso.Editor", "Contoso Editor"),
        ("DEFAULT_APPS", "Fabrikam.Player.64-bit", "Fabrikam Player"),
    ]


def test_the_id_check_only_shows_packages() -> None:
    check = id_check()
    assert check.show_command("winget", "Contoso.Editor") == [
        "winget",
        "show",
        "--id",
        "Contoso.Editor",
        "--exact",
        "--source",
        "winget",
        "--accept-source-agreements",
        "--disable-interactivity",
    ]
    source = ID_CHECK.read_text(encoding="utf-8")
    assert re.findall(r'"(install|upgrade|uninstall|import|source add|source reset)"', source) == []
    assert "shell=True" not in source


def test_the_id_check_reads_winget_show() -> None:
    check = id_check()
    shown = "Found Contoso Editor [Contoso.Editor]\nVersion: 1.2.3\nPublisher: Contoso\n"
    assert check.show_status(0, shown, "contoso.editor") == ("found", "1.2.3")
    assert check.show_status(0, "Found Contoso Editor [Contoso.Editor]\n", "Contoso.Editor") == ("found", "")
    missing = "No package found matching input criteria.\n"
    assert check.show_status(0x8A150014, missing, "Contoso.Editor") == ("missing", "")
    assert check.show_status(0x8A150014 - (1 << 32), "", "Contoso.Editor") == ("missing", "")
    assert check.show_status(0x8A15000F, "", "Contoso.Editor") == ("error", "winget ended with 0x8A15000F")
    assert check.show_status(0, "Found Fabrikam Player [Fabrikam.Player]\n", "Contoso.Editor")[0] == "error"


def test_the_id_check_names_the_package_folder_on_github() -> None:
    check = id_check()
    assert check.manifest_dir("Mozilla.Firefox") == "manifests/m/Mozilla/Firefox"
    assert check.manifest_dir("Microsoft.VCRedist.2015+.x64") == "manifests/m/Microsoft/VCRedist/2015+/x64"
    assert check.manifest_dir("7zip.7zip") == "manifests/7/7zip/7zip"
    assert check.listing_found([{"name": "1.2.3", "type": "dir"}, {"name": ".validation", "type": "file"}])
    assert not check.listing_found([{"name": "README.md", "type": "file"}])
    assert not check.listing_found({"message": "Not Found"})


@pytest.mark.parametrize(
    ("manifest", "package_id", "hints"),
    [
        (
            "Installers:\n- InstallerType: exe\n  InstallerSwitches:\n    Silent: /S\n",
            "Contoso.Editor",
            [],
        ),
        (
            "InstallerType: exe\nInstallLocationRequired: true\n",
            "Contoso.Launcher",
            [
                "the installer needs an install folder, which Cairn never passes",
                "an exe installer without a silent switch can show its own window",
            ],
        ),
        (
            "Installers:\n- InstallerType: msix\n  ElevationRequirement: elevationProhibited\n",
            "Contoso.Music",
            ["an installer refuses to run from an administrator app (elevationProhibited)"],
        ),
        (
            "InstallerType: zip\nNestedInstallerType: portable\n",
            "Contoso.Tool",
            ["a portable package: no Start menu entry, and Windows doesn't list it as installed"],
        ),
        (
            "InstallerType: nullsoft\nRequireExplicitUpgrade: true\n",
            "Contoso.Chat",
            ["winget now updates it only when it is named (RequireExplicitUpgrade)"],
        ),
        (
            "InstallerType: exe\nInstallerSwitches:\n  Silent: --silent\nRequireExplicitUpgrade: true\n",
            "Discord.Discord",
            [],
        ),
    ],
)
def test_the_id_check_hints_at_installer_quirks(manifest: str, package_id: str, hints: list[str]) -> None:
    assert id_check().manifest_warnings(manifest, package_id) == hints


def test_the_id_check_judges_the_installer_winget_picks() -> None:
    check = id_check()
    shown = "Found Contoso Editor [Contoso.Editor]\nVersion: 1.2.3\nInstaller:\n  Installer Type: nullsoft\n"
    assert check.shown_installer_type(shown) == "nullsoft"
    assert check.shown_installer_type("Found Contoso Editor [Contoso.Editor]\n") == ""
    # A manifest that also lists a portable zip for another architecture.
    both = (
        "Installers:\n- Architecture: x64\n  InstallerType: nullsoft\n"
        "- Architecture: arm64\n  InstallerType: zip\n  NestedInstallerType: portable\n"
    )
    portable = "a portable package: no Start menu entry, and Windows doesn't list it as installed"
    assert check.manifest_warnings(both, "Contoso.Editor", "nullsoft") == []
    assert check.manifest_warnings(both, "Contoso.Editor", "zip") == [portable]
    assert check.manifest_warnings(both, "Contoso.Editor") == [portable], "without the type, any installer"
    exe = "Installers:\n- InstallerType: exe\n- InstallerType: msix\n"
    assert check.manifest_warnings(exe, "Contoso.Chat", "msix") == []
    assert check.manifest_warnings(exe, "Contoso.Chat", "exe") == [
        "an exe installer without a silent switch can show its own window"
    ]


def test_the_id_check_reads_the_entry_of_the_picked_installer() -> None:
    check = id_check()
    manifest = (
        "PackageIdentifier: Contoso.Paint\nInstallerType: zip\nInstallers:\n"
        "- Architecture: x64\n  NestedInstallerType: wix\n"
        "  InstallerUrl: https://example.com/contoso%20paint.msi.zip\n"
        "- Architecture: x64\n  NestedInstallerType: portable\n"
        "  InstallerUrl: https://example.com/contoso-paint.portable.zip\n"
        "ManifestType: installer\n"
    )
    shown = (
        "Found Contoso Paint [Contoso.Paint]\nInstaller:\n  Installer Type: zip\n"
        "  Installer Url: https://example.com/contoso paint.msi.zip\n"
    )
    url = check.shown_installer_url(shown)
    assert url == "https://example.com/contoso paint.msi.zip"
    assert check.shown_installer_url("Found Contoso Paint [Contoso.Paint]\n") == ""
    picked = check.picked_installer(manifest, url)
    assert "NestedInstallerType: wix" in picked and "portable" not in picked
    assert picked.startswith("PackageIdentifier: Contoso.Paint\nInstallerType: zip\n"), "the root fields stay"
    assert check.manifest_warnings(picked, "Contoso.Paint", "zip") == []
    other = check.picked_installer(manifest, "https://example.com/contoso-paint.portable.zip")
    assert check.manifest_warnings(other, "Contoso.Paint", "zip") == [
        "a portable package: no Start menu entry, and Windows doesn't list it as installed"
    ]
    assert check.picked_installer(manifest, "https://example.com/elsewhere.zip") == manifest
    assert check.picked_installer(manifest, "") == manifest


def test_the_id_check_exit_codes() -> None:
    check = id_check()
    app = check.App("DEFAULT_APPS", "Contoso.Editor", "Contoso Editor", "Productivity")
    found = check.Result(app, "found", "1.2.3")
    assert check.exit_code([found], 1) == 0
    assert check.exit_code([found, check.Result(app, "missing")], 2) == 1
    assert check.exit_code([found, check.Result(app, "error", "x")], 2) == 2
    assert check.exit_code([found], 2) == 2, "a check cut short is incomplete"
