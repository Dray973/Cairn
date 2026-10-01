"""Modal confirmation and result dialogs."""

from __future__ import annotations

import tkinter as tk
from collections.abc import Callable, Sequence

import customtkinter as ctk

from .. import theme
from .brand import set_window_icon


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


class MessageDialog(ctk.CTkToplevel):
    """Modal dialog with a heading, a message, an optional detail list and up to two buttons.

    `on_confirm` runs when the primary button is pressed; closing the window or pressing
    the secondary button runs `on_cancel`. `links` adds one button per (label, callback).
    With `acknowledge`, a checkbox with that text appears above the buttons and the primary
    button stays disabled until it is ticked. The heading is kept as `title_text`.
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        title: str,
        message: str,
        details: Sequence[str] = (),
        confirm_text: str = "OK",
        cancel_text: str | None = None,
        danger: bool = False,
        on_confirm: Callable[[], None] | None = None,
        on_cancel: Callable[[], None] | None = None,
        links: Sequence[tuple[str, Callable[[], None]]] = (),
        acknowledge: str | None = None,
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE)
        # The icon's photos, which Tk frees with the window; also stops CustomTkinter from
        # replacing the icon with its own a moment later.
        set_window_icon(self)
        self.title(title)
        self.title_text = title
        self.resizable(False, False)
        self._on_confirm = on_confirm
        self._on_cancel = on_cancel
        self._closed = False
        self.grid_columnconfigure(0, weight=1)

        ctk.CTkLabel(self, text=title, font=_font(16, "bold"), text_color=theme.INK, anchor="w").grid(
            row=0, column=0, sticky="w", padx=20, pady=(18, 4)
        )
        ctk.CTkLabel(
            self,
            text=message,
            font=_font(12),
            text_color=theme.INK_SECONDARY,
            anchor="w",
            justify="left",
            wraplength=520,
        ).grid(row=1, column=0, sticky="w", padx=20)

        row = 2
        if details:
            box = ctk.CTkTextbox(
                self,
                width=520,
                height=min(260, 22 + 18 * len(details)),
                font=_font(11),
                fg_color=theme.PAGE,
                text_color=theme.INK_SECONDARY,
                border_width=1,
                border_color=theme.BORDER,
                wrap="word",
            )
            box.insert("1.0", "\n".join(details))
            box.configure(state="disabled")
            box.grid(row=row, column=0, sticky="ew", padx=20, pady=(10, 0))
            row += 1

        for label, callback in links:
            ctk.CTkButton(
                self,
                text=label,
                font=_font(11),
                height=26,
                fg_color=theme.BUTTON_NEUTRAL,
                hover_color=theme.BUTTON_NEUTRAL_HOVER,
                command=callback,
            ).grid(row=row, column=0, sticky="w", padx=20, pady=(6, 0))
            row += 1

        self.acknowledge_box: ctk.CTkCheckBox | None = None
        if acknowledge:
            self.acknowledge_box = ctk.CTkCheckBox(
                self,
                text=acknowledge,
                font=_font(11),
                text_color=theme.INK_SECONDARY,
                command=self._acknowledged,
            )
            self.acknowledge_box.grid(row=row, column=0, sticky="w", padx=20, pady=(12, 0))
            row += 1

        buttons = ctk.CTkFrame(self, fg_color="transparent")
        buttons.grid(row=row, column=0, sticky="e", padx=20, pady=18)
        if cancel_text:
            ctk.CTkButton(
                buttons,
                text=cancel_text,
                width=100,
                fg_color=theme.BUTTON_NEUTRAL,
                hover_color=theme.BUTTON_NEUTRAL_HOVER,
                font=_font(12),
                command=self._cancel,
            ).pack(side="left", padx=(0, 8))
        self.confirm_button = ctk.CTkButton(
            buttons,
            text=confirm_text,
            width=140,
            font=_font(12, "bold"),
            fg_color=theme.CRITICAL if danger else theme.ACCENT,
            hover_color=theme.CRITICAL_HOVER if danger else theme.ACCENT_HOVER,
            state="disabled" if self.acknowledge_box is not None else "normal",
            command=self._confirm,
        )
        self.confirm_button.pack(side="left")

        self.protocol("WM_DELETE_WINDOW", self._cancel)
        self.bind("<Escape>", lambda _e: self._cancel())
        self.transient(master)
        self.after(10, self._grab)

    def _grab(self) -> None:
        try:
            self.lift()
            self.focus_force()
            self.grab_set()
        except tk.TclError:
            pass

    def _close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            self.grab_release()
        except tk.TclError:
            pass
        self.destroy()

    @property
    def _acknowledgement_missing(self) -> bool:
        return self.acknowledge_box is not None and self.acknowledge_box.get() != 1

    def _acknowledged(self) -> None:
        self.confirm_button.configure(state="disabled" if self._acknowledgement_missing else "normal")

    def _confirm(self) -> None:
        if self._acknowledgement_missing:
            return
        self._close()
        if self._on_confirm:
            self._on_confirm()

    def _cancel(self) -> None:
        self._close()
        if self._on_cancel:
            self._on_cancel()
