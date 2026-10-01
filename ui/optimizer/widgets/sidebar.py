"""The window's grouped sidebar: the brand, one row per section and the About row.

Two modes: wide (glyph and name) and rail (glyph only), chosen by the window from its width
(`sidebar_mode`). Four densities follow the sidebar's own height (`choose_density`), so every
section fits without scrolling at the smallest window. The pure helpers at the top load
without a window.

Rows are plain CustomTkinter frames whose labels forward clicks and hovering to the row;
hidden parts are removed with `grid_forget`, never `grid_remove`. In rail mode the brand shows
the cairn alone and hovering a row shows its name in a tooltip: a label placed in the window
next to the sidebar (no extra top-level window).
"""

from __future__ import annotations

import tkinter as tk
import tkinter.font as tkfont
from collections.abc import Callable, Sequence
from typing import Any, NamedTuple

import customtkinter as ctk

from .. import APP_NAME, theme
from ..sections import ABOUT_GLYPH, Section, grouped
from .brand import BrandMark

# Logical pixels.
WIDE_WIDTH = 188
RAIL_WIDTH = 60
RAIL_BELOW = 1080
# The first of these families Tk knows draws the glyphs; without either, rows show a letter.
ICON_FAMILIES = ("Segoe Fluent Icons", "Segoe MDL2 Assets")


class Density(NamedTuple):
    name: str
    item: int  # row height
    gap: int  # space below each row
    header: int  # group heading height; 0 hides the headings
    group_gap: int  # space between groups while the headings are hidden
    font: int  # name size
    glyph: int  # glyph size


DENSITIES = (
    Density("regular", 32, 2, 26, 0, 13, 15),
    Density("compact", 28, 1, 20, 0, 13, 14),
    Density("dense", 25, 1, 0, 8, 12, 13),
    Density("tiny", 21, 1, 0, 4, 11, 12),
)
# Space the brand (46 px mark plus padding) and the footer (rule plus About row) take.
BRAND_BLOCK = 66
FOOTER_BLOCK = 40
# In rail mode groups are divided by a 1 px rule with 6 px above and below it.
RAIL_GROUP_GAP = 13
# The About row keeps one size whatever the density.
FOOTER_DENSITY = Density("footer", 28, 0, 0, 0, 12, 14)
# Space between the rail and its tooltip.
TOOLTIP_GAP = 6
# The brand's padding in wide mode (physical pixels, as the brand is a plain Tk canvas).
BRAND_PADX = (14, 0)
BRAND_PADY = (12, 8)


def sidebar_mode(window_width: float) -> str:
    """Mode for a window `window_width` logical pixels wide: "rail" below RAIL_BELOW, else "wide"."""
    return "rail" if window_width < RAIL_BELOW else "wide"


def nav_height(d: Density, items: int, groups: int, rail: bool) -> int:
    """Height the section rows need at density `d`, headings or group gaps included."""
    if d.header and not rail:
        extra = groups * d.header
    else:
        extra = max(groups - 1, 0) * max(d.group_gap, RAIL_GROUP_GAP if rail else 0)
    return items * (d.item + d.gap) + extra


def choose_density(available: float, items: int, groups: int, rail: bool) -> Density:
    """The first density whose rows fit into `available` logical pixels, else the smallest."""
    for density in DENSITIES:
        if nav_height(density, items, groups, rail) <= available:
            return density
    return DENSITIES[-1]


def badge_fit(available: int, long_px: int, short_px: int) -> str:
    """Which text of a top-bar badge fits into `available` pixels: "long", "short" or "none"."""
    if long_px <= available:
        return "long"
    if short_px <= available:
        return "short"
    return "none"


def _font(size: int, weight: str = "normal", family: str = theme.FONT_FAMILY) -> ctk.CTkFont:
    return ctk.CTkFont(family=family, size=size, weight=weight)


