"""Animated brand mark at the top of the sidebar: a stone cairn and the product name.

The cairn is four ellipsoid stones shaded per pixel (diffuse light from the upper left, a
specular highlight and a rim light in the accent colour). Their surface mottling turns
around the vertical axis, so the stack reads as a solid, slowly spinning object. Frames are
rendered lazily, one per frame-loop tick, replayed once rendered and shown through one photo;
a destroyed mark lets go of them. The name is extruded canvas text whose letters carry a
travelling highlight.

With animations turned off in Windows (Settings > Accessibility > Visual effects >
Animation effects) the mark is drawn once and stays still. In the sidebar's rail mode only the
cairn shows (`set_wordmark`), and the mark redraws itself when the window moves to a display
with another scale.

The same renderer draws the app icon (`write_icon`, with native high-contrast renders for the
small sizes) and the README logo (`write_logo`); `scripts/make_icon.py` writes both. Windows
get the icon through `set_window_icon`, which Tk releases with the window.
"""

from __future__ import annotations

import ctypes
import functools
import logging
import math
import tkinter as tk
import tkinter.font as tkfont
from pathlib import Path

import customtkinter as ctk
from PIL import Image, ImageChops, ImageDraw, ImageFilter, ImageTk

from .. import APP_NAME, ICON_PATH, theme

log = logging.getLogger(__name__)

NAME = APP_NAME
# Logical size of the square cairn image and of the name text.
LOGO_SIZE = 46
NAME_SIZE = 25
# One full turn of the stones.
TURN_SECONDS = 7.5
TURN_FRAMES = 96
# One pass of the highlight across the name, including the pause after it.
SHIMMER_SECONDS = 4.2
SUPERSAMPLE = 2

# Stones as (centre x, centre y, half width, half height) in units of the square, bottom first.
STONES = (
    (0.50, 0.805, 0.43, 0.13),
    (0.47, 0.615, 0.33, 0.112),
    (0.535, 0.447, 0.245, 0.097),
    (0.50, 0.298, 0.16, 0.08),
)
STONE_COLORS = ("#4a5466", "#566174", "#636e82", "#727d92")
# High-contrast renders (small icon sizes): stones blended this far towards INK_SECONDARY and
# a light outline around the silhouette, so the cairn reads on dark and light taskbars.
CONTRAST_BLEND = 0.25
OUTLINE_COLOR = (0xC3, 0xC2, 0xB7)
OUTLINE_ALPHA = 140
# Icon sizes: the small ones are rendered natively in high contrast, the others downsampled
# from the 256 px render.
ICON_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
NATIVE_ICON_MAX = 32
ICON_TURN = 0.08
# Attribute of a Tk root that holds the window icon's photos for its interpreter.
_ICON_PHOTOS = "_cairn_icon_photos"
# Set by CTk and CTkToplevel when an icon was given; while False they load their own .ico.
_CTK_ICON_FLAG = "_iconbitmap_method_called"
LOGO_SIZE_PX = 224
TEXT_BASE = theme.blend(theme.ACCENT, theme.INK, 0.28)
TEXT_HIGHLIGHT = theme.INK
EXTRUDE_COLORS = tuple(theme.blend(theme.ACCENT, theme.PAGE, a) for a in (0.16, 0.24, 0.34, 0.46))

_LIGHT = (-0.55, -0.62, 0.56)


def _rgb(color: str) -> tuple[int, int, int]:
    return int(color[1:3], 16), int(color[3:5], 16), int(color[5:7], 16)


def _hex(rgb: tuple[float, float, float]) -> str:
    return "#" + "".join(f"{max(0, min(255, round(v))):02x}" for v in rgb)


def _normalized(v: tuple[float, float, float]) -> tuple[float, float, float]:
    n = math.sqrt(sum(c * c for c in v))
    return v[0] / n, v[1] / n, v[2] / n


def animations_enabled() -> bool:
    """Windows' "Animation effects" setting (SPI_GETCLIENTAREAANIMATION); True when unknown."""
    try:
        value = ctypes.c_int(1)
        if ctypes.windll.user32.SystemParametersInfoW(0x1042, 0, ctypes.byref(value), 0):
            return bool(value.value)
    except (AttributeError, OSError):
        pass
    return True


