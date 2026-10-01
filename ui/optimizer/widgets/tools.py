"""Tools section: Windows' repair and disk tools with their live output, quick actions and a
launcher for the built-in Windows tools.

The pure helpers at the top format durations, drives and job states and decide why a tool
cannot run. `OutputBuffer` holds output lines until the panel inserts them into the output
box, at most `FLUSH_LINES` per call, so a burst of output never stalls the frame loop.
"""

from __future__ import annotations

import tkinter as tk
from collections import deque
from collections.abc import Callable, Iterable, Mapping, Sequence
from typing import Any

import customtkinter as ctk

from .. import theme
from .cleanup import fmt_size
from .monitor import set_text

# Output lines kept in the output box; older lines stay in the log file.
MAX_OUTPUT_LINES = 2000
# Most output lines inserted into the output box per call of `ToolsPanel.flush`.
FLUSH_LINES = 200
# Silence after which the running tool gets a note that it is still working.
IDLE_HINT_MS = 300000

# Job state -> (badge, colour).
STATE_STYLE: dict[str, tuple[str, str]] = {
    "running": ("◐ Running", theme.INK_SECONDARY),
    "succeeded": ("✓ Finished", theme.GOOD),
    "completed": ("– Finished: see the result", theme.INK_SECONDARY),
    "attention": ("⚠ Needs attention", theme.WARNING),
    "failed": ("⚠ Failed", theme.CRITICAL),
    "cancelled": ("○ Stopped", theme.INK_MUTED),
}

# Drive tool id -> the volume field that says why the tool cannot run on that volume.
VOLUME_BLOCK_KEYS = {
    "drive_optimize": "optimize_blocked",
    "drive_retrim": "retrim_blocked",
    "disk_check": "check_blocked",
}

MEDIA_TEXT = {"ssd": "SSD", "hdd": "Hard disk"}

INTRO_TEXT = (
    "Windows' built-in repair and disk tools. They run in the background while you use the rest of "
    "Cairn. What they change isn't in the journal and can't be undone from History."
)
SYSTEM_FILES_CAPTION = (
    "Start with Check system files. If it finds problems it can't fix, run Repair the component "
    "store, then Repair system files again."
)
NON_CANCELLABLE_CAPTION = "This tool runs to completion; Cairn can't stop it once it starts."
ADMIN_TOOLS_NOTE = "Some tools need administrator rights; restart as administrator to open them."
OUTPUT_PLACEHOLDER = "Run a tool to see its output here."
NO_DRIVES_TEXT = "No fixed drives found"
RESTART_SENTENCE = "Restart Windows to finish this repair."
RESTORE_ON_TEXT = "System Protection: On  ·  takes a few seconds to a minute"
RESTORE_OFF_TEXT = "⚠ System Protection is off, so Windows can't create restore points."
RESTORE_UNKNOWN_TEXT = "System Protection: unknown"
# The row reason of the tool whose job is running.
RUNNING_NOW = "running now"
# Lines at the top of the output box above the tool's output: the command line.
HEADER_LINES = 1
# A tool row's layout in logical pixels: its run button with the padding around it, and the
# padding on both sides of its texts. Its texts wrap at no less than `MIN_ROW_WRAP`.
RUN_COLUMN_WIDTH = 90 + 2 * 10
ROW_TEXT_PAD = 2 * 12
MIN_ROW_WRAP = 160
# Padding on both sides of the texts and quick actions of the tools card, and of the texts
# of the output card; the width of the output card's log button.
CARD_TEXT_PAD = 2 * 10
OUTPUT_TEXT_PAD = 2 * 14
LOG_BUTTON_WIDTH = 100


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


# -- pure helpers --------------------------------------------------------------------


