"""The sidebar's brand mark: the rendered cairn, the name's highlight, the cairn-only mark of
the rail, the frames a closed window's mark lets go of, the icon file, the windows' icon and
the README logo."""

from __future__ import annotations

import ctypes
from pathlib import Path
from typing import Any

import pytest
from PIL import Image, ImageChops

from optimizer import ICON_PATH
from optimizer.widgets.brand import (
    ICON_SIZES,
    ICON_TURN,
    LOGO_SIZE_PX,
    NAME,
    TEXT_BASE,
    TEXT_HIGHLIGHT,
    TURN_FRAMES,
    TURN_SECONDS,
    BrandMark,
    CairnRenderer,
    icon_images,
    set_window_icon,
    shimmer_colors,
    window_icon_images,
    write_icon,
    write_logo,
)

from .app_support import AppFactory, ctk, pump, theme

LOGO_PATH = Path(__file__).resolve().parents[2] / "docs" / "cairn.png"
# Largest difference per channel between a committed image and a fresh render.
RENDER_TOLERANCE = 2
# USER objects the process gains or loses between two counts without any window closing.
USER_JITTER = 3


def largest_difference(a: Image.Image, b: Image.Image) -> int:
    assert a.size == b.size
    extrema = ImageChops.difference(a.convert("RGBA"), b.convert("RGBA")).getextrema()
    return max(high for _low, high in extrema)


def bright_edge_pixels(image: Image.Image) -> int:
    """Partly transparent pixels brighter than luminance 150 next to an opaque one: the light
    outline around the silhouette."""
    px = image.load()
    width, height = image.size
    count = 0
    for y in range(height):
        for x in range(width):
            r, g, b, a = px[x, y]
            if not 0 < a < 255 or 0.2126 * r + 0.7152 * g + 0.0722 * b <= 150:
                continue
            if any(
                px[x + dx, y + dy][3] == 255
                for dx in (-1, 0, 1)
                for dy in (-1, 0, 1)
                if 0 <= x + dx < width and 0 <= y + dy < height
            ):
                count += 1
    return count


def test_cairn_frames_have_the_requested_size_and_turn() -> None:
    renderer = CairnRenderer(46)
    first, later = renderer.frame(0.0), renderer.frame(0.25)
    assert first.size == later.size == (46, 46)
    assert first.mode == later.mode == "RGBA"
    assert first.tobytes() != later.tobytes(), "the stones' surface moves as they turn"
    assert renderer.frame(1.0).tobytes() == first.tobytes(), "a full turn loops seamlessly"
    corner = first.getpixel((0, 0))
    assert corner[:3] == tuple(int(theme.PAGE[i : i + 2], 16) for i in (1, 3, 5)), "drawn on the page colour"


def test_transparent_render_leaves_the_background_clear() -> None:
    image = CairnRenderer(64, background=None).frame(0.0)
    assert image.getpixel((0, 0))[3] == 0
    assert image.getpixel((32, 40))[3] == 255, "the stack itself is opaque"


def test_highlight_crosses_the_name_and_then_rests() -> None:
    assert shimmer_colors(5, 0.0) == [TEXT_BASE] * 5
    middle = shimmer_colors(5, 0.3)
    assert middle[2] == TEXT_HIGHLIGHT, "the centre letter is brightest mid-sweep"
    assert middle[0] == middle[4] != TEXT_HIGHLIGHT
    assert shimmer_colors(5, 0.9) == [TEXT_BASE] * 5, "the highlight rests after its pass"


def test_icon_holds_every_windows_size(tmp_path: Path) -> None:
    path = tmp_path / "cairn.ico"
    write_icon(path)
    with Image.open(path) as icon:
        sizes = set(icon.info["sizes"])
    assert {(16, 16), (32, 32), (48, 48), (256, 256)} <= sizes


def test_shipped_icon_exists() -> None:
    icon = Path(__file__).resolve().parents[1] / "optimizer" / "assets" / "cairn.ico"
    assert icon.is_file()
    assert icon == ICON_PATH


def test_icon_sizes_are_rendered_for_windows(tmp_path: Path) -> None:
    images = icon_images()
    assert [image.size for image in images] == [(s, s) for s in ICON_SIZES]
    assert all(image.mode == "RGBA" for image in images)
    path = tmp_path / "cairn.ico"
    write_icon(path)
    with Image.open(path) as icon:
        assert sorted(icon.info["sizes"]) == [(s, s) for s in ICON_SIZES]
        for image in images:
            assert largest_difference(icon.ico.getimage(image.size), image) == 0, image.size


