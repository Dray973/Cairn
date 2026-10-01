"""Startup section: every listed startup entry, with an on/off switch where it can be changed."""

from __future__ import annotations

import tkinter as tk
from collections.abc import Callable
from typing import Any

import customtkinter as ctk

from .. import theme


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def _shorten(text: str, limit: int = 110) -> str:
    return text if len(text) <= limit else text[: limit - 1] + "…"


def can_change(entry: dict[str, Any], engine_ready: bool, elevated: bool) -> bool:
    """Whether this process may switch the entry.

    Per-user entries live in HKCU and the user's Startup folder, so they need no elevation;
    machine-wide entries (`requires_admin`) do. Entries the engine reports as not
    toggleable never can.
    """
    if not engine_ready or not entry.get("can_toggle", True):
        return False
    return elevated or not entry.get("requires_admin", True)


class StartupRow(ctk.CTkFrame):
    def __init__(
        self,
        master: tk.Misc,
        entry: dict[str, Any],
        engine_ready: bool,
        elevated: bool,
        on_toggle: Callable[[dict[str, Any], bool], None],
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.entry = entry
        self._on_toggle = on_toggle
        self.switch: ctk.CTkSwitch | None = None
        self.hint_label: ctk.CTkLabel | None = None
        self.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(self, text=entry["name"], font=_font(12, "bold"), text_color=theme.INK, anchor="w").grid(
            row=0, column=0, sticky="w", padx=12, pady=(8, 0)
        )
        meta = [entry["publisher"] or "Unknown publisher", entry["location"]]
        ctk.CTkLabel(
            self, text="  ·  ".join(meta), font=_font(10), text_color=theme.INK_SECONDARY, anchor="w"
        ).grid(row=1, column=0, sticky="w", padx=12)
        toggleable = entry.get("can_toggle", True)
        if not toggleable:
            # The engine's note says why, for example Group Policy or another account.
            hint = entry.get("note") or "This entry cannot be switched on or off from here."
        elif engine_ready and entry.get("requires_admin", True) and not elevated:
            hint = "Machine-wide entry: changing it needs administrator rights."
        else:
            hint = None
        lines: list[tuple[str, str]] = [(_shorten(entry["command"] or entry["path"]), theme.INK_MUTED)]
        if not entry["exists"] and entry["path"]:
            lines.append(("⚠ The program this entry starts no longer exists", theme.WARNING))
        if hint is not None:
            lines.append((hint, theme.INK_MUTED))
        for offset, (text, color) in enumerate(lines):
            label = ctk.CTkLabel(
                self, text=text, font=_font(10), text_color=color, anchor="w", justify="left", wraplength=640
            )
            last = offset == len(lines) - 1
            label.grid(row=2 + offset, column=0, sticky="w", padx=12, pady=(0, 8 if last else 0))
            if hint is not None and last:
                self.hint_label = label
        self.state_label = ctk.CTkLabel(
            self,
            text="Enabled" if entry["enabled"] else "Disabled",
            font=_font(11),
            text_color=theme.GOOD if entry["enabled"] else theme.INK_MUTED,
            width=70,
        )
        self.state_label.grid(row=0, column=1, rowspan=2, sticky="e", padx=(0, 0 if toggleable else 10))
        if not toggleable:
            return
        self.switch = ctk.CTkSwitch(
            self,
            text="",
            width=46,
            switch_width=42,
            switch_height=22,
            progress_color=theme.ACCENT,
            button_color=theme.INK,
            button_hover_color=theme.INK_SECONDARY,
            fg_color=theme.BASELINE,
            state="normal" if can_change(entry, engine_ready, elevated) else "disabled",
            command=self._flipped,
        )
        if entry["enabled"]:
            self.switch.select()
        self.switch.grid(row=0, column=2, rowspan=2, sticky="e", padx=(4, 10))

    def _flipped(self) -> None:
        if self.switch is None:
            return
        desired = bool(self.switch.get())
        # The switch follows the system: it moves back until the change is confirmed.
        if self.entry["enabled"]:
            self.switch.select()
        else:
            self.switch.deselect()
        self._on_toggle(self.entry, desired)

    def set_busy(self) -> None:
        if self.switch is not None:
            self.switch.configure(state="disabled")
        self.state_label.configure(text="…", text_color=theme.INK_SECONDARY)


class StartupPanel(ctk.CTkFrame):
    def __init__(
        self,
        master: tk.Misc,
        on_refresh: Callable[[], None],
        on_toggle: Callable[[dict[str, Any], bool], None],
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_toggle = on_toggle
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(2, weight=1)
        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            header, text="Startup apps", font=_font(13, "bold"), text_color=theme.INK_SECONDARY
        ).grid(row=0, column=0, sticky="w")
        self.refresh_button = ctk.CTkButton(
            header,
            text="Refresh",
            width=100,
            height=30,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_refresh,
        )
        self.refresh_button.grid(row=0, column=1, sticky="e")
        self.summary = ctk.CTkLabel(
            self,
            text="Programs that start when you sign in. Turning one off keeps it installed; it just "
            "no longer starts automatically.",
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
        )
        self.summary.grid(row=1, column=0, sticky="ew", padx=14, pady=(2, 6))
        self.list = ctk.CTkScrollableFrame(
            self,
            fg_color=theme.SURFACE,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.list.grid(row=2, column=0, sticky="nsew", padx=6, pady=(0, 8))
        self.list.grid_columnconfigure(0, weight=1)
        self._rows: list[StartupRow] = []
        self.loaded = False

    @property
    def row_count(self) -> int:
        return len(self._rows)

    def set_loading(self) -> None:
        self.refresh_button.configure(state="disabled")

    def show_error(self, message: str) -> None:
        self.refresh_button.configure(state="normal")
        self.summary.configure(text=f"Could not read startup apps: {message}", text_color=theme.CRITICAL)

    def show(self, entries: list[dict[str, Any]], engine_ready: bool, elevated: bool) -> None:
        """Lists `entries`; each switch is enabled when `can_change` allows it."""
        self.loaded = True
        self.refresh_button.configure(state="normal")
        for row in self._rows:
            row.destroy()
        self._rows.clear()
        enabled = sum(1 for e in entries if e["enabled"])
        self.summary.configure(
            text=f"{len(entries)} startup apps  ·  {enabled} enabled  ·  {len(entries) - enabled} disabled. "
            "Turning one off keeps it installed; it just no longer starts at sign-in.",
            text_color=theme.INK_MUTED,
        )
        for i, entry in enumerate(entries):
            row = StartupRow(self.list, entry, engine_ready, elevated, self._toggled)
            row.grid(row=i, column=0, sticky="ew", padx=6, pady=4)
            self._rows.append(row)

    def _toggled(self, entry: dict[str, Any], enabled: bool) -> None:
        for row in self._rows:
            if row.entry["id"] == entry["id"]:
                row.set_busy()
        self._on_toggle(entry, enabled)
