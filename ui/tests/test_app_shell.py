"""The window's shell with a FakeEngine: the rail's tooltip and brand, the History badge, the
account badge and its explanation, a copy that may not ask for administrator rights, the
dialogs' icon, activation by a second start, About (F1) and its links, the brand's redraw at
a new display scale, and the GDI and USER objects closed windows and dialogs give back.

Process helpers are replaced with recorders where a test reaches them; administrator dialogs
are never confirmed.
"""

from __future__ import annotations

import ctypes
import gc
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest

from optimizer import ICON_PATH, PROJECT_URL, __version__, system
from optimizer.app import (
    ADMIN_UNAVAILABLE_TITLE,
    OTHER_ACCOUNT_TITLE,
    UNCONFIRMED_ACCOUNT_TITLE,
)
from optimizer.widgets.about import NO_DATA_YET_TEXT
from optimizer.widgets.brand import ICON_SIZES, LOGO_SIZE

from .app_support import (
    App,
    AppFactory,
    EngineBridge,
    FakeEngine,
    MessageDialog,
    TimerResolution,
    ctk,
    dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    resize_to,
    scanned,
    theme,
)

WindowFactory = Callable[..., tuple[App, FakeEngine]]
# Windows and dialogs opened and closed while the GDI and USER objects are counted, after one
# that warms up.
CLOSED_WINDOWS = 6
CLOSED_DIALOGS = 4
# GDI objects a closed window keeps: those of its Tcl interpreter, which is never deleted.
INTERPRETER_GDI = 3


@pytest.fixture
def make_window() -> Iterator[WindowFactory]:
    """Like `make_app`, with the App's own keyword arguments: `make_window(elevated=...,
    can_elevate=..., start_note=..., **fake_engine_options)`."""
    created: list[App] = []
    timer = TimerResolution()
    timer.__enter__()
    gc.disable()

    def factory(
        *, elevated: bool, can_elevate: bool = True, start_note: str = "", **engine_options: Any
    ) -> tuple[App, FakeEngine]:
        engine = FakeEngine(elevated=elevated, **engine_options)
        app = App(
            engine=EngineBridge(module=engine),  # type: ignore[arg-type]
            elevated=elevated,
            can_elevate=can_elevate,
            start_note=start_note,
        )
        created.append(app)
        return app, engine

    try:
        yield factory
    finally:
        try:
            for app in created:
                if app._running:
                    app._request_close(force=True)
                if app.engine is not None:
                    app.engine.shutdown(wait=True)
            created.clear()
            gc.collect()
        finally:
            gc.enable()
            timer.__exit__(None, None, None)


def buttons(widget: Any) -> list[Any]:
    """Every CTkButton under `widget`."""
    found = []
    for child in widget.winfo_children():
        if isinstance(child, ctk.CTkButton):
            found.append(child)
        found += buttons(child)
    return found


def button(widget: Any, text: str) -> Any:
    matches = [b for b in buttons(widget) if b.cget("text") == text]
    assert len(matches) == 1, [b.cget("text") for b in buttons(widget)]
    return matches[0]


def gui_objects() -> tuple[int, int]:
    """GDI and USER objects this process holds."""
    user32 = ctypes.WinDLL("user32")
    user32.GetGuiResources.argtypes = [ctypes.c_void_p, ctypes.c_uint]
    kernel32 = ctypes.WinDLL("kernel32")
    kernel32.GetCurrentProcess.restype = ctypes.c_void_p
    process = kernel32.GetCurrentProcess()
    return user32.GetGuiResources(process, 0), user32.GetGuiResources(process, 1)


def badge_text(app: App) -> str:
    return str(app.account_badge.cget("text")) if app.account_badge.winfo_manager() else ""


def close_dialogs(app: App) -> None:
    for dialog in dialogs(app):
        dialog._cancel()
    pump(app, 0.1)