def test_shipped_icon_matches_the_generator() -> None:
    """The committed icon is what `scripts/make_icon.py` writes today."""
    fresh = {image.size: image for image in icon_images()}
    with Image.open(ICON_PATH) as icon:
        assert sorted(icon.info["sizes"]) == sorted(fresh)
        for size, image in fresh.items():
            assert largest_difference(icon.ico.getimage(size), image) <= RENDER_TOLERANCE, size


def test_readme_logo_matches_the_generator(tmp_path: Path) -> None:
    assert LOGO_PATH.is_file()
    fresh = CairnRenderer(LOGO_SIZE_PX, background=None).frame(ICON_TURN)
    with Image.open(LOGO_PATH) as logo:
        assert logo.size == (LOGO_SIZE_PX, LOGO_SIZE_PX)
        assert largest_difference(logo, fresh) <= RENDER_TOLERANCE
    written = tmp_path / "logo.png"
    write_logo(written)
    with Image.open(written) as logo:
        assert largest_difference(logo, fresh) == 0
        assert logo.getpixel((0, 0))[3] == 0, "transparent background"


def test_small_icons_have_an_outline() -> None:
    plain = CairnRenderer(16, background=None).frame(ICON_TURN)
    contrast = CairnRenderer(16, background=None, contrast=True).frame(ICON_TURN)
    assert contrast.size == (16, 16)
    outlined = bright_edge_pixels(contrast)
    assert outlined >= 16
    assert outlined >= 3 * bright_edge_pixels(plain)


def gui_objects() -> tuple[int, int]:
    """GDI and USER objects this process holds."""
    user32 = ctypes.WinDLL("user32")
    user32.GetGuiResources.argtypes = [ctypes.c_void_p, ctypes.c_uint]
    kernel32 = ctypes.WinDLL("kernel32")
    kernel32.GetCurrentProcess.restype = ctypes.c_void_p
    process = kernel32.GetCurrentProcess()
    return user32.GetGuiResources(process, 0), user32.GetGuiResources(process, 1)


def test_window_icon_images_are_the_shipped_sizes() -> None:
    images = window_icon_images()
    assert [image.size for image in images] == [(size, size) for size in ICON_SIZES]
    assert {image.mode for image in images} == {"RGBA"}


def test_an_unreadable_icon_file_gives_no_images(tmp_path: Path) -> None:
    broken = tmp_path / "broken.ico"
    broken.write_bytes(b"not an icon")
    assert window_icon_images(broken) == ()
    assert window_icon_images(tmp_path / "missing.ico") == ()


def test_windows_share_one_set_of_icon_photos(make_app: AppFactory, monkeypatch: pytest.MonkeyPatch) -> None:
    app, _ = make_app(elevated=True)
    calls: list[tuple[bool, tuple[Any, ...]]] = []
    original = ctk.CTkToplevel.wm_iconphoto

    def record(window: Any, default: bool = False, *photos: Any) -> None:
        calls.append((default, photos))
        original(window, default, *photos)

    monkeypatch.setattr(ctk.CTkToplevel, "wm_iconphoto", record)
    first, second = ctk.CTkToplevel(app), ctk.CTkToplevel(app)
    try:
        assert set_window_icon(first)
        assert set_window_icon(second, default=True)
        assert first._iconbitmap_method_called, "CustomTkinter keeps this icon"
        assert second._iconbitmap_method_called
    finally:
        first.destroy()
        second.destroy()
    assert [default for default, _ in calls] == [False, True]
    (_, one), (_, other) = calls
    assert len(one) == len(other) == len(ICON_SIZES)
    assert all(a is b for a, b in zip(one, other, strict=True)), "photos are made once"
    assert [(photo.width(), photo.height()) for photo in one] == [(size, size) for size in ICON_SIZES]
    pump(app, 0.3)
    assert app.errors == []