class CairnRenderer:
    """Renders the cairn at a fixed pixel size; `frame(turn)` gives the stones turned by
    `turn` (0..1 of a revolution). Everything that does not move is computed once."""

    def __init__(self, size: int, background: str | None = theme.PAGE, *, contrast: bool = False) -> None:
        self.size = size
        self.background = background
        self.contrast = contrast
        s = size * SUPERSAMPLE
        self._s = s
        self._base = self._backdrop(s)
        light = _normalized(_LIGHT)
        half = _normalized((light[0], light[1], light[2] + 1.0))
        rim_rgb = _rgb(theme.ACCENT)
        covered: set[int] = set()
        records: list[tuple[int, float, float, float, float, float, float, float, float, float, float]] = []
        # Upper stones sit in front of the ones below them, so they claim pixels first.
        for index in reversed(range(len(STONES))):
            cx, cy, a, b = STONES[index]
            color = STONE_COLORS[index]
            if contrast:
                color = theme.blend(theme.INK_SECONDARY, color, CONTRAST_BLEND)
            base = _rgb(color)
            phase = index * 1.7
            x0, x1 = max(0, int((cx - a) * s)), min(s, int((cx + a) * s) + 1)
            y0, y1 = max(0, int((cy - b) * s)), min(s, int((cy + b) * s) + 1)
            for y in range(y0, y1):
                ny = ((y + 0.5) / s - cy) / b
                if ny <= -1.0 or ny >= 1.0:
                    continue
                for x in range(x0, x1):
                    offset = y * s + x
                    if offset in covered:
                        continue
                    nx = ((x + 0.5) / s - cx) / a
                    r2 = nx * nx + ny * ny
                    if r2 >= 1.0:
                        continue
                    covered.add(offset)
                    nz = math.sqrt(1.0 - r2)
                    diffuse = max(0.0, nx * light[0] + ny * light[1] + nz * light[2])
                    specular = max(0.0, nx * half[0] + ny * half[1] + nz * half[2]) ** 30
                    rim = (1.0 - nz) ** 2.6 * (0.35 + 0.65 * max(0.0, nx))
                    shade = 0.24 + 0.82 * diffuse
                    lon = math.atan2(nx, nz)
                    lat = math.asin(max(-1.0, min(1.0, ny)))
                    records.append(
                        (
                            offset * 4,
                            base[0] * shade + 255 * 0.5 * specular + rim_rgb[0] * rim * 0.75,
                            base[1] * shade + 255 * 0.5 * specular + rim_rgb[1] * rim * 0.75,
                            base[2] * shade + 255 * 0.5 * specular + rim_rgb[2] * rim * 0.75,
                            base[0] * 0.42,
                            base[1] * 0.42,
                            base[2] * 0.42,
                            lon,
                            lat,
                            phase,
                            shade,
                        )
                    )
        self._records = records

    def _backdrop(self, s: int) -> bytes:
        """The background with the soft contact shadow under the bottom stone (none in a
        high-contrast render, where it would blur the outline)."""
        bg = (0, 0, 0, 0) if self.background is None else (*_rgb(self.background), 255)
        img = Image.new("RGBA", (s, s), bg)
        if self.contrast:
            return img.tobytes()
        shadow = Image.new("L", (s, s), 0)
        cx, cy, a, b = STONES[0]
        ImageDraw.Draw(shadow).ellipse(
            ((cx - a * 1.02) * s, (cy + b * 0.35) * s, (cx + a * 1.02) * s, (cy + b * 1.55) * s), fill=150
        )
        shadow = shadow.filter(ImageFilter.GaussianBlur(s * 0.035))
        dark = Image.new("RGBA", (s, s), (0, 0, 0, 255))
        img.paste(dark, (0, 0), shadow)
        return img.tobytes()

    def frame(self, turn: float) -> Image.Image:
        """The cairn with its stones turned by `turn` of a revolution."""
        buf = bytearray(self._base)
        rot = turn * 2.0 * math.pi
        sin = math.sin
        for offset, r, g, b, mr, mg, mb, lon, lat, phase, shade in self._records:
            u = lon + rot
            m = 0.6 * sin(2.0 * u + phase) + 0.4 * sin(5.0 * u + 1.8 * lat + 2.0 * phase)
            m *= shade
            buf[offset] = max(0, min(255, int(r + mr * m)))
            buf[offset + 1] = max(0, min(255, int(g + mg * m)))
            buf[offset + 2] = max(0, min(255, int(b + mb * m)))
            buf[offset + 3] = 255
        big = Image.frombytes("RGBA", (self._s, self._s), bytes(buf))
        image = big.resize((self.size, self.size), Image.Resampling.LANCZOS)
        return _outlined(image) if self.contrast else image


