"""Cleanup section: per-location sizes, selection and the clean action."""

from __future__ import annotations

import tkinter as tk
from collections.abc import Callable
from typing import Any

import customtkinter as ctk

from .. import theme


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def fmt_size(value: int) -> str:
    step = 1024.0
    size = float(value)
    for unit in ("B", "KB", "MB", "GB"):
        if size < step or unit == "GB":
            return f"{size:.0f} {unit}" if unit in ("B", "KB") else f"{size:.1f} {unit}"
        size /= step
    return f"{size:.1f} GB"


#: Most locations listed under a cleanup row; the rest are summarised as a count.
MAX_LISTED_PATHS = 4


def paths_text(paths: list[str]) -> str:
    """The locations a target works on, one per line, with any overflow as a count."""
    lines = paths[:MAX_LISTED_PATHS]
    extra = len(paths) - len(lines)
    if extra > 0:
        lines.append(f"and {extra} more location{'s' if extra != 1 else ''}")
    return "\n".join(lines)


class CleanupRow(ctk.CTkFrame):
    def __init__(
        self,
        master: tk.Misc,
        target: dict[str, Any],
        selectable: bool,
        blocked: str | None,
        on_change: Callable[[], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self.target = target
        self.grid_columnconfigure(0, weight=1)
        self.var = tk.BooleanVar(value=selectable and bool(target["default_on"]) and target["bytes"] > 0)
        self.checkbox = ctk.CTkCheckBox(
            self,
            text=target["title"],
            variable=self.var,
            font=_font(12, "bold"),
            text_color=theme.INK,
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            border_color=theme.INK_MUTED,
            state="normal" if selectable else "disabled",
            command=on_change,
        )
        self.checkbox.grid(row=0, column=0, sticky="w")
        ctk.CTkLabel(
            self,
            text=fmt_size(target["bytes"]),
            font=_font(12, "bold"),
            text_color=theme.INK if target["bytes"] else theme.INK_MUTED,
            anchor="e",
            width=90,
        ).grid(row=0, column=1, sticky="e", padx=(6, 8))
        detail = target["description"]
        if target.get("files"):
            detail += f"  ·  {target['files']:,} files"
        ctk.CTkLabel(
            self,
            text=detail,
            font=_font(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=760,
        ).grid(row=1, column=0, columnspan=2, sticky="w", padx=(28, 0))
        next_row = 2
        if target.get("recent_files_kept"):
            ctk.CTkLabel(
                self,
                text="Anything created or changed in the last 24 hours is kept.",
                font=_font(10),
                text_color=theme.INK_SECONDARY,
                anchor="w",
            ).grid(row=next_row, column=0, columnspan=2, sticky="w", padx=(28, 0))
            next_row += 1
        paths = [str(p) for p in target.get("paths") or []]
        if paths:
            ctk.CTkLabel(
                self,
                text=paths_text(paths),
                font=_font(9),
                text_color=theme.INK_MUTED,
                anchor="w",
                justify="left",
                wraplength=760,
            ).grid(row=next_row, column=0, columnspan=2, sticky="w", padx=(28, 0))
            next_row += 1
        if blocked:
            ctk.CTkLabel(
                self,
                text=f"⚠ {blocked}",
                font=_font(10),
                text_color=theme.WARNING,
                anchor="w",
                justify="left",
                wraplength=760,
            ).grid(row=next_row, column=0, columnspan=2, sticky="w", padx=(28, 0))

    @property
    def selected(self) -> bool:
        return bool(self.var.get()) and self.checkbox.cget("state") == "normal"


class CleanupPanel(ctk.CTkFrame):
    """Sizes of every cleanup location with checkboxes, a running total and a clean button."""

    def __init__(
        self,
        master: tk.Misc,
        on_scan: Callable[[], None],
        on_clean: Callable[[list[dict[str, Any]]], None],
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_clean = on_clean
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(2, weight=1)
        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            header, text="Disk cleanup", font=_font(13, "bold"), text_color=theme.INK_SECONDARY
        ).grid(row=0, column=0, sticky="w")
        self.scan_button = ctk.CTkButton(
            header,
            text="Scan",
            width=100,
            height=30,
            font=_font(12, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            command=on_scan,
        )
        self.scan_button.grid(row=0, column=1, sticky="e")
        self.summary = ctk.CTkLabel(
            self,
            text="Scan to measure temporary files, caches and crash dumps.",
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
        )
        self.summary.grid(row=1, column=0, sticky="ew", padx=14, pady=(2, 4))
        self.list = ctk.CTkScrollableFrame(
            self,
            fg_color=theme.SURFACE,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.list.grid(row=2, column=0, sticky="nsew", padx=6)
        self.list.grid_columnconfigure(0, weight=1)

        footer = ctk.CTkFrame(self, fg_color="transparent")
        footer.grid(row=3, column=0, sticky="ew", padx=14, pady=10)
        footer.grid_columnconfigure(0, weight=1)
        self.selection = ctk.CTkLabel(
            footer, text="", font=_font(12), text_color=theme.INK_SECONDARY, anchor="w"
        )
        self.selection.grid(row=0, column=0, sticky="w")
        self.clean_button = ctk.CTkButton(
            footer,
            text="Clean selected",
            width=160,
            height=34,
            font=_font(13, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            state="disabled",
            command=self._clean,
        )
        self.clean_button.grid(row=0, column=1, sticky="e")
        ctk.CTkLabel(
            footer,
            text="Deleted files cannot be restored. Files in use are always skipped; recently "
            "changed files are kept only where a location says so.",
            font=_font(10),
            text_color=theme.INK_MUTED,
        ).grid(row=1, column=0, columnspan=2, sticky="w", pady=(4, 0))
        self._rows: list[CleanupRow] = []
        self._mutations_enabled = False
        self.scanned = False

    @property
    def row_count(self) -> int:
        return len(self._rows)

    def set_scanning(self) -> None:
        self.scan_button.configure(state="disabled", text="Scanning…")
        self.clean_button.configure(state="disabled")
        self.summary.configure(text="Measuring…", text_color=theme.INK_MUTED)

    def set_busy(self, text: str) -> None:
        self.scan_button.configure(state="disabled")
        self.clean_button.configure(state="disabled")
        self.summary.configure(text=text, text_color=theme.INK_SECONDARY)

    def set_idle(self) -> None:
        self.scan_button.configure(state="normal", text="Scan")
        self._update_selection()

    def show_error(self, message: str) -> None:
        self.set_idle()
        self.summary.configure(text=f"Scan failed: {message}", text_color=theme.CRITICAL)

    def show(self, scan: dict[str, Any], elevated: bool, mutations_enabled: bool) -> None:
        self.scanned = True
        self._mutations_enabled = mutations_enabled
        for row in self._rows:
            row.destroy()
        self._rows.clear()
        for i, target in enumerate(scan["targets"]):
            blocked = target.get("blocked_reason")
            if target["requires_admin"] and not elevated and not blocked:
                blocked = "needs administrator rights"
            selectable = mutations_enabled and not blocked and target["bytes"] > 0
            row = CleanupRow(self.list, target, selectable, blocked, self._update_selection)
            row.grid(row=i, column=0, sticky="ew", padx=6, pady=5)
            self._rows.append(row)
        self.summary.configure(
            text=f"{fmt_size(scan['total_bytes'])} found in {len(scan['targets'])} locations  ·  "
            f"{scan['duration_ms']} ms",
            text_color=theme.INK_MUTED,
        )
        self.set_idle()

    def selected_targets(self) -> list[dict[str, Any]]:
        return [row.target for row in self._rows if row.selected]

    def _update_selection(self) -> None:
        chosen = self.selected_targets()
        total = sum(t["bytes"] for t in chosen)
        if not self._rows:
            self.selection.configure(text="")
        else:
            self.selection.configure(text=f"Selected: {fmt_size(total)} in {len(chosen)} locations")
        state = "normal" if self._mutations_enabled and chosen else "disabled"
        self.clean_button.configure(state=state)

    def _clean(self) -> None:
        chosen = self.selected_targets()
        if chosen:
            self._on_clean(chosen)