def test_the_rail_shows_the_cairn_alone_and_names_rows_in_a_tooltip(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)

    resize_to(app, 1000, 700)
    nav = app.nav
    assert nav.mode == "rail"
    assert not app.brand.wordmark
    assert app.brand.winfo_manager() == "grid", "the cairn stays"
    hidden = {app.brand.itemcget(item, "state") for item in app.brand.find_withtag("wordmark")}
    assert hidden == {"hidden"}
    assert app.brand.winfo_rootx() + app.brand.winfo_width() <= nav.winfo_rootx() + nav.winfo_width()
    for item in nav.items.values():
        assert item.name_label.winfo_manager() == "", item.name

    row = nav.items["Startup"]
    row.glyph_label._label.event_generate("<Enter>")
    pump(app, 0.2)
    assert nav.tooltip_text == "Startup"
    tip = nav.tooltip
    assert tip.winfo_toplevel() is app, "a label in the window, not a window of its own"
    assert tip.winfo_rootx() >= nav.winfo_rootx() + nav.winfo_width()
    middle = row.winfo_rooty() + row.winfo_height() / 2
    assert abs(tip.winfo_rooty() + tip.winfo_height() / 2 - middle) <= 2
    row.invoke()
    assert nav.tooltip_text is None, "a click hides the tooltip"
    assert app.current_section == "Startup"

    nav.about_item.glyph_label._label.event_generate("<Enter>")
    pump(app, 0.2)
    assert nav.tooltip_text == "About Cairn"

    resize_to(app, 1440, 900)
    assert nav.mode == "wide"
    assert app.brand.wordmark
    assert nav.tooltip_text is None
    shown = {app.brand.itemcget(item, "state") for item in app.brand.find_withtag("wordmark")}
    assert shown == {"normal"}
    row = nav.items["History"]
    row._left()
    row.glyph_label._label.event_generate("<Enter>")
    pump(app, 0.2)
    assert nav.tooltip_text is None, "wide rows show their names"
    pump(app, 5.0, until=lambda: idle(app))
    assert app.errors == []


def test_the_history_badge_counts_revertible_changes(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    history = app.nav.items["History"]
    assert history.badge == ""
    engine.applied.update({"privacy.activity_history", "gaming.game_mode"})
    app._refresh_journal()
    pump(app, 5.0, until=lambda: history.badge == "2")
    assert history.badge_label.cget("text") == "2"
    assert history.badge_label.winfo_manager() == "grid"
    engine.applied.clear()
    app._refresh_journal()
    pump(app, 5.0, until=lambda: history.badge == "")
    assert history.badge_label.winfo_manager() == ""
    assert app.errors == []


def test_the_badge_names_another_account_and_explains_it(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True, other_user=True)
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)
    resize_to(app, 1440, 900)
    pump(app, 5.0, until=lambda: "another account" in badge_text(app))
    assert badge_text(app) == "⚠ Administrator  ·  another account"
    assert app.account_badge.cget("text_color") == theme.WARNING
    assert engine.calls_named("elevated_as_other_user") == [()]

    app.account_badge._label.event_generate("<Button-1>")
    dialog = next_dialog(app)
    assert dialog.title_text == OTHER_ACCOUNT_TITLE
    assert "your own settings" in dialog_text(dialog)
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: idle(app))
    assert app.errors == []


def test_an_unconfirmed_account_is_said_and_nothing_fails(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True, other_user=None)
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)
    resize_to(app, 1440, 900)
    pump(app, 5.0, until=lambda: "account not confirmed" in badge_text(app))
    app.account_badge._label.event_generate("<Button-1>")
    dialog = next_dialog(app)
    assert dialog.title_text == UNCONFIRMED_ACCOUNT_TITLE
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: idle(app))
    assert app.errors == []


def test_the_signed_in_administrator_gets_the_plain_badge(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)
    resize_to(app, 1440, 900)
    pump(app, 5.0, until=lambda: bool(engine.calls_named("elevated_as_other_user")) and idle(app))
    assert badge_text(app) == "✓ Running as administrator"
    assert app.account_badge.cget("text_color") == theme.GOOD
    app.account_badge._label.event_generate("<Button-1>")
    pump(app, 0.3)
    assert dialogs(app) == [], "nothing to explain"
    assert app.errors == []


def test_a_standard_user_is_not_asked_about_the_account(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=False)
    pump(app, 5.0, until=lambda: scanned(app))
    assert engine.calls_named("elevated_as_other_user") == []
    assert app.restart_button is not None
    assert app.restart_button.cget("text") == "Restart as administrator"
    assert app.errors == []