def fmt_duration(ms: float) -> str:
    """A duration as "45 s", "12 min 5 s" or "1 h 3 min"; zero minor parts are left out ("12 min")."""
    seconds = max(0, int(ms // 1000))
    if seconds < 60:
        return f"{seconds} s"
    minutes, seconds = divmod(seconds, 60)
    if minutes < 60:
        return f"{minutes} min {seconds} s" if seconds else f"{minutes} min"
    hours, minutes = divmod(minutes, 60)
    return f"{hours} h {minutes} min" if minutes else f"{hours} h"


def duration_text(hint: str, lead: str = "Usually") -> str:
    """The engine's duration hint as a phrase: "Usually 10–30 minutes", "Usually under a minute".

    A hint that already starts with "usually" keeps its wording; a hint split by drive type
    ("SSD: about a minute; hard disk: …") is shown as it is, capitalised.
    """
    text = hint.strip()
    if not text:
        return ""
    if text.lower().startswith("usually "):
        text = text[len("usually ") :]
    if ":" in text.split(";", 1)[0]:
        return text[:1].upper() + text[1:]
    return f"{lead} {text}"


def sentence(text: str) -> str:
    """`text` with a capital first letter and a closing full stop."""
    text = text.strip()
    if not text:
        return ""
    text = text[:1].upper() + text[1:]
    return text if text.endswith((".", "!", "?", "…")) else text + "."


def volume_letter(volume: Mapping[str, Any]) -> str:
    """The volume's drive letter as "C:", whether the engine reports "C", "C:" or "C:\\"."""
    letter = str(volume.get("letter") or "").strip().rstrip("\\").rstrip(":").upper()
    return f"{letter}:" if letter else ""


def size_text(value: int) -> str:
    """`fmt_size` with whole values written plainly: "952 GB", "611.5 GB", "512 MB"."""
    number, _, unit = fmt_size(value).partition(" ")
    if number.endswith(".0"):
        number = number[:-2]
    return f"{number} {unit}"


def volume_text(volume: Mapping[str, Any]) -> str:
    """The drive as "C:  Windows  ·  SSD  ·  NTFS  ·  611 GB free of 952 GB".

    A read error replaces the sizes; an unknown media type is left out.
    """
    head = volume_letter(volume)
    label = str(volume.get("label") or "").strip()
    if label:
        head += f"  {label}"
    parts = [head]
    media = MEDIA_TEXT.get(str(volume.get("media") or "").lower())
    if media:
        parts.append(media)
    if volume.get("file_system"):
        parts.append(str(volume["file_system"]))
    if volume.get("error"):
        parts.append(f"⚠ {volume['error']}")
    elif volume.get("size_bytes"):
        free = size_text(int(volume.get("free_bytes") or 0))
        parts.append(f"{free} free of {size_text(int(volume['size_bytes']))}")
    return "  ·  ".join(parts)


def meta_text(tool: Mapping[str, Any]) -> str:
    """Duration, whether the tool can be stopped and what it changes, for the tool's row."""
    parts = []
    duration = duration_text(str(tool.get("duration_hint") or ""))
    if duration:
        parts.append(duration)
    stoppable = "Can be stopped" if tool.get("cancellable") else "Runs to completion"
    if tool.get("requires_detach"):
        parts += [stoppable, "Repairs can't be undone"]
    elif tool.get("changes_system"):
        parts += [stoppable, "Nothing to undo"]
    else:
        parts += [stoppable, "Read-only"]
    return "  ·  ".join(parts)


def row_block(
    tool: Mapping[str, Any],
    volume: Mapping[str, Any] | None,
    *,
    engine_ready: bool,
    elevated: bool,
    job_running: bool,
    running_tool: str | None = None,
) -> str | None:
    """Why the tool cannot run now, or None when it can; the first reason that applies wins.

    `volume` is the selected drive for drive tools and is ignored by the others. While a job
    runs, the row of its tool (`running_tool`, a tool id) says `RUNNING_NOW` and every other
    row says that another tool is running.
    """
    if not engine_ready:
        return "engine unavailable"
    if tool.get("requires_admin", True) and not elevated:
        return "needs administrator rights"
    if job_running:
        if running_tool is not None and str(tool.get("id")) == running_tool:
            return RUNNING_NOW
        return "another tool is running"
    if not tool.get("needs_volume"):
        return None
    if volume is None:
        return "no drive selected"
    if volume.get("error"):
        return str(volume["error"])
    key = VOLUME_BLOCK_KEYS.get(str(tool.get("id")))
    blocked = volume.get(key) if key else None
    return str(blocked) if blocked else None


def state_style(state: str | None) -> tuple[str, str]:
    """Badge and colour of a job state; an unknown state is shown neutrally."""
    return STATE_STYLE.get(state or "", (f"– {state or 'Unknown'}", theme.INK_MUTED))


def progress_text(progress: float | None) -> str:
    return "" if progress is None else f"{float(progress):.0f}%"


def tool_status_text(title: str, progress: float | None) -> str:
    """Status bar label of a running tool: "◐ Check system files 45%"."""
    pct = progress_text(progress)
    return f"◐ {title} {pct}" if pct else f"◐ {title}"


def time_text(job: Mapping[str, Any]) -> str:
    """How long the job has run and how long ago it printed, or how long it took and its exit code."""
    elapsed = fmt_duration(float(job.get("elapsed_ms") or 0))
    state = job.get("state")
    if state == "running":
        text = f"Running for {elapsed}"
        idle = job.get("idle_ms")
        if idle is not None:
            text += f"  ·  last output {fmt_duration(float(idle))} ago"
            if idle >= IDLE_HINT_MS:
                text += "\nSome steps print nothing for a long time; the tool is still working."
        return text
    text = f"Stopped after {elapsed}" if state == "cancelled" else f"Finished in {elapsed}"
    code = job.get("exit_code")
    if code is not None:
        code_hex = job.get("exit_code_hex") or f"0x{int(code) & 0xFFFFFFFF:08X}"
        text += f"  ·  exit code {code} ({code_hex})"
    return text


def result_text(job: Mapping[str, Any]) -> str:
    """The finished job's hint and summary, plus the restart it needs, one per line."""
    parts = [str(p) for p in (job.get("hint"), job.get("summary")) if p]
    if job.get("restart_required"):
        parts.append(RESTART_SENTENCE)
    return "\n".join(parts)


def skipped_text(count: int) -> str:
    """The output line that stands for `count` lines that are only in the log file."""
    noun = "earlier line is" if count == 1 else "earlier lines are"
    return f"… {count:,} {noun} only in the log file"


class OutputBuffer:
    """Output lines waiting to be inserted into the output box, oldest first.

    At most `cap` entries are kept; lines dropped from the front, and lines the engine no
    longer had (`skipped`), are shown as one "… N earlier lines are only in the log file"
    line in their place.
    """

    def __init__(self, cap: int = 5000) -> None:
        self.cap = cap
        # A str is an output line; an int stands for that many lines that are only in the log.
        self._items: deque[str | int] = deque()
        self._dropped = 0

    @property
    def pending(self) -> int:
        """Number of lines `take` would still return."""
        return len(self._items) + (1 if self._dropped else 0)

    def feed(self, lines: Iterable[str], skipped: int = 0) -> None:
        if skipped > 0:
            self._items.append(int(skipped))
        self._items.extend(str(line) for line in lines)
        while len(self._items) > self.cap:
            item = self._items.popleft()
            self._dropped += item if isinstance(item, int) else 1

    def take(self, limit: int) -> list[str]:
        """Removes and returns up to `limit` lines."""
        out: list[str] = []
        if limit <= 0:
            return out
        if self._dropped:
            out.append(skipped_text(self._dropped))
            self._dropped = 0
        while self._items and len(out) < limit:
            item = self._items.popleft()
            out.append(skipped_text(item) if isinstance(item, int) else item)
        return out

    def clear(self) -> None:
        self._items.clear()
        self._dropped = 0


def _set_state(widget: ctk.CTkBaseClass, enabled: bool) -> None:
    state = "normal" if enabled else "disabled"
    if widget.cget("state") != state:
        widget.configure(state=state)


def _set_color(label: ctk.CTkLabel, color: str) -> None:
    if label.cget("text_color") != color:
        label.configure(text_color=color)


def _neutral_button(master: tk.Misc, text: str, command: Callable[[], None], **kw: Any) -> ctk.CTkButton:
    options: dict[str, Any] = {
        "height": 28,
        "font": _font(12),
        "fg_color": theme.BUTTON_NEUTRAL,
        "hover_color": theme.BUTTON_NEUTRAL_HOVER,
        "text_color": theme.INK,
    }
    options.update(kw)
    return ctk.CTkButton(master, text=text, command=command, **options)


# -- widgets -------------------------------------------------------------------------


class ToolRow(ctk.CTkFrame):
    """One maintenance tool: title, description, duration and effect, and its run button.

    The texts wrap to the width the row gets: the description beside the run button, the
    meta line and the reason below it across the whole row.
    """

    def __init__(
        self, master: tk.Misc, tool: dict[str, Any], on_run: Callable[[dict[str, Any]], None]
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.tool = tool
        self.block: str | None = None
        self._wraps: tuple[int, int] | None = None
        self.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            self,
            text=tool.get("title") or tool.get("id", ""),
            font=_font(12, "bold"),
            text_color=theme.INK,
            anchor="w",
        ).grid(row=0, column=0, sticky="w", padx=12, pady=(8, 0))
        self.description_label = ctk.CTkLabel(
            self,
            text=tool.get("description") or "",
            font=_font(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=380,
        )
        self.description_label.grid(row=1, column=0, sticky="w", padx=12)
        self.meta_label = ctk.CTkLabel(
            self,
            text=meta_text(tool),
            font=_font(10),
            text_color=theme.INK_SECONDARY,
            anchor="w",
            justify="left",
            wraplength=460,
        )
        # The run button spans the first two rows only, so the meta line can use its column.
        self.meta_label.grid(row=2, column=0, columnspan=2, sticky="w", padx=12, pady=(0, 8))
        self.blocked_label = ctk.CTkLabel(
            self,
            text="",
            font=_font(10),
            text_color=theme.WARNING,
            anchor="w",
            justify="left",
            wraplength=460,
        )
        self.run_button = _neutral_button(
            self, tool.get("verb") or "Run", lambda: on_run(tool), width=90, font=_font(12, "bold")
        )
        self.run_button.grid(row=0, column=1, rowspan=2, sticky="e", padx=10, pady=(8, 0))
        self.bind("<Configure>", self._fit_texts)

    def _fit_texts(self, event: tk.Event) -> None:
        """Wraps the texts to the row's new width; only a changed wrap is applied, since a
        wrap changes the row's height and so fires this again."""
        # Event sizes are in screen pixels; wrap lengths are in logical ones.
        width = int(event.width / self._get_widget_scaling())
        across = max(MIN_ROW_WRAP, width - ROW_TEXT_PAD)
        beside = max(MIN_ROW_WRAP, across - RUN_COLUMN_WIDTH)
        if self._wraps == (beside, across):
            return
        self._wraps = (beside, across)
        self.description_label.configure(wraplength=beside)
        self.meta_label.configure(wraplength=across)
        self.blocked_label.configure(wraplength=across)

    def set_block(self, reason: str | None, *, enabled: bool = True) -> None:
        """Shows why the tool cannot run (and disables it), or clears the reason. The row of
        the tool that is running says so neutrally; any other reason is a warning.

        `enabled` False disables the button without a reason, while another operation runs.
        """
        self.block = reason
        if reason:
            icon, color = ("◐", theme.INK_SECONDARY) if reason == RUNNING_NOW else ("⚠", theme.WARNING)
            set_text(self.blocked_label, f"{icon} {reason}")
            _set_color(self.blocked_label, color)
            if not self.blocked_label.winfo_manager():
                self.blocked_label.grid(row=3, column=0, columnspan=2, sticky="w", padx=12, pady=(0, 8))
        elif self.blocked_label.winfo_manager():
            self.blocked_label.grid_forget()
        _set_state(self.run_button, reason is None and enabled)


class ToolsPanel(ctk.CTkFrame):
    """The tools with their drive picker, quick actions and Windows tools on the left; the
    followed job's state, progress and output on the right.

    `show` fills the left card and never touches the output of a running job. The owner
    feeds job views through `begin_job`, `update_job` and `finish_job`; output of a hidden
    section is buffered and inserted by `flush`, at most `FLUSH_LINES` lines per call. The texts
    of both cards wrap to the width the cards get.
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_refresh: Callable[[], None],
        on_run: Callable[[dict[str, Any], str | None], None],
        on_stop: Callable[[], None],
        on_open_log: Callable[[], None],
        on_restore_point: Callable[[], None],
        on_restart_explorer: Callable[[], None],
        on_open_windows_tool: Callable[[str], None],
    ) -> None:
        super().__init__(master, fg_color="transparent")
        self._on_run = on_run
        self._on_open_windows_tool = on_open_windows_tool
        self.grid_columnconfigure((0, 1), weight=1, uniform="tools")
        self.grid_rowconfigure(0, weight=1)

        self.loaded = False
        self._loading = False
        self._engine_ready = True
        self._unsupported = False
        self._elevated = False
        self._actions_enabled = True
        self._job_running = False
        self._job: dict[str, Any] | None = None
        self._restore_enabled: bool | None = None
        self._volumes: list[dict[str, Any]] = []
        self._volume_texts: dict[str, dict[str, Any]] = {}
        self._windows_tools: list[dict[str, Any]] = []
        self._buffer = OutputBuffer()
        self.rows: dict[str, ToolRow] = {}
        self.windows_buttons: dict[str, ctk.CTkButton] = {}
        self._card_wraps: tuple[int, int] | None = None
        self._output_wraps: tuple[int, int] | None = None

        self._build_tools_card(on_refresh, on_restore_point, on_restart_explorer)
        self._build_output_card(on_stop, on_open_log)
        self._refresh_states()

    # -- layout ------------------------------------------------------------------------

    def _section(self, row: int, text: str) -> None:
        ctk.CTkLabel(
            self.tools_card, text=text, font=_font(11, "bold"), text_color=theme.INK_MUTED, anchor="w"
        ).grid(row=row, column=0, sticky="w", padx=10, pady=(14, 2))

    def _caption(self, master: tk.Misc, text: str, wraplength: int = 460) -> ctk.CTkLabel:
        return ctk.CTkLabel(
            master,
            text=text,
            font=_font(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=wraplength,
        )

    def _build_tools_card(
        self,
        on_refresh: Callable[[], None],
        on_restore_point: Callable[[], None],
        on_restart_explorer: Callable[[], None],
    ) -> None:
        card = ctk.CTkScrollableFrame(
            self,
            fg_color=theme.SURFACE,
            corner_radius=10,
            border_width=1,
            border_color=theme.BORDER,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        card.grid(row=0, column=0, sticky="nsew", padx=(0, 5))
        card.grid_columnconfigure(0, weight=1)
        self.tools_card = card

        header = ctk.CTkFrame(card, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=8, pady=(6, 0))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            header, text="Maintenance tools", font=_font(13, "bold"), text_color=theme.INK_SECONDARY
        ).grid(row=0, column=0, sticky="w")
        self.refresh_button = _neutral_button(header, "Refresh", on_refresh, width=100, height=30)
        self.refresh_button.grid(row=0, column=1, sticky="e")
        self.intro_label = ctk.CTkLabel(
            card,
            text=INTRO_TEXT,
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=470,
        )
        self.intro_label.grid(row=1, column=0, sticky="w", padx=10, pady=(2, 0))
        self.summary = ctk.CTkLabel(
            card,
            text="",
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=470,
        )

        self._section(3, "SYSTEM FILES")
        self.system_caption = self._caption(card, SYSTEM_FILES_CAPTION)
        self.system_caption.grid(row=4, column=0, sticky="w", padx=10, pady=(0, 4))
        self._system_box = ctk.CTkFrame(card, fg_color="transparent")
        self._system_box.grid(row=5, column=0, sticky="ew", padx=4)
        self._system_box.grid_columnconfigure(0, weight=1)

        self._section(6, "DRIVES")
        picker = ctk.CTkFrame(card, fg_color="transparent")
        picker.grid(row=7, column=0, sticky="ew", padx=10, pady=(0, 4))
        picker.grid_columnconfigure(1, weight=1)
        ctk.CTkLabel(picker, text="Drive", font=_font(11), text_color=theme.INK_SECONDARY).grid(
            row=0, column=0, sticky="w", padx=(0, 8)
        )
        self.volume_menu = ctk.CTkOptionMenu(
            picker,
            values=[NO_DRIVES_TEXT],
            width=380,
            height=28,
            dynamic_resizing=False,
            font=_font(11),
            fg_color=theme.BUTTON_NEUTRAL,
            button_color=theme.BASELINE,
            button_hover_color=theme.INK_MUTED,
            text_color=theme.INK,
            dropdown_fg_color=theme.SURFACE_RAISED,
            dropdown_hover_color=theme.BUTTON_NEUTRAL_HOVER,
            dropdown_text_color=theme.INK,
            dropdown_font=_font(11),
            command=self._volume_chosen,
            state="disabled",
        )
        self.volume_menu.grid(row=0, column=1, sticky="w")
        self._drive_box = ctk.CTkFrame(card, fg_color="transparent")
        self._drive_box.grid(row=8, column=0, sticky="ew", padx=4)
        self._drive_box.grid_columnconfigure(0, weight=1)

        self._section(9, "QUICK ACTIONS")
        restore = ctk.CTkFrame(card, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        restore.grid(row=10, column=0, sticky="ew", padx=10, pady=4)
        restore.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            restore, text="Create a restore point", font=_font(12, "bold"), text_color=theme.INK, anchor="w"
        ).grid(row=0, column=0, sticky="w", padx=12, pady=(8, 0))
        self.restore_caption = self._caption(restore, RESTORE_UNKNOWN_TEXT, wraplength=330)
        self.restore_caption.grid(row=1, column=0, sticky="w", padx=12, pady=(0, 8))
        self.restore_button = _neutral_button(
            restore, "Create", on_restore_point, width=90, font=_font(12, "bold")
        )
        self.restore_button.grid(row=0, column=1, rowspan=2, sticky="e", padx=10, pady=(8, 8))
        self.protection_button = _neutral_button(
            restore,
            "Open System Protection",
            lambda: self._on_open_windows_tool("system_protection"),
            height=26,
            font=_font(11),
        )

        explorer = ctk.CTkFrame(card, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        explorer.grid(row=11, column=0, sticky="ew", padx=10, pady=4)
        explorer.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(
            explorer, text="Restart File Explorer", font=_font(12, "bold"), text_color=theme.INK, anchor="w"
        ).grid(row=0, column=0, sticky="w", padx=12, pady=(8, 0))
        self.explorer_caption = self._caption(
            explorer,
            "Brings back a frozen taskbar, Start menu or desktop. Open File Explorer windows close.",
            wraplength=330,
        )
        self.explorer_caption.grid(row=1, column=0, sticky="w", padx=12, pady=(0, 8))
        self.explorer_button = _neutral_button(
            explorer, "Restart", on_restart_explorer, width=90, font=_font(12, "bold")
        )
        self.explorer_button.grid(row=0, column=1, rowspan=2, sticky="e", padx=10, pady=(8, 8))

        self._section(12, "WINDOWS TOOLS")
        self.windows_note = self._caption(card, ADMIN_TOOLS_NOTE)
        self._windows_box = ctk.CTkFrame(card, fg_color="transparent")
        self._windows_box.grid(row=14, column=0, sticky="ew", padx=6, pady=(0, 10))
        self._windows_box.grid_columnconfigure((0, 1, 2), weight=1, uniform="windows")
        # Added to the scrollable frame's own binding, which keeps its scroll region current.
        card.bind("<Configure>", self._fit_card_texts, add="+")

    def _fit_card_texts(self, event: tk.Event) -> None:
        """Wraps the tools card's texts to its new width: the texts across the card, and the
        quick actions' captions beside their buttons. Only a changed wrap is applied, since a
        wrap changes the card's height and so fires this again."""
        # Event sizes are in screen pixels; wrap lengths are in logical ones.
        width = int(event.width / self._get_widget_scaling())
        across = max(MIN_ROW_WRAP, width - CARD_TEXT_PAD)
        beside = max(MIN_ROW_WRAP, across - ROW_TEXT_PAD - RUN_COLUMN_WIDTH)
        if self._card_wraps == (across, beside):
            return
        self._card_wraps = (across, beside)
        for label in (self.intro_label, self.summary, self.system_caption, self.windows_note):
            label.configure(wraplength=across)
        for label in (self.restore_caption, self.explorer_caption):
            label.configure(wraplength=beside)

    def _build_output_card(self, on_stop: Callable[[], None], on_open_log: Callable[[], None]) -> None:
        card = ctk.CTkFrame(
            self, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        card.grid(row=0, column=1, sticky="nsew", padx=(5, 0))
        card.grid_columnconfigure(0, weight=1)
        card.grid_rowconfigure(6, weight=1)
        self.output_card = card

        header = ctk.CTkFrame(card, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(0, weight=1)
        ctk.CTkLabel(header, text="Output", font=_font(13, "bold"), text_color=theme.INK_SECONDARY).grid(
            row=0, column=0, sticky="w"
        )
        self.state_label = ctk.CTkLabel(
            header, text="", font=_font(12, "bold"), text_color=theme.INK_SECONDARY
        )
        self.state_label.grid(row=0, column=1, sticky="e")
        self.job_label = ctk.CTkLabel(
            card,
            text=OUTPUT_PLACEHOLDER,
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
        )
        self.job_label.grid(row=1, column=0, sticky="ew", padx=14)

        progress = ctk.CTkFrame(card, fg_color="transparent")
        progress.grid(row=2, column=0, sticky="ew", padx=14, pady=(6, 0))
        progress.grid_columnconfigure(0, weight=1)
        self.progress_bar = ctk.CTkProgressBar(
            progress, mode="determinate", height=8, progress_color=theme.ACCENT, fg_color=theme.BASELINE
        )
        self.progress_bar.set(0)
        self.progress_label = ctk.CTkLabel(
            progress, text="", font=_font(11), text_color=theme.INK_SECONDARY, anchor="e"
        )
        self.progress_label.grid(row=0, column=1, sticky="e")
        self.progress_line_label = ctk.CTkLabel(
            card, text="", font=_font(10), text_color=theme.INK_MUTED, anchor="w", justify="left"
        )
        self.progress_line_label.grid(row=3, column=0, sticky="ew", padx=14)
        self.time_label = ctk.CTkLabel(
            card, text="", font=_font(10), text_color=theme.INK_MUTED, anchor="w", justify="left"
        )
        self.time_label.grid(row=4, column=0, sticky="ew", padx=14)
        self.result_label = ctk.CTkLabel(
            card,
            text="",
            font=_font(11),
            text_color=theme.INK_SECONDARY,
            anchor="w",
            justify="left",
            wraplength=500,
        )
        self.output = ctk.CTkTextbox(
            card,
            font=ctk.CTkFont(family=theme.MONO_FAMILY, size=10),
            fg_color=theme.PAGE,
            text_color=theme.INK_SECONDARY,
            border_width=1,
            border_color=theme.BORDER,
            wrap="none",
            undo=False,
            state="disabled",
        )
        self.output.grid(row=6, column=0, sticky="nsew", padx=10, pady=(6, 0))

        footer = ctk.CTkFrame(card, fg_color="transparent")
        footer.grid(row=7, column=0, sticky="ew", padx=14, pady=10)
        footer.grid_columnconfigure(0, weight=1)
        self.caption_label = self._caption(footer, "", wraplength=360)
        self.caption_label.grid(row=0, column=0, sticky="w")
        self.stop_button = ctk.CTkButton(
            footer,
            text="Stop",
            width=90,
            height=30,
            font=_font(12, "bold"),
            fg_color=theme.CRITICAL,
            hover_color=theme.CRITICAL_HOVER,
            text_color=theme.INK,
            command=on_stop,
        )
        self.log_button = _neutral_button(
            footer, "Open log", on_open_log, width=LOG_BUTTON_WIDTH, height=30, state="disabled"
        )
        self.log_button.grid(row=0, column=2, sticky="e")
        card.bind("<Configure>", self._fit_output_texts)

    def _fit_output_texts(self, event: tk.Event) -> None:
        """Wraps the output card's texts to its new width; only a changed wrap is applied."""
        width = int(event.width / self._get_widget_scaling())
        across = max(MIN_ROW_WRAP, width - OUTPUT_TEXT_PAD)
        # The caption has text only while the stop button is hidden, so it shares its line
        # with the log button alone.
        caption = max(MIN_ROW_WRAP, across - LOG_BUTTON_WIDTH)
        if self._output_wraps == (across, caption):
            return
        self._output_wraps = (across, caption)
        for label in (self.job_label, self.progress_line_label, self.time_label, self.result_label):
            label.configure(wraplength=across)
        self.caption_label.configure(wraplength=caption)

    # -- loading -----------------------------------------------------------------------

    @property
    def pending_output(self) -> int:
        """Output lines read from the engine and not yet in the output box."""
        return self._buffer.pending

    def _set_summary(self, text: str, color: str) -> None:
        if text:
            set_text(self.summary, text)
            _set_color(self.summary, color)
            if not self.summary.winfo_manager():
                self.summary.grid(row=2, column=0, sticky="w", padx=10, pady=(4, 0))
        elif self.summary.winfo_manager():
            self.summary.grid_forget()

    def set_loading(self) -> None:
        self._loading = True
        self._set_summary("Reading the tools and drives…", theme.INK_MUTED)
        self._refresh_states()

    def show_error(self, message: str) -> None:
        self._loading = False
        self._set_summary(f"Could not read the tools: {message}", theme.CRITICAL)
        self._refresh_states()

    def set_engine_ready(self, ready: bool) -> None:
        self._engine_ready = ready
        if not ready:
            self._set_summary("⚠ The engine is not available, so these tools can't run.", theme.WARNING)
        self._refresh_states()

    def set_unsupported(self, text: str) -> None:
        self._unsupported = True
        self._set_summary(f"⚠ {text}", theme.WARNING)
        self._refresh_states()

    def show(
        self,
        catalog: Sequence[dict[str, Any]],
        volumes: Sequence[dict[str, Any]],
        restore_enabled: bool | None,
        windows_tools: Sequence[dict[str, Any]],
        *,
        engine_ready: bool,
        elevated: bool,
    ) -> None:
        """Lists the tools, drives, quick actions and Windows tools; a running job's output stays."""
        self.loaded = True
        self._loading = False
        self._engine_ready = engine_ready
        self._elevated = elevated
        self._restore_enabled = restore_enabled
        self._set_summary("", theme.INK_MUTED)

        for row in self.rows.values():
            row.destroy()
        self.rows = {}
        placed = {"system_files": 0, "drives": 0}
        for tool in catalog:
            group = "drives" if tool.get("group") == "drives" or tool.get("needs_volume") else "system_files"
            box = self._drive_box if group == "drives" else self._system_box
            row = ToolRow(box, dict(tool), self._run)
            row.grid(row=placed[group], column=0, sticky="ew", padx=6, pady=4)
            placed[group] += 1
            self.rows[str(tool["id"])] = row

        previous = self.selected_volume()
        self._volumes = [dict(v) for v in volumes]
        self._volume_texts = {volume_text(v): v for v in self._volumes}
        if self._volumes:
            self.volume_menu.configure(values=list(self._volume_texts))
            chosen = next((v for v in self._volumes if volume_letter(v) == previous), self._volumes[0])
            self.volume_menu.set(volume_text(chosen))
        else:
            self.volume_menu.configure(values=[NO_DRIVES_TEXT])
            self.volume_menu.set(NO_DRIVES_TEXT)

        if restore_enabled is True:
            caption, color = RESTORE_ON_TEXT, theme.INK_MUTED
        elif restore_enabled is False:
            caption, color = RESTORE_OFF_TEXT, theme.WARNING
        else:
            caption, color = RESTORE_UNKNOWN_TEXT, theme.INK_MUTED
        if not elevated:
            caption += "  ·  needs administrator rights"
        set_text(self.restore_caption, caption)
        _set_color(self.restore_caption, color)
        if restore_enabled is False:
            if not self.protection_button.winfo_manager():
                self.protection_button.grid(row=2, column=0, sticky="w", padx=12, pady=(0, 8))
        elif self.protection_button.winfo_manager():
            self.protection_button.grid_forget()

        for button in self.windows_buttons.values():
            button.destroy()
        self.windows_buttons = {}
        self._windows_tools = [dict(t) for t in windows_tools]
        for i, info in enumerate(self._windows_tools):
            tool_id = str(info["id"])
            button = _neutral_button(
                self._windows_box,
                info.get("title") or tool_id,
                lambda tool_id=tool_id: self._on_open_windows_tool(tool_id),
                height=30,
                font=_font(11),
            )
            button.grid(row=i // 3, column=i % 3, sticky="ew", padx=4, pady=4)
            self.windows_buttons[tool_id] = button
        needs_note = not elevated and any(t.get("requires_admin") for t in self._windows_tools)
        if needs_note and not self.windows_note.winfo_manager():
            self.windows_note.grid(row=13, column=0, sticky="w", padx=10, pady=(0, 4))
        elif not needs_note and self.windows_note.winfo_manager():
            self.windows_note.grid_forget()

        self._refresh_states()

    # -- selection and enablement --------------------------------------------------------

    def selected_volume(self) -> str | None:
        """Letter ("C:") of the selected drive, or None when no drive is listed."""
        volume = self._volume_texts.get(self.volume_menu.get())
        return volume_letter(volume) if volume is not None else None

    def selected_volume_info(self) -> dict[str, Any] | None:
        return self._volume_texts.get(self.volume_menu.get())

    def select_volume(self, letter: str) -> bool:
        """Selects the drive `letter` ("C", "C:" or "C:\\"); False when it is not listed."""
        wanted = volume_letter({"letter": letter})
        volume = next((v for v in self._volumes if volume_letter(v) == wanted), None)
        if volume is None:
            return False
        self.volume_menu.set(volume_text(volume))
        self._refresh_states()
        return True

    def _volume_chosen(self, _value: str) -> None:
        self._refresh_states()

    def set_actions_enabled(self, enabled: bool) -> None:
        """Enables or disables the actions while another operation runs."""
        if enabled != self._actions_enabled:
            self._actions_enabled = enabled
            self._refresh_states()

    def _refresh_states(self) -> None:
        ready = self._engine_ready and not self._unsupported
        volume = self.selected_volume_info()
        running_tool = str(self._job.get("tool")) if self._job_running and self._job else None
        for row in self.rows.values():
            reason = row_block(
                row.tool,
                volume,
                engine_ready=ready,
                elevated=self._elevated,
                job_running=self._job_running,
                running_tool=running_tool,
            )
            row.set_block(reason, enabled=self._actions_enabled)
        _set_state(self.refresh_button, ready and not self._loading)
        _set_state(self.volume_menu, bool(self._volumes))
        _set_state(
            self.restore_button,
            ready
            and self.loaded
            and self._elevated
            and self._restore_enabled is not False
            and self._actions_enabled,
        )
        _set_state(self.protection_button, ready and self._elevated)
        _set_state(self.explorer_button, ready and self._actions_enabled)
        for info in self._windows_tools:
            button = self.windows_buttons.get(str(info["id"]))
            if button is not None:
                _set_state(button, ready and (self._elevated or not info.get("requires_admin", True)))
        _set_state(self.log_button, ready and bool(self._job and self._job.get("log_path")))

    def _run(self, tool: dict[str, Any]) -> None:
        self._on_run(tool, self.selected_volume() if tool.get("needs_volume") else None)

    # -- the followed job ----------------------------------------------------------------

    def _show_state(self, job: Mapping[str, Any]) -> None:
        badge, color = state_style(job.get("state"))
        set_text(self.state_label, badge)
        _set_color(self.state_label, color)

    def _show_progress(self, progress: float | None) -> None:
        if progress is None:
            if self.progress_bar.winfo_manager():
                self.progress_bar.grid_forget()
            set_text(self.progress_label, "Working…")
            return
        if not self.progress_bar.winfo_manager():
            self.progress_bar.grid(row=0, column=0, sticky="ew", padx=(0, 10))
        value = max(0.0, min(1.0, float(progress) / 100.0))
        if abs(self.progress_bar.get() - value) > 1e-4:
            self.progress_bar.set(value)
        set_text(self.progress_label, progress_text(progress))

    def _hide_progress(self) -> None:
        if self.progress_bar.winfo_manager():
            self.progress_bar.grid_forget()
        set_text(self.progress_label, "")
        set_text(self.progress_line_label, "")

    def begin_job(self, job: Mapping[str, Any]) -> None:
        """Clears the output and follows the job that just started."""
        self._job = dict(job)
        self._job_running = True
        self._buffer.clear()
        title = job.get("title") or job.get("tool") or "Tool"
        command = job.get("command_line") or ""
        set_text(self.job_label, f"{title}  ·  {command}" if command else str(title))
        _set_color(self.job_label, theme.INK_SECONDARY)
        self.output.configure(state="normal")
        self.output.delete("1.0", "end")
        self.output.insert("1.0", f"> {command}")
        self.output.configure(state="disabled")
        self._show_state(job)
        self._show_progress(job.get("progress"))
        set_text(self.progress_line_label, str(job.get("progress_line") or ""))
        set_text(self.time_label, time_text(job))
        if self.result_label.winfo_manager():
            self.result_label.grid_forget()
        if job.get("cancellable"):
            self.stop_button.configure(state="normal", text="Stop")
            if not self.stop_button.winfo_manager():
                self.stop_button.grid(row=0, column=1, sticky="e", padx=(0, 8))
            set_text(self.caption_label, "")
        else:
            if self.stop_button.winfo_manager():
                self.stop_button.grid_forget()
            set_text(self.caption_label, NON_CANCELLABLE_CAPTION)
        self._refresh_states()

    def update_job(self, view: Mapping[str, Any], *, visible: bool) -> None:
        """Shows the job's latest state and queues its new output lines; inserts some of them
        right away when the section is `visible`."""
        self._job = {k: v for k, v in view.items() if k != "lines"}
        self._show_state(view)
        if view.get("state") == "running":
            self._show_progress(view.get("progress"))
            set_text(self.progress_line_label, str(view.get("progress_line") or ""))
            if view.get("cancel_requested"):
                self.set_stopping()
        set_text(self.time_label, time_text(view))
        self._buffer.feed(view.get("lines") or [], int(view.get("skipped") or 0))
        if visible:
            self.flush()

    def set_stopping(self) -> None:
        if self.stop_button.winfo_manager():
            self.stop_button.configure(state="disabled", text="Stopping…")

    def finish_job(self, view: Mapping[str, Any]) -> None:
        """Shows the final state, the result and the time the job took."""
        self._job = {k: v for k, v in view.items() if k != "lines"}
        self._job_running = False
        self._show_state(view)
        self._hide_progress()
        set_text(self.time_label, time_text(view))
        text = result_text(view)
        if text:
            badge, color = state_style(view.get("state"))
            set_text(self.result_label, f"{badge.split(' ', 1)[0]} {text}")
            _set_color(self.result_label, color)
            if not self.result_label.winfo_manager():
                self.result_label.grid(row=5, column=0, sticky="ew", padx=14, pady=(6, 0))
        elif self.result_label.winfo_manager():
            self.result_label.grid_forget()
        if self.stop_button.winfo_manager():
            self.stop_button.grid_forget()
        set_text(self.caption_label, "")
        self._refresh_states()

    def lose_job(self, message: str) -> None:
        """Stops following a job the engine no longer reports."""
        self._job_running = False
        set_text(self.state_label, "⚠ Unknown")
        _set_color(self.state_label, theme.CRITICAL)
        self._hide_progress()
        set_text(self.result_label, f"⚠ {message}")
        _set_color(self.result_label, theme.CRITICAL)
        if not self.result_label.winfo_manager():
            self.result_label.grid(row=5, column=0, sticky="ew", padx=14, pady=(6, 0))
        if self.stop_button.winfo_manager():
            self.stop_button.grid_forget()
        set_text(self.caption_label, "")
        self._refresh_states()

    def flush(self, limit: int = FLUSH_LINES) -> int:
        """Inserts up to `limit` buffered lines and returns how many were inserted.

        The box keeps the last `MAX_OUTPUT_LINES` lines below the command line and follows
        the end only while it is scrolled to the bottom.
        """
        lines = self._buffer.take(limit)
        if not lines:
            return 0
        follow = self.output.yview()[1] >= 0.999
        text = "\n".join(lines)
        self.output.configure(state="normal")
        if self.output.compare("end-1c", "!=", "1.0"):
            text = "\n" + text
        self.output.insert("end", text)
        total = int(self.output.index("end-1c").split(".")[0])
        excess = total - HEADER_LINES - MAX_OUTPUT_LINES
        if excess > 0:
            first = HEADER_LINES + 1
            self.output.delete(f"{first}.0", f"{first + excess}.0")
        self.output.configure(state="disabled")
        if follow:
            # Moving the view costs a third of `see("end")`, which also lays out the target
            # line for horizontal scrolling, and it keeps the horizontal position.
            self.output.yview_moveto(1.0)
        return len(lines)

    def output_text(self) -> str:
        """Everything in the output box: the command line, then the tool's output."""
        return str(self.output.get("1.0", "end-1c"))