class NavItem(ctk.CTkFrame):
    """One sidebar row: a 3 px accent bar, the glyph, the name and a badge.

    The whole row is clickable. In rail mode only the glyph shows, and a badge becomes a dot
    at the glyph's top right (unless `rail_dot` is False).
    """

    def __init__(
        self,
        master: Any,
        *,
        name: str,
        glyph: str,
        icon_family: str | None,
        on_click: Callable[[str], None],
        on_hover: Callable[[NavItem, bool], None],
        badge_size: int = 10,
        badge_weight: str = "bold",
        rail_dot: bool = True,
    ) -> None:
        super().__init__(master, fg_color=theme.PAGE, corner_radius=6, height=DENSITIES[0].item)
        self.name = name
        self._on_click = on_click
        self._on_hover = on_hover
        self._rail_dot = rail_dot
        self._selected = False
        self._hover = False
        self._rail: bool | None = None
        self._density: Density | None = None
        self._badge_text = ""
        self._colors: tuple[str, str, str] | None = None
        self.grid_propagate(False)
        self.grid_rowconfigure(0, weight=1)

        self._glyph_font = _font(DENSITIES[0].glyph, family=icon_family or theme.FONT_FAMILY)
        self._name_font = _font(DENSITIES[0].font)
        self.bar = ctk.CTkFrame(self, width=3, height=16, corner_radius=1, fg_color="transparent")
        self.glyph_label = ctk.CTkLabel(
            self,
            text=glyph if icon_family else name[:1],
            width=24,
            font=self._glyph_font if icon_family else _font(DENSITIES[0].glyph, "bold"),
            text_color=theme.INK_MUTED,
        )
        self.name_label = ctk.CTkLabel(
            self, text=name, font=self._name_font, text_color=theme.INK_SECONDARY, anchor="w"
        )
        self.badge_label = ctk.CTkLabel(
            self, text="", font=_font(badge_size, badge_weight), text_color=theme.INK_MUTED
        )
        self.dot = ctk.CTkFrame(self, width=6, height=6, corner_radius=3, fg_color=theme.INK_MUTED)
        for widget in (self, self.bar, self.glyph_label, self.name_label, self.badge_label):
            widget.bind("<Button-1>", self._clicked, add="+")
            widget.bind("<Enter>", self._entered, add="+")
            widget.bind("<Leave>", self._left, add="+")
        self.set_layout(DENSITIES[0], rail=False)

    # -- state ---------------------------------------------------------------------

    def invoke(self) -> None:
        """Same as a click on the row."""
        self._on_click(self.name)

    def set_selected(self, selected: bool) -> None:
        if selected != self._selected:
            self._selected = selected
            self._paint()

    @property
    def selected(self) -> bool:
        return self._selected

    @property
    def badge(self) -> str:
        return self._badge_text

    def set_badge(self, text: str, color: str) -> None:
        """Shows `text` in `color` at the row's right (a dot in rail mode); "" hides it."""
        self._badge_text = text
        self.badge_label.configure(text=text, text_color=color)
        self.dot.configure(fg_color=color)
        self._place_badge()

    def set_layout(self, density: Density, rail: bool) -> None:
        """Sizes the row for `density` and shows the parts that `rail` mode keeps."""
        if density != self._density:
            self._density = density
            self.configure(height=density.item)
            self._glyph_font.configure(size=density.glyph)
            self._name_font.configure(size=density.font)
            label_height = max(density.item - 4, 12)
            for label in (self.glyph_label, self.name_label, self.badge_label):
                label.configure(height=label_height)
            self.bar.configure(height=max(density.item - 14, 8))
        if rail != self._rail:
            self._rail = rail
            self.bar.grid(row=0, column=0, sticky="w", padx=(2, 0))
            if rail:
                self.grid_columnconfigure(1, weight=1)
                self.grid_columnconfigure(2, weight=0)
                self.glyph_label.grid(row=0, column=1)
                self.name_label.grid_forget()
            else:
                self.grid_columnconfigure(1, weight=0)
                self.grid_columnconfigure(2, weight=1)
                self.glyph_label.grid(row=0, column=1, padx=(6, 8))
                self.name_label.grid(row=0, column=2, sticky="ew")
        self._place_badge()

    def _place_badge(self) -> None:
        if self._rail or not self._badge_text:
            self.badge_label.grid_forget()
        else:
            self.badge_label.grid(row=0, column=3, sticky="e", padx=(4, 10))
        if self._rail and self._badge_text and self._rail_dot:
            self.dot.place(relx=0.5, rely=0.5, x=8, y=-8, anchor="center")
        else:
            self.dot.place_forget()

    def _paint(self) -> None:
        if self._selected:
            colors = (theme.SURFACE_RAISED, theme.ACCENT, theme.INK)
        else:
            colors = (theme.SURFACE if self._hover else theme.PAGE, theme.INK_MUTED, theme.INK_SECONDARY)
        if colors == self._colors:
            return
        self._colors = colors
        fill, glyph, name = colors
        self.configure(fg_color=fill)
        self.bar.configure(fg_color=theme.ACCENT if self._selected else "transparent")
        self.glyph_label.configure(text_color=glyph)
        self.name_label.configure(text_color=name)

    # -- events --------------------------------------------------------------------

    def _clicked(self, _event: Any = None) -> None:
        self.invoke()

    def _entered(self, _event: Any = None) -> None:
        if not self._hover:
            self._hover = True
            self._paint()
            self._on_hover(self, True)

    def _left(self, _event: Any = None) -> None:
        # Moving between the row and its labels leaves one widget and enters another.
        try:
            inside = self.winfo_containing(*self.winfo_pointerxy())
        except (tk.TclError, KeyError):
            inside = None
        if inside is not None and str(inside).startswith(str(self)):
            return
        if self._hover:
            self._hover = False
            self._paint()
            self._on_hover(self, False)