def test_window_icons_are_released_with_their_windows() -> None:
    # A hidden root without a frame loop, so nothing else allocates while this counts.
    root = ctk.CTk()
    try:
        root.withdraw()
        assert set_window_icon(root, default=True)
        pump(root, 0.3)  # type: ignore[arg-type]

        def open_and_close() -> None:
            window = ctk.CTkToplevel(root)
            assert set_window_icon(window)
            pump(root, 0.3)  # type: ignore[arg-type]  # past CustomTkinter's 200-ms icon timer
            window.destroy()
            pump(root, 0.05)  # type: ignore[arg-type]

        open_and_close()
        before = gui_objects()
        for _ in range(4):
            open_and_close()
        gdi, user = (now - then for now, then in zip(gui_objects(), before, strict=True))
    finally:
        root.destroy()
    # An icon read from the .ico file keeps 27 GDI and 9 USER objects per window, and
    # CustomTkinter's own icon 3 and 1. The USER count moves by a few objects without any
    # window closing, so the GDI count is the sharp check.
    assert gdi <= 2 and user <= USER_JITTER, (gdi, user)


def test_a_turning_mark_holds_one_photo_on_screen() -> None:
    # Every photo that has been on screen holds a GDI bitmap while it exists; the mark shows
    # all its poses through one. The window is shown (an unmapped canvas draws nothing) and
    # has no frame loop, so nothing else allocates while this counts.
    root = ctk.CTk()
    try:
        assert set_window_icon(root)
        mark = BrandMark(root, animate=True)
        mark.pack()
        pump(root, 0.3)  # type: ignore[arg-type]  # past CustomTkinter's 200-ms icon timer
        before = gui_objects()
        for i in range(TURN_FRAMES):
            mark.tick(i * TURN_SECONDS / TURN_FRAMES)
            root.update()
        assert mark.rendered_frames == TURN_FRAMES
        gdi, user = (now - then for now, then in zip(gui_objects(), before, strict=True))
    finally:
        root.destroy()
    assert gdi <= 2 and user <= USER_JITTER, (gdi, user)


def test_brand_wordmark_can_hide(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    brand = app.brand
    full = int(brand.cget("width"))
    assert brand.wordmark
    brand.set_wordmark(False)
    assert not brand.wordmark
    assert int(brand.cget("width")) == brand._logo_px < full
    assert {brand.itemcget(item, "state") for item in brand.find_withtag("wordmark")} == {"hidden"}
    assert brand.itemcget(brand._logo, "state") in {"", "normal"}, "the cairn stays"
    colors = [brand.itemcget(item, "fill") for item in brand._letters]
    brand.animate = True
    for i in range(10):
        brand.tick(i * 0.37)
    assert [brand.itemcget(item, "fill") for item in brand._letters] == colors, "no highlight while hidden"
    brand.set_wordmark(True)
    assert int(brand.cget("width")) == full
    assert {brand.itemcget(item, "state") for item in brand.find_withtag("wordmark")} == {"normal"}
    pump(app, 0.2)
    assert app.errors == []


def test_header_shows_the_name_and_the_cairn_turns(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    assert app.title() == NAME
    brand = app.brand
    brand.animate = True
    letters = "".join(brand.itemcget(item, "text") for item in brand._letters)
    assert letters == NAME
    photo = brand.itemcget(brand._logo, "image")
    assert photo == str(brand._photo)
    start = brand.rendered_frames
    for i in range(12):
        brand.tick(i * TURN_SECONDS / TURN_FRAMES)
    assert brand.rendered_frames >= start + 11, "one new frame per tick"
    assert brand._shown == 11
    assert brand.itemcget(brand._logo, "image") == photo, "every pose goes through one photo"
    pump(app, 0.5)
    assert app.errors == []


def test_a_closed_window_s_mark_lets_go_of_its_frames(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    brand = app.brand
    brand.animate = True
    for i in range(8):
        brand.tick(i * TURN_SECONDS / TURN_FRAMES)
    assert brand.rendered_frames >= 8
    app._request_close(force=True)
    assert brand.rendered_frames == 0
    assert brand._renderer is None and brand._photo is None
    brand.tick(TURN_SECONDS / 2)
    assert brand.rendered_frames == 0, "a late tick draws nothing"
    assert app.errors == []


def test_still_brand_does_not_change(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    brand = app.brand
    brand.animate = False
    before = brand.rendered_frames
    pose = brand._shown
    colors = [brand.itemcget(item, "fill") for item in brand._letters]
    for i in range(20):
        brand.tick(i * 0.37)
    assert brand.rendered_frames == before
    assert brand._shown == pose
    assert [brand.itemcget(item, "fill") for item in brand._letters] == colors
    assert app.errors == []