def _outlined(image: Image.Image) -> Image.Image:
    """`image` over a light one-pixel outline of its silhouette."""
    mask = image.getchannel("A").point(lambda a: 255 if a >= 128 else 0)
    ring = ImageChops.subtract(mask.filter(ImageFilter.MaxFilter(3)), mask)
    outline = Image.new("RGBA", image.size, (*OUTLINE_COLOR, 0))
    outline.putalpha(ring.point(lambda a: OUTLINE_ALPHA if a else 0))
    return Image.alpha_composite(outline, image)


def icon_images() -> list[Image.Image]:
    """Every icon size, smallest first: sizes up to NATIVE_ICON_MAX rendered natively in high
    contrast, the others downsampled from the 256 px render."""
    large = CairnRenderer(256, background=None).frame(ICON_TURN)
    images = []
    for size in ICON_SIZES:
        if size <= NATIVE_ICON_MAX:
            images.append(CairnRenderer(size, background=None, contrast=True).frame(ICON_TURN))
        elif size == large.width:
            images.append(large)
        else:
            images.append(large.resize((size, size), Image.Resampling.LANCZOS))
    return images


def write_icon(path: str | Path) -> None:
    """Writes the cairn as a multi-size Windows icon with a transparent background."""
    images = icon_images()
    largest = images[-1]
    largest.save(
        str(path),
        format="ICO",
        sizes=[im.size for im in images],
        append_images=images[:-1],
    )


def write_logo(path: str | Path, size: int = LOGO_SIZE_PX) -> None:
    """Writes the cairn as a transparent PNG of `size` pixels (the README logo)."""
    CairnRenderer(size, background=None).frame(ICON_TURN).save(str(path), format="PNG")


@functools.cache
def window_icon_images(path: Path = ICON_PATH) -> tuple[Image.Image, ...]:
    """Every size of the icon file at `path` as an RGBA image, smallest first; empty when the
    file cannot be read."""
    try:
        with Image.open(path) as icon:
            images = []
            for size in sorted(icon.info["sizes"]):
                icon.size = size
                images.append(icon.copy().convert("RGBA"))
    except (OSError, KeyError, ValueError):
        log.warning("the window icon %s could not be read", path, exc_info=True)
        return ()
    return tuple(images)


def set_window_icon(window: tk.Tk | tk.Toplevel, *, default: bool = False) -> bool:
    """Gives `window` the Cairn icon: every size of `ICON_PATH` through `wm iconphoto`, from
    photos made once per Tk interpreter.

    Tk frees an icon made from photos when its window is destroyed, while an icon read from an
    .ico file with `wm iconbitmap` stays allocated until the process ends (27 GDI and 9 USER
    objects per call for this icon; a process may hold 10,000 GDI objects). With `default`,
    windows created later without an icon of their own get this one too.

    CustomTkinter windows load their own .ico 200 ms after they are built unless `iconbitmap`
    was called; the flag they check is set here, so they keep this icon. False when the icon
    file cannot be read: the window keeps the icon it has.
    """
    root = window._root()
    photos: tuple[ImageTk.PhotoImage, ...] | None = getattr(root, _ICON_PHOTOS, None)
    if photos is None:
        photos = tuple(ImageTk.PhotoImage(image, master=root) for image in window_icon_images())
        setattr(root, _ICON_PHOTOS, photos)
    if not photos:
        return False
    window.wm_iconphoto(default, *photos)
    if hasattr(window, _CTK_ICON_FLAG):
        setattr(window, _CTK_ICON_FLAG, True)
    return True


def shimmer_colors(count: int, t: float) -> list[str]:
    """Colours of `count` letters at `t` (0..1 of a shimmer cycle): a soft highlight that
    crosses the word during the first 60 % of the cycle and rests after it."""
    travel = t / 0.6
    centre = -0.6 + travel * 2.2
    base, high = _rgb(TEXT_BASE), _rgb(TEXT_HIGHLIGHT)
    colors = []
    for i in range(count):
        pos = (i + 0.5) / count
        glow = math.exp(-(((pos - centre) / 0.22) ** 2)) if travel <= 1.0 else 0.0
        colors.append(_hex(tuple(bc + (hc - bc) * glow for bc, hc in zip(base, high, strict=True))))
    return colors


