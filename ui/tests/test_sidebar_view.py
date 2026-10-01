"""Pure helpers of the window's shell: the sidebar's mode and densities, the top bar's badge fit
and the account badge for every combination of rights, account and start note. No window."""

from __future__ import annotations

import pytest

from optimizer import theme
from optimizer.app import (
    ACCOUNT_OTHER,
    ACCOUNT_PENDING,
    ACCOUNT_SAME,
    ACCOUNT_UNKNOWN,
    ADMIN_UNAVAILABLE_TEXT,
    NOT_INSTALLED_TEXT,
    OTHER_ACCOUNT_TEXT,
    OTHER_ACCOUNT_TITLE,
    UNCONFIRMED_ACCOUNT_TEXT,
    UNCONFIRMED_ACCOUNT_TITLE,
    BadgeState,
    badge_state,
)
from optimizer.sections import GROUPS, SECTIONS
from optimizer.widgets.sidebar import (
    BRAND_BLOCK,
    DENSITIES,
    FOOTER_BLOCK,
    RAIL_BELOW,
    badge_fit,
    choose_density,
    nav_height,
    sidebar_mode,
)

ITEMS = len(SECTIONS)
GROUP_COUNT = len(GROUPS)
# Status bar height below the sidebar (logical px).
STATUS_BAR = 28


def available(window_height: int) -> int:
    """Height the section rows get in a window `window_height` logical px tall."""
    return window_height - STATUS_BAR - BRAND_BLOCK - FOOTER_BLOCK


def test_mode_switches_at_1080_px() -> None:
    assert RAIL_BELOW == 1080
    assert sidebar_mode(1079) == "rail"
    assert sidebar_mode(1079.9) == "rail"
    assert sidebar_mode(1080) == "wide"
    assert sidebar_mode(1440) == "wide"


def test_nav_height_arithmetic() -> None:
    regular, compact, dense, tiny = DENSITIES
    assert nav_height(regular, 16, 4, rail=False) == 16 * (32 + 2) + 4 * 26
    assert nav_height(compact, 16, 4, rail=False) == 16 * 29 + 4 * 20 == 544
    # Headings hidden: gaps between the groups only.
    assert nav_height(dense, 16, 4, rail=False) == 16 * 26 + 3 * 8
    assert nav_height(tiny, 16, 4, rail=False) == 16 * 22 + 3 * 4
    # The rail has rules between groups instead of headings.
    assert nav_height(regular, 16, 4, rail=True) == 16 * 34 + 3 * 13
    assert nav_height(dense, 16, 4, rail=True) == 16 * 26 + 3 * 13
    assert nav_height(regular, 3, 1, rail=True) == 3 * 34


@pytest.mark.parametrize(
    ("height", "density"),
    [(900, "regular"), (700, "compact"), (678, "compact"), (630, "dense"), (600, "dense"), (400, "tiny")],
)
def test_density_follows_the_window_height(height: int, density: str) -> None:
    assert available(700) == 566
    assert choose_density(available(height), ITEMS, GROUP_COUNT, rail=False).name == density


def test_the_smallest_density_is_the_fallback() -> None:
    assert choose_density(0, ITEMS, GROUP_COUNT, rail=False) == DENSITIES[-1]
    assert choose_density(10_000, ITEMS, GROUP_COUNT, rail=True) == DENSITIES[0]


def test_badge_fit_prefers_the_long_text() -> None:
    assert badge_fit(300, 250, 90) == "long"
    assert badge_fit(250, 250, 90) == "long"
    assert badge_fit(249, 250, 90) == "short"
    assert badge_fit(90, 250, 90) == "short"
    assert badge_fit(89, 250, 90) == "none"
    assert badge_fit(-10, 250, 90) == "none"


def test_badge_for_an_administrator_of_the_signed_in_account() -> None:
    for account in (ACCOUNT_PENDING, ACCOUNT_SAME):
        for can_elevate in (True, False):
            assert badge_state(True, account, "", can_elevate) == BadgeState(
                "✓ Running as administrator", "✓ Administrator", theme.GOOD
            )


def test_badge_for_an_administrator_of_another_account() -> None:
    state = badge_state(True, ACCOUNT_OTHER, "", True)
    assert state.long == "⚠ Administrator  ·  another account"
    assert state.short == "⚠ Another account"
    assert state.color == theme.WARNING
    assert (state.explanation_title, state.explanation) == (OTHER_ACCOUNT_TITLE, OTHER_ACCOUNT_TEXT)
    assert "your own settings" in state.explanation


def test_badge_when_the_account_is_not_confirmed() -> None:
    state = badge_state(True, ACCOUNT_UNKNOWN, "", True)
    assert state.long == "⚠ Administrator  ·  account not confirmed"
    assert state.short == "⚠ Account not confirmed"
    assert state.color == theme.WARNING
    assert (state.explanation_title, state.explanation) == (
        UNCONFIRMED_ACCOUNT_TITLE,
        UNCONFIRMED_ACCOUNT_TEXT,
    )


def test_badge_for_a_standard_user() -> None:
    for account in (ACCOUNT_PENDING, ACCOUNT_SAME, ACCOUNT_OTHER, ACCOUNT_UNKNOWN):
        state = badge_state(False, account, "", True)
        assert state == BadgeState(
            "⚠ Standard user  ·  most changes need administrator rights", "⚠ Standard user", theme.WARNING
        )


def test_badge_after_the_prompt_was_declined() -> None:
    state = badge_state(False, ACCOUNT_PENDING, "elevation-declined", True)
    assert state == BadgeState(
        "⚠ Standard user  ·  administrator rights weren't granted", "⚠ Standard user", theme.WARNING
    )


def test_badge_after_the_prompt_failed() -> None:
    state = badge_state(False, ACCOUNT_PENDING, "elevation-failed:1234", True)
    assert state.long == "⚠ Standard user  ·  couldn't get administrator rights"
    assert state.short == "⚠ Standard user"
    assert state.color == theme.WARNING
    assert state.explanation == "Windows reported error 1234 when Cairn asked for administrator rights."
    assert state.explanation_title


def test_badge_for_a_copy_that_is_not_installed() -> None:
    for note in ("", "elevation-declined"):
        state = badge_state(False, ACCOUNT_PENDING, note, False)
        assert state.long == "⚠ Standard user  ·  this copy isn't installed"
        assert state.short == "⚠ Standard user"
        assert state.color == theme.WARNING
        assert state.explanation == NOT_INSTALLED_TEXT
    assert "Install Cairn with its setup program" in ADMIN_UNAVAILABLE_TEXT


def test_every_badge_text_marks_its_status_with_an_icon() -> None:
    for elevated in (True, False):
        for account in (ACCOUNT_PENDING, ACCOUNT_SAME, ACCOUNT_OTHER, ACCOUNT_UNKNOWN):
            for note in ("", "elevation-declined", "elevation-failed:5"):
                for can_elevate in (True, False):
                    state = badge_state(elevated, account, note, can_elevate)
                    for text in (state.long, state.short):
                        assert text[:1] in {"✓", "⚠"}, text
                    assert bool(state.explanation) == bool(state.explanation_title)