def test_a_copy_that_is_not_installed_offers_no_relaunch(make_window: WindowFactory) -> None:
    app, _ = make_window(elevated=False, can_elevate=False)
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)
    resize_to(app, 1440, 900)
    assert app.restart_button is None
    assert "Restart as administrator" not in [b.cget("text") for b in buttons(app.top_bar)]
    assert badge_text(app) == "⚠ Standard user  ·  this copy isn't installed"

    assert app._guard() is False
    dialog = next_dialog(app)
    assert dialog.title_text == ADMIN_UNAVAILABLE_TITLE
    assert [b.cget("text") for b in buttons(dialog)] == ["OK"], "no cancel and no relaunch"
    dialog.confirm_button.invoke()
    pump(app, 0.2)
    assert dialogs(app) == []

    app.account_badge._label.event_generate("<Button-1>")
    dialog = next_dialog(app)
    assert "setup program" in dialog_text(dialog)
    dialog.confirm_button.invoke()
    assert app.errors == []


def test_a_declined_prompt_is_noted_in_the_badge(make_window: WindowFactory) -> None:
    app, _ = make_window(elevated=False, start_note="elevation-declined")
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)
    resize_to(app, 1440, 900)
    assert badge_text(app) == "⚠ Standard user  ·  administrator rights weren't granted"
    assert app.restart_button is not None
    assert app.errors == []


@pytest.mark.parametrize(
    "window",
    [
        {"elevated": False, "start_note": "elevation-declined"},
        {"elevated": False, "start_note": "elevation-failed:1223"},
        {"elevated": False, "can_elevate": False},
        {"elevated": True, "other_user": True},
        {"elevated": True, "other_user": None},
    ],
)
def test_long_badges_never_crowd_the_top_bar(make_window: WindowFactory, window: dict[str, Any]) -> None:
    app, _ = make_window(**window)
    pump(app, 5.0, until=lambda: scanned(app))
    app.minsize(900, 560)
    for width, height in ((1440, 900), (1240, 630), (1120, 700), (1000, 700)):
        resize_to(app, width, height)
        pump(app, 5.0, until=lambda: idle(app))
        pump(app, 0.2)
        shown = badge_text(app)
        assert shown in {"", app._badge.long, app._badge.short}, shown
        if width >= 1240:
            assert shown == app._badge.long, f"{width} px: {shown!r}"
        if not shown:
            continue
        badge = app.account_badge
        assert badge._label.winfo_reqwidth() <= badge.winfo_width() + 1, f"{width} px: {shown!r} is cut"
        right = badge.winfo_rootx() + badge.winfo_width()
        buttons_left = min(b.winfo_rootx() for b in (app.restart_button, app.revert_button) if b is not None)
        assert right <= buttons_left, f"{width} px: the badge runs into the buttons"
        title_right = app.section_title.winfo_rootx() + app.section_title.winfo_width()
        assert badge.winfo_rootx() >= title_right, f"{width} px: the badge runs into the title"
    assert app.errors == []


def test_every_dialog_shows_the_cairn_icon(make_app: AppFactory, monkeypatch: pytest.MonkeyPatch) -> None:
    # Tk cannot report a window's icon, so the calls are recorded: the icon file through
    # `iconbitmap`, or its images through `wm_iconphoto` (`set_window_icon`). A CTkToplevel
    # pumps events while it is built; when that takes longer than CustomTkinter's 200-ms icon
    # timer, the timer sets its own icon first. The icon a dialog keeps is the last one set,
    # and the flag checked below stops the timer from replacing it later.
    icons: list[tuple[str, str]] = []
    cairn = "the Cairn icon"
    cairn_photos = [(size, size) for size in ICON_SIZES]
    original_bitmap = MessageDialog.iconbitmap
    original_photo = MessageDialog.wm_iconphoto

    def record_bitmap(dialog: MessageDialog, bitmap: Any = None, default: Any = None) -> None:
        icons.append((str(dialog), cairn if Path(str(bitmap)) == ICON_PATH else str(bitmap)))
        original_bitmap(dialog, bitmap, default)

    def record_photo(dialog: MessageDialog, default: bool = False, *photos: Any) -> None:
        sizes = [(photo.width(), photo.height()) for photo in photos]
        icons.append((str(dialog), cairn if sizes == cairn_photos else f"photos {sizes}"))
        original_photo(dialog, default, *photos)

    monkeypatch.setattr(MessageDialog, "iconbitmap", record_bitmap)
    monkeypatch.setattr(MessageDialog, "wm_iconphoto", record_photo)
    app, _ = make_app(elevated=True, other_user=True)
    pump(app, 5.0, until=lambda: scanned(app) and app._account == "other")
    app.show_about()
    app._badge_clicked()
    pump(app, 0.5)
    shown = dialogs(app)
    assert len(shown) == 2
    assert ICON_PATH.is_file()
    for dialog in shown:
        assert isinstance(dialog, MessageDialog)
        set_icons = [icon for name, icon in icons if name == str(dialog)]
        assert set_icons, dialog.title_text
        assert set_icons[-1] == cairn, (dialog.title_text, set_icons)
        assert dialog._iconbitmap_method_called, "CustomTkinter's own icon is not set later"
    close_dialogs(app)
    assert app.errors == []