class BrandMark(tk.Canvas):
    """The sidebar's cairn and name. Call `tick(now)` once per frame.

    `set_wordmark(False)` shows the cairn alone (the sidebar's rail mode). The mark follows
    the window's display scale: at a new scale its images and text are drawn again.
    """

    def __init__(self, master: tk.Misc, *, animate: bool | None = None) -> None:
        super().__init__(master, width=1, height=1, bg=theme.PAGE, highlightthickness=0, bd=0)
        self.animate = animations_enabled() if animate is None else animate
        self._wordmark = True
        self._draw(ctk.ScalingTracker.get_widget_scaling(master))
        ctk.ScalingTracker.add_widget(self._rescale, self)

    def _draw(self, scale: float) -> None:
        """Creates the images and canvas items for display scale `scale`."""
        self.delete("all")
        self._logo_px = max(16, round(LOGO_SIZE * scale))
        font = tkfont.Font(root=self, family=theme.FONT_FAMILY, size=-round(NAME_SIZE * scale), weight="bold")
        depth = len(EXTRUDE_COLORS)
        gap = round(10 * scale)
        text_w = font.measure(NAME) + depth + 2
        height = max(self._logo_px, font.metrics("linespace") + depth)
        self._full_width = self._logo_px + gap + text_w
        renderer = CairnRenderer(self._logo_px)
        self._renderer: CairnRenderer | None = renderer
        # Rendered poses, shown one at a time through one photo: every photo that has been on
        # screen holds a GDI bitmap for as long as it exists, so a photo per pose would hold
        # TURN_FRAMES of them.
        first = renderer.frame(0.0)
        self._frames: list[Image.Image | None] = [None] * TURN_FRAMES
        self._frames[0] = first
        self._shown = 0
        self._shimmer_step = -1
        self._photo: ImageTk.PhotoImage | None = ImageTk.PhotoImage(first, master=self)
        self._logo = self.create_image(0, height // 2, image=self._photo, anchor="w")
        x0 = self._logo_px + gap
        y = height // 2 - depth // 2
        # Extrusion layers from the back to the front, then one item per letter on top.
        for k, color in enumerate(EXTRUDE_COLORS):
            offset = depth - k
            self.create_text(
                x0 + offset, y + offset, text=NAME, font=font, fill=color, anchor="w", tags=("wordmark",)
            )
        self._letters: list[int] = []
        x = x0
        for letter in NAME:
            self._letters.append(
                self.create_text(x, y, text=letter, font=font, fill=TEXT_BASE, anchor="w", tags=("wordmark",))
            )
            x += font.measure(letter)
        self._font = font
        self.configure(height=height)
        self._apply_wordmark()

    def _rescale(self, widget_scaling: float, _window_scaling: float) -> None:
        self._draw(widget_scaling)

    def _apply_wordmark(self) -> None:
        self.itemconfigure("wordmark", state="normal" if self._wordmark else "hidden")
        self.configure(width=self._full_width if self._wordmark else self._logo_px)

    @property
    def wordmark(self) -> bool:
        """Whether the name shows next to the cairn."""
        return self._wordmark

    def set_wordmark(self, visible: bool) -> None:
        """Shows the name next to the cairn, or the cairn alone."""
        if visible != self._wordmark:
            self._wordmark = visible
            self._apply_wordmark()

    def destroy(self) -> None:
        ctk.ScalingTracker.remove_widget(self._rescale, self)
        super().destroy()
        # CustomTkinter's trackers keep every root, and so the closed window's widgets, for
        # the life of the process: the mark lets go of its poses, photo and renderer here.
        self._renderer = None
        self._photo = None
        self._frames = []

    @property
    def rendered_frames(self) -> int:
        return sum(1 for f in self._frames if f is not None)

    def tick(self, now: float) -> None:
        """Advances the animation to `now` (seconds, any monotonic clock); nothing once the
        mark is destroyed."""
        renderer, photo = self._renderer, self._photo
        if not self.animate or renderer is None or photo is None:
            return
        index = int((now % TURN_SECONDS) / TURN_SECONDS * TURN_FRAMES) % TURN_FRAMES
        if index != self._shown:
            pose = self._frames[index]
            if pose is None:
                # Poses are rendered on first use, so a tick renders at most one.
                pose = self._frames[index] = renderer.frame(index / TURN_FRAMES)
            photo.paste(pose)
            self._shown = index
        if not self._wordmark:
            return
        step = int((now % SHIMMER_SECONDS) / SHIMMER_SECONDS * 120)
        if step != self._shimmer_step:
            self._shimmer_step = step
            for item, color in zip(
                self._letters, shimmer_colors(len(self._letters), step / 120), strict=True
            ):
                self.itemconfigure(item, fill=color)