class Sidebar(ctk.CTkFrame):
    """The brand, the sections grouped under headings, and "About Cairn" with the version.

    `select` changes only the visual state; a click calls `on_select(name)`, and the About
    row calls `on_about()`. The window sets the mode (`set_mode`); the density follows the
    sidebar's height. Below the smallest density the footer is hidden and the list is cut
    at the bottom (every section stays reachable with Ctrl+Tab).
    """

    def __init__(
        self,
        master: Any,
        *,
        sections: Sequence[Section],
        version: str,
        on_select: Callable[[str], None],
        on_about: Callable[[], None],
    ) -> None:
        super().__init__(master, fg_color=theme.PAGE, corner_radius=0, width=WIDE_WIDTH)
        self.grid_propagate(False)
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(1, weight=1)
        self._on_select = on_select
        self._groups = grouped(list(sections))
        self._version = version
        self._height: float | None = None
        self._footer_shown = True
        self._selected: str | None = None
        self.mode = "wide"
        self.density = DENSITIES[0]
        family = self._icon_family()

        self.brand = BrandMark(self)
        self._grid_brand()

        self._nav = ctk.CTkFrame(self, fg_color="transparent", corner_radius=0)
        self._nav.grid(row=1, column=0, sticky="new")
        self._nav.grid_columnconfigure(0, weight=1)
        self.items: dict[str, NavItem] = {}
        self._headings: list[tuple[ctk.CTkLabel, ctk.CTkFrame]] = []
        for group, members in self._groups:
            heading = ctk.CTkLabel(
                self._nav,
                text=group.upper(),
                font=_font(10, "bold"),
                text_color=theme.INK_MUTED,
                anchor="w",
                height=DENSITIES[0].header,
            )
            rule = ctk.CTkFrame(self._nav, height=1, fg_color=theme.BORDER, corner_radius=0)
            self._headings.append((heading, rule))
            for s in members:
                self.items[s.name] = NavItem(
                    self._nav,
                    name=s.name,
                    glyph=s.glyph,
                    icon_family=family,
                    on_click=self._clicked,
                    on_hover=self._hovered,
                )

        self._footer = ctk.CTkFrame(self, fg_color="transparent", corner_radius=0)
        self._footer.grid(row=2, column=0, sticky="sew", pady=(0, 6))
        self._footer.grid_columnconfigure(0, weight=1)
        ctk.CTkFrame(self._footer, height=1, fg_color=theme.BORDER, corner_radius=0).grid(
            row=0, column=0, sticky="ew", padx=12, pady=(0, 5)
        )
        self.about_item = NavItem(
            self._footer,
            name=f"About {APP_NAME}",
            glyph=ABOUT_GLYPH,
            icon_family=family,
            on_click=self._about_clicked,
            on_hover=self._hovered,
            badge_size=11,
            badge_weight="normal",
            rail_dot=False,
        )
        self.about_item.grid(row=1, column=0, sticky="ew", padx=8)
        self._on_about = on_about
        self.set_version_warning(False)

        # The rail's tooltip lives in the window, so it can extend over the section beside it.
        self.tooltip = ctk.CTkLabel(
            self.winfo_toplevel(),
            text="",
            font=_font(12),
            fg_color=theme.SURFACE_RAISED,
            text_color=theme.INK,
            corner_radius=6,
            padx=8,
        )

        divider = ctk.CTkFrame(self, width=1, fg_color=theme.BORDER, corner_radius=0)
        divider.place(relx=1.0, x=-1, rely=0, relheight=1)
        tk.Frame.bind(self, "<Configure>", self._configured, add="+")
        self._layout()

    # -- public API ----------------------------------------------------------------

    def select(self, name: str) -> None:
        """Marks `name` as the shown section; does not call `on_select`."""
        if self._selected is not None and self._selected in self.items:
            self.items[self._selected].set_selected(False)
        self._selected = name
        if name in self.items:
            self.items[name].set_selected(True)

    def set_badge(self, name: str, text: str = "", color: str = theme.INK_MUTED) -> None:
        """Shows `text` next to section `name`; "" removes the badge."""
        item = self.items[name]
        if item.badge != text or text:
            item.set_badge(text, color)

    def set_version_warning(self, warning: bool) -> None:
        """Shows the version in the About row, as a warning when the engine's differs."""
        if warning:
            self.about_item.set_badge(f"⚠ v{self._version}", theme.WARNING)
        else:
            self.about_item.set_badge(f"v{self._version}", theme.INK_MUTED)

    def set_mode(self, mode: str) -> None:
        """Switches to `mode`: "wide" (glyphs and names) or "rail" (glyphs only)."""
        if mode == self.mode:
            return
        self.mode = mode
        self.hide_tooltip()
        self.brand.set_wordmark(mode != "rail")
        self._grid_brand()
        self.configure(width=RAIL_WIDTH if mode == "rail" else WIDE_WIDTH)
        if not self._fit():
            self._layout()

    @property
    def footer_shown(self) -> bool:
        return self._footer_shown

    @property
    def tooltip_text(self) -> str | None:
        """The name the rail tooltip shows, or None while it is hidden."""
        if not self.tooltip.winfo_manager():
            return None
        return str(self.tooltip.cget("text"))

    def hide_tooltip(self) -> None:
        self.tooltip.place_forget()

    # -- layout --------------------------------------------------------------------

    def _grid_brand(self) -> None:
        """The brand at the top: left-aligned with its name in wide mode, the cairn centred in
        the rail."""
        if self.mode == "rail":
            scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
            pad = max(0, (round(RAIL_WIDTH * scale) - int(self.brand.cget("width"))) // 2)
            padx: tuple[int, int] = (pad, 0)
        else:
            padx = BRAND_PADX
        self.brand.grid(row=0, column=0, sticky="w", padx=padx, pady=BRAND_PADY)

    def _icon_family(self) -> str | None:
        try:
            known = set(tkfont.families(self))
        except tk.TclError:
            return None
        return next((f for f in ICON_FAMILIES if f in known), None)

    def _configured(self, event: tk.Event) -> None:
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        self._height = event.height / scale
        self._fit()

    def _fit(self) -> bool:
        """Chooses the density for the known height; True when it laid the rows out again."""
        if self._height is None:
            return False
        rail = self.mode == "rail"
        items, groups = len(self.items), len(self._groups)
        available = self._height - BRAND_BLOCK - FOOTER_BLOCK
        footer = nav_height(DENSITIES[-1], items, groups, rail) <= available
        if not footer:
            available += FOOTER_BLOCK
        if footer != self._footer_shown:
            self._footer_shown = footer
            if footer:
                self._footer.grid(row=2, column=0, sticky="sew", pady=(0, 6))
            else:
                self._footer.grid_forget()
        density = choose_density(available, items, groups, rail)
        if density == self.density:
            return False
        self.density = density
        self._layout()
        return True

    def _layout(self) -> None:
        rail = self.mode == "rail"
        d = self.density
        row = 0
        for index, ((_group, members), (heading, rule)) in enumerate(
            zip(self._groups, self._headings, strict=True)
        ):
            heading.grid_forget()
            rule.grid_forget()
            top = 0
            if rail:
                if index:
                    rule.grid(row=row, column=0, sticky="ew", padx=14, pady=6)
                    row += 1
            elif d.header:
                heading.configure(height=d.header)
                heading.grid(row=row, column=0, sticky="ew", padx=(20, 0))
                row += 1
            elif index:
                top = d.group_gap
            for position, s in enumerate(members):
                item = self.items[s.name]
                item.set_layout(d, rail)
                item.grid(row=row, column=0, sticky="ew", padx=8, pady=(top if position == 0 else 0, d.gap))
                row += 1
        self.about_item.set_layout(FOOTER_DENSITY, rail)

    # -- events --------------------------------------------------------------------

    def _clicked(self, name: str) -> None:
        self.hide_tooltip()
        self._on_select(name)

    def _about_clicked(self, _name: str) -> None:
        self.hide_tooltip()
        self._on_about()

    def _hovered(self, item: NavItem, inside: bool) -> None:
        """Shows the row's name next to the rail while the pointer is on the row."""
        if inside and self.mode == "rail" and item.winfo_ismapped():
            self._show_tooltip(item)
        else:
            self.hide_tooltip()

    def _show_tooltip(self, item: NavItem) -> None:
        window = self.winfo_toplevel()
        scale = ctk.ScalingTracker.get_widget_scaling(self) or 1.0
        # Positions relative to the window, in logical pixels (the label scales them).
        x = (self.winfo_rootx() - window.winfo_rootx() + self.winfo_width()) / scale + TOOLTIP_GAP
        y = (item.winfo_rooty() - window.winfo_rooty() + item.winfo_height() / 2) / scale
        self.tooltip.configure(text=item.name)
        self.tooltip.place(x=x, y=y, anchor="w")
        self.tooltip.lift()
