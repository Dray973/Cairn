"""The sidebar's brand mark: the rendered cairn, the name's highlight, the cairn-only mark of
the rail, the frames a closed window's mark lets go of, the icon file, the windows' icon, the
README logo, and the prebuilt images the window shows when Pillow, or only its Tk hook, cannot be
loaded."""

from __future__ import annotations

import ast
import ctypes
import functools
import importlib.abc
import sys
import tkinter as tk
from collections.abc import Iterable, Sequence
from pathlib import Path
from types import ModuleType
from typing import Any

import PIL
import pytest
from PIL import Image, ImageChops

from optimizer import ICON_PATH, PACKAGE_DIR
from optimizer.widgets import brand as brand_module
from optimizer.widgets.brand import (
    FALLBACK_DIR,
    ICON_SIZES,
    ICON_TURN,
    LOGO_SIZE_PX,
    MARK_SCALES,
    NAME,
    TEXT_BASE,
    TEXT_HIGHLIGHT,
    TURN_FRAMES,
    TURN_SECONDS,
    BrandMark,
    CairnRenderer,
    fallback_images,
    fallback_mark_size,
    icon_images,
    mark_size,
    set_window_icon,
    shimmer_colors,
    window_icon_images,
    write_fallback_images,
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


TYPE_CHECKING_TESTS = {"TYPE_CHECKING", "typing.TYPE_CHECKING"}


@functools.cache
def fresh_fallback_images() -> dict[str, Image.Image]:
    return fallback_images()


class _BlockPillow(importlib.abc.MetaPathFinder):
    """Refuses every import of Pillow, as Smart App Control does when it blocks its modules."""

    def find_spec(
        self, fullname: str, path: Sequence[str] | None = None, target: ModuleType | None = None
    ) -> None:
        if fullname == "PIL" or fullname.startswith("PIL."):
            raise ModuleNotFoundError(f"No module named {fullname!r}", name=fullname)
        return None


@pytest.fixture
def without_pillow(monkeypatch: pytest.MonkeyPatch) -> None:
    """Pillow cannot be imported for the rest of the test: its loaded modules are hidden and
    every new import of it fails."""
    for name in [n for n in sys.modules if n == "PIL" or n.startswith("PIL.")]:
        monkeypatch.delitem(sys.modules, name)
    monkeypatch.setattr(sys, "meta_path", [_BlockPillow(), *sys.meta_path])


# Pillow's Tk hook: the extension module through which ImageTk's photos reach Tk.
TK_HOOK = "PIL._imagingtk"
# How loading the hook fails, by name: Smart App Control judges each module when it loads, so it
# can let _imaging load, which every program with Pillow loads, and refuse _imagingtk, which only
# programs that show Pillow's images in Tk load; and the hook itself refuses to load when it
# cannot find Tcl's or Tk's functions in the process.
TK_HOOK_FAILURES: dict[str, tuple[type[Exception], str]] = {
    "blocked": (
        ImportError,
        "DLL load failed while importing _imagingtk: An Application Control policy has blocked this file.",
    ),
    "no-tcl-routines": (RuntimeError, "Could not find Tcl routines"),
}


class _FailTkHook(importlib.abc.MetaPathFinder):
    """Makes every import of Pillow's Tk hook fail with a new `kind(message)`."""

    def __init__(self, kind: type[Exception], message: str) -> None:
        self.kind = kind
        self.message = message

    def find_spec(
        self, fullname: str, path: Sequence[str] | None = None, target: ModuleType | None = None
    ) -> None:
        if fullname == TK_HOOK:
            raise self.kind(self.message)
        return None


@pytest.fixture
def without_pillows_tk_hook(request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch) -> None:
    """Loading Pillow's Tk hook fails for the rest of the test, as the TK_HOOK_FAILURES entry
    named by the parameter says (a loaded hook is hidden); the rest of Pillow loads."""
    kind, message = TK_HOOK_FAILURES[request.param]
    monkeypatch.delitem(sys.modules, TK_HOOK, raising=False)
    monkeypatch.delattr(PIL, "_imagingtk", raising=False)
    monkeypatch.setattr(sys, "meta_path", [_FailTkHook(kind, message), *sys.meta_path])


def names_pillow(node: ast.AST) -> bool:
    """Whether `node` is an import of Pillow."""
    if isinstance(node, ast.Import):
        return any(alias.name == "PIL" or alias.name.startswith("PIL.") for alias in node.names)
    if isinstance(node, ast.ImportFrom):
        module = node.module or ""
        return node.level == 0 and (module == "PIL" or module.startswith("PIL."))
    return False


def eager_pillow_imports(source: str) -> list[int]:
    """Lines of `source` that import Pillow while the module loads: anywhere but inside a
    function or under `if TYPE_CHECKING:`."""
    found: list[int] = []

    def visit(nodes: Iterable[ast.AST]) -> None:
        for node in nodes:
            if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
                continue
            if isinstance(node, ast.If) and ast.unparse(node.test) in TYPE_CHECKING_TESTS:
                visit(node.orelse)
                continue
            if names_pillow(node):
                found.append(node.lineno)
            visit(ast.iter_child_nodes(node))

    visit(ast.parse(source).body)
    return found


def test_the_import_check_finds_eager_imports() -> None:
    assert eager_pillow_imports("from PIL import Image\n") == [1]
    assert eager_pillow_imports("import os\nimport PIL.Image as I\n") == [2]
    assert eager_pillow_imports("class A:\n    from PIL import ImageTk\n") == [2]
    assert eager_pillow_imports("try:\n    import PIL\nexcept ImportError:\n    pass\n") == [2]
    assert eager_pillow_imports("def f():\n    from PIL import Image\n") == []
    assert eager_pillow_imports("if TYPE_CHECKING:\n    from PIL import Image\n") == []


def test_no_module_needs_pillow_to_load() -> None:
    # Pillow's extension modules are not signed by their authors; Smart App Control may block
    # them. Every module of the app must still import, so the window can start.
    found = []
    for path in sorted(PACKAGE_DIR.rglob("*.py")):
        for line in eager_pillow_imports(path.read_text(encoding="utf-8")):
            found.append(f"{path.relative_to(PACKAGE_DIR).as_posix()}:{line}")
    assert found == []


def test_fallback_images_match_the_generator() -> None:
    """The committed images are what `write_fallback_images` writes today."""
    fresh = fresh_fallback_images()
    committed = {path.name for path in FALLBACK_DIR.iterdir() if path.is_file()}
    assert committed == set(fresh)
    for name, image in fresh.items():
        with Image.open(FALLBACK_DIR / name) as stored:
            assert stored.format == "PNG", name
            assert largest_difference(stored, image) <= RENDER_TOLERANCE, name


def test_fallback_images_cover_every_scale_and_icon_size(tmp_path: Path) -> None:
    images = fresh_fallback_images()
    for scale in MARK_SCALES:
        size = mark_size(scale)
        assert images[f"mark-{size}.png"].size == (size, size)
        assert fallback_mark_size(size) == size, "every listed scale has its own image"
        corner = images[f"mark-{size}.png"].getpixel((0, 0))
        assert corner[:3] == tuple(int(theme.PAGE[i : i + 2], 16) for i in (1, 3, 5)), "drawn on the page"
    for size in ICON_SIZES:
        assert images[f"icon-{size}.png"].size == (size, size)
        assert images[f"icon-{size}.png"].getpixel((0, 0))[3] == 0, "transparent like the icon"
    written = write_fallback_images(tmp_path)
    assert sorted(path.name for path in written) == sorted(images)
    assert all(path.parent == tmp_path for path in written)


def test_a_scale_without_its_own_image_gets_the_closest_one() -> None:
    sizes = sorted(mark_size(scale) for scale in MARK_SCALES)
    assert fallback_mark_size(sizes[0] - 10) == sizes[0]
    assert fallback_mark_size(sizes[-1] + 50) == sizes[-1]
    assert fallback_mark_size(sizes[0] + 5) == sizes[0], "46 + 5 is closer to 46 than to 58"
    assert fallback_mark_size(sizes[0] + 6) == sizes[1], "a tie goes to the larger image"


def test_the_renderer_needs_pillow(without_pillow: None, tmp_path: Path) -> None:
    assert brand_module.pillow() is None
    with pytest.raises(RuntimeError, match="Pillow"):
        CairnRenderer(16)
    with pytest.raises(RuntimeError, match="Pillow"):
        window_icon_images(tmp_path / "unread.ico")


def test_app_starts_without_pil(make_app: AppFactory, without_pillow: None) -> None:
    assert brand_module.pillow() is None
    app, _ = make_app(elevated=True)
    mark = app.brand
    assert mark.rendered_frames == 0, "nothing is rendered without Pillow"
    photo = mark._photo
    assert isinstance(photo, tk.PhotoImage), "Tk reads the prebuilt image itself"
    assert mark.itemcget(mark._logo, "image") == str(photo)
    wanted = mark_size(ctk.ScalingTracker.get_widget_scaling(mark))
    assert photo.width() == photo.height() == mark._logo_px == fallback_mark_size(wanted)
    icons = getattr(app, brand_module._ICON_PHOTOS)
    assert all(isinstance(icon, tk.PhotoImage) for icon in icons)
    assert [(icon.width(), icon.height()) for icon in icons] == [(size, size) for size in ICON_SIZES]
    # The cairn stays still; the name keeps its highlight (mid-sweep at 30 % of a cycle).
    mark.animate = True
    mark.tick(0.3 * brand_module.SHIMMER_SECONDS + 0.01)
    colors = [mark.itemcget(item, "fill") for item in mark._letters]
    assert colors == shimmer_colors(len(NAME), 0.3)
    assert colors[len(colors) // 2] == TEXT_HIGHLIGHT
    assert mark.rendered_frames == 0
    dialog = ctk.CTkToplevel(app)
    try:
        assert set_window_icon(dialog), "dialogs get the prebuilt icon too"
    finally:
        dialog.destroy()
    pump(app, 0.3)
    assert app.errors == []


@pytest.mark.parametrize("without_pillows_tk_hook", sorted(TK_HOOK_FAILURES), indirect=True)
def test_app_starts_without_pillows_tk_hook(make_app: AppFactory, without_pillows_tk_hook: None) -> None:
    # ImageTk loads the hook only for its first photo, which the window makes while it is
    # built: a hook that cannot be loaded must select the prebuilt images before that.
    app, _ = make_app(elevated=True)
    assert brand_module.pillow() is None
    mark = app.brand
    assert mark.rendered_frames == 0
    photo = mark._photo
    assert isinstance(photo, tk.PhotoImage), "Tk reads the prebuilt image itself"
    wanted = mark_size(ctk.ScalingTracker.get_widget_scaling(mark))
    assert photo.width() == photo.height() == mark._logo_px == fallback_mark_size(wanted)
    icons = getattr(app, brand_module._ICON_PHOTOS)
    assert all(isinstance(icon, tk.PhotoImage) for icon in icons)
    assert [(icon.width(), icon.height()) for icon in icons] == [(size, size) for size in ICON_SIZES]
    dialog = ctk.CTkToplevel(app)
    try:
        assert set_window_icon(dialog), "dialogs get the prebuilt icon too"
    finally:
        dialog.destroy()
    assert TK_HOOK not in sys.modules
    pump(app, 0.3)
    assert app.errors == []