def test_a_second_start_brings_the_window_to_the_front(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, _ = make_app(elevated=True)
    raised: list[int] = []
    monkeypatch.setattr(system, "bring_window_to_front", lambda hwnd: raised.append(hwnd) or True)

    class OneRequest:
        def __init__(self) -> None:
            self.pending = 1
            self.stopped = False

        def activation_requested(self) -> bool:
            requested, self.pending = self.pending > 0, 0
            return requested

        def stop_activation(self) -> None:
            self.stopped = True

    request = OneRequest()
    app.instance = request  # type: ignore[assignment]
    pump(app, 0.6)
    assert raised == [int(app.wm_frame(), 16)]
    pump(app, 0.4)
    assert len(raised) == 1, "one request, one activation"
    app._request_close(force=True)
    assert request.stopped
    assert app.errors == []


def test_a_lock_without_an_activation_check_never_asks_for_the_window(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    app, _ = make_app(elevated=True)
    raised: list[int] = []
    monkeypatch.setattr(system, "bring_window_to_front", lambda hwnd: raised.append(hwnd) or True)

    class StopOnly:
        """All that closing the window needs of its lock."""

        def __init__(self) -> None:
            self.stopped = 0

        def stop_activation(self) -> None:
            self.stopped += 1

    lock = StopOnly()
    app.instance = lock  # type: ignore[assignment]
    pump(app, 0.6)
    assert raised == []
    assert app.errors == [], "the activation check runs every quarter second"
    app._request_close(force=True)
    assert lock.stopped == 1
    assert app.errors == []


def test_f1_shows_about_with_every_part_and_its_links(
    make_app: AppFactory, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app) and app._install_info is None)
    assert engine.calls_named("install_info") == [()]
    opened_uris: list[str] = []
    opened_folders: list[str] = []
    monkeypatch.setattr(system, "open_uri", opened_uris.append)
    monkeypatch.setattr(system, "open_folder", lambda path: opened_folders.append(str(path)))

    app.focus_force()
    app.event_generate("<F1>")
    dialog = next_dialog(app)
    assert dialog.title_text == "About Cairn"
    text = dialog_text(dialog)
    assert f"• App: {__version__}" in text
    assert f"• ⚠ Engine 0.0.0-test doesn't match the app ({__version__})" in text
    assert "• Hardware monitor: optimizer_telemetry " in text
    assert "• Python 3.12" in text and "Tk 8.6" in text and "CustomTkinter 6" in text
    assert "• Development copy: " in text
    assert "• Installed copy: none" in text
    data = "C:\\Users\\Test\\AppData\\Local\\PCOptimizer"
    assert f"• Data: {data} (journal, tool logs and Cairn's logs)" in text
    assert "MIT License" in text

    button(dialog, "Project page").invoke()
    assert opened_uris == [PROJECT_URL]
    button(dialog, "Open data folder").invoke()
    assert opened_folders == [], "the fake data folder does not exist"
    assert app.status_message.cget("text") == NO_DATA_YET_TEXT
    dialog.confirm_button.invoke()

    app.engine.journal_path = lambda: str(tmp_path / "journal.db")  # type: ignore[method-assign]
    app.show_about()
    dialog = next_dialog(app)
    assert f"• Data: {tmp_path} " in dialog_text(dialog)
    button(dialog, "Open data folder").invoke()
    assert opened_folders == [str(tmp_path)]
    dialog.confirm_button.invoke()
    pump(app, 5.0, until=lambda: idle(app))
    assert app.errors == []


def test_about_without_an_engine_says_so(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    engine, app.engine = app.engine, None
    try:
        app.show_about()
        text = dialog_text(next_dialog(app))
    finally:
        app.engine = engine
    assert "• Engine: not loaded" in text
    close_dialogs(app)
    assert app.errors == []


def test_the_brand_redraws_at_a_new_display_scale(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    brand = app.brand
    scale = ctk.ScalingTracker.get_widget_scaling(brand)
    width, height = int(brand.cget("width")), int(brand.cget("height"))
    brand._rescale(scale * 2, scale * 2)
    assert brand._logo_px == round(LOGO_SIZE * scale * 2)
    # Text widths follow the font's hinting, so they grow a little less than twice.
    assert int(brand.cget("width")) >= width * 1.8
    assert int(brand.cget("height")) >= height * 1.8
    assert brand.rendered_frames == 1, "frames of the old size are dropped"
    brand.set_wordmark(False)
    brand._rescale(scale, scale)
    assert not brand.wordmark, "the rail keeps its cairn-only mark"
    brand.set_wordmark(True)
    assert (int(brand.cget("width")), int(brand.cget("height"))) == (width, height)
    letters = "".join(brand.itemcget(item, "text") for item in brand._letters)
    assert letters == "Cairn"
    pump(app, 0.2)
    assert app.errors == []


# Seven windows open and close one after another, each after its first scan: on a busy machine
# that takes longer than the suite's per-test limit of 60 s.
@pytest.mark.timeout(240)
def test_closed_windows_give_back_their_gdi_and_user_objects(make_window: WindowFactory) -> None:
    # A process may hold 10,000 GDI objects and the suite opens hundreds of windows; when Tk
    # cannot get one it ends the process. An icon read from the .ico file with `iconbitmap`
    # keeps 27 GDI and 9 USER objects per window until the process ends, CustomTkinter's own
    # icon 3 and 1. A closed window keeps only its Tcl interpreter's GDI objects:
    # CustomTkinter's trackers keep a reference to every root, so the interpreter is never
    # deleted (and so never on a thread other than Tk's, which would abort the process).
    def open_and_close() -> None:
        app, _ = make_window(elevated=True)
        pump(app, 5.0, until=lambda: scanned(app))
        pump(app, 0.5)
        brand = app.brand
        assert brand.rendered_frames > 1 or not brand.animate, "the brand's frames are drawn"
        assert app.errors == []
        app._request_close(force=True)
        assert app.engine is not None
        app.engine.shutdown(wait=True)
        assert brand.rendered_frames == 0, "the closed window's brand lets go of its frames"

    open_and_close()
    before = gui_objects()
    for _ in range(CLOSED_WINDOWS):
        open_and_close()
    gdi, user = (now - then for now, then in zip(gui_objects(), before, strict=True))
    kept = f"kept by {CLOSED_WINDOWS} closed windows"
    assert gdi <= (INTERPRETER_GDI + 1) * CLOSED_WINDOWS, f"{gdi} GDI objects {kept}"
    assert user <= CLOSED_WINDOWS, f"{user} USER objects {kept}"


def test_closed_dialogs_give_back_their_gdi_and_user_objects(make_app: AppFactory) -> None:
    # Dialogs are the windows the suite opens most. With the icon's photos every object comes
    # back when a dialog closes; with the icon file each dialog keeps 27 GDI and 9 USER objects.
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    pump(app, 1.0)

    def open_and_close() -> None:
        app.show_about()
        dialog = next_dialog(app)
        pump(app, 0.3)  # past CustomTkinter's 200-ms icon timer
        dialog.confirm_button.invoke()
        pump(app, 0.1)
        assert dialogs(app) == []

    open_and_close()
    before = gui_objects()
    for _ in range(CLOSED_DIALOGS):
        open_and_close()
    gdi, user = (now - then for now, then in zip(gui_objects(), before, strict=True))
    kept = f"kept by {CLOSED_DIALOGS} closed dialogs"
    assert gdi <= CLOSED_DIALOGS, f"{gdi} GDI objects {kept}"
    assert user <= CLOSED_DIALOGS, f"{user} USER objects {kept}"
    assert app.errors == []
