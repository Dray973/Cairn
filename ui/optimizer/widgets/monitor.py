"""Live hardware monitor fed by the telemetry DLL.

`HardwareMonitor.update_frame` runs on every UI frame (60 Hz): charts and core bars move
each frame, text readouts refresh at `TEXT_HZ` so they stay legible, and the process
table follows the sampler's process-scan cadence.

Layout: the cards ask only for compact sizes (charts and core bars at their smallest useful
height, the process table for `TABLE_PREFERRED_ROWS` rows), so the whole monitor fits the
smallest window. Extra height goes to the charts and core bars; the process table shows as
many whole rows as its card holds, up to `TABLE_ROWS`.
"""

from __future__ import annotations

import tkinter as tk

import customtkinter as ctk

from .. import theme
from ..bridge.telemetry import TelSnapshot, cores, top_processes
from .charts import CoreBars, Meter, TimeSeriesChart

TEXT_HZ = 10.0
TABLE_ROWS = 8
# Process rows the table asks room for; a taller card shows more, up to TABLE_ROWS.
TABLE_PREFERRED_ROWS = 6
# Heights of a card title and a stat line, in CustomTkinter units: the text plus a few pixels.
TITLE_HEIGHT = 22
STAT_LINE_HEIGHT = 18
# Space above and below the process table's header rule, in pixels.
SEPARATOR_PADY = 2
GIB = 1024**3
MIB = 1024**2


def fmt_gb(value: int) -> str:
    return f"{value / GIB:.1f} GB"


def fmt_rate(value: float) -> str:
    if value >= 1e6:
        return f"{value / 1e6:.1f}M/s"
    if value >= 1e3:
        return f"{value / 1e3:.1f}k/s"
    return f"{value:.0f}/s"


def fmt_count(value: int) -> str:
    return f"{value:,}"


def set_text(label: ctk.CTkLabel, text: str) -> None:
    """Updates a label only when its text changes; CTkLabel redraws on every configure."""
    if label.cget("text") != text:
        label.configure(text=text)


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


class Card(ctk.CTkFrame):
    """Surface panel with a title row."""

    def __init__(self, master: tk.Misc, title: str, **kw: object) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER, **kw
        )
        self.grid_columnconfigure(0, weight=1)
        self.title = ctk.CTkLabel(
            self,
            text=title,
            font=_font(13, "bold"),
            text_color=theme.INK_SECONDARY,
            anchor="w",
            height=TITLE_HEIGHT,
        )
        self.title.grid(row=0, column=0, sticky="w", padx=14, pady=(10, 0))


class StatLine(ctk.CTkLabel):
    def __init__(self, master: tk.Misc, text: str = "") -> None:
        super().__init__(
            master,
            text=text,
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            height=STAT_LINE_HEIGHT,
        )


class CpuCard(Card):
    def __init__(self, master: tk.Misc) -> None:
        super().__init__(master, "CPU")
        self.hero = ctk.CTkLabel(self, text="–", font=_font(30, "bold"), text_color=theme.INK, anchor="w")
        self.hero.grid(row=1, column=0, sticky="w", padx=14)
        self.detail = StatLine(self)
        self.detail.grid(row=2, column=0, sticky="ew", padx=14)
        self.chart = TimeSeriesChart(self, color=theme.CPU, fill=theme.CPU_FILL, label="CPU", height=64)
        self.chart.grid(row=3, column=0, sticky="nsew", padx=10, pady=(8, 2))
        self.grid_rowconfigure(3, weight=3)
        self.cores_label = StatLine(self, text="Per core (250 ms average)")
        self.cores_label.grid(row=4, column=0, sticky="w", padx=14, pady=(4, 0))
        self.bars = CoreBars(self, color=theme.CPU, height=48)
        self.bars.grid(row=5, column=0, sticky="nsew", padx=10)
        self.grid_rowconfigure(5, weight=1)
        self.rates = StatLine(self)
        self.rates.grid(row=6, column=0, sticky="ew", padx=14, pady=(2, 10))

    def frame(self, s: TelSnapshot, now: float) -> None:
        self.chart.push(now, s.cpu.total_utilization)
        self.chart.render(now)
        self.bars.render([(c.utilization, c.frequency_mhz) for c in cores(s)])

    def text(self, s: TelSnapshot) -> None:
        cpu = s.cpu
        set_text(self.hero, f"{cpu.total_utilization:.0f}%")
        clocks = [c.frequency_mhz for c in cores(s) if c.frequency_mhz]
        clock = f"  ·  {sum(clocks) / len(clocks) / 1000:.2f} GHz average clock" if clocks else ""
        set_text(
            self.detail,
            f"{cpu.core_count} logical processors  ·  kernel {cpu.total_kernel:.0f}%  ·  "
            f"user {cpu.total_user:.0f}%  ·  DPC/ISR {cpu.total_dpc_interrupt:.1f}%{clock}",
        )
        set_text(
            self.rates,
            f"Context switches {fmt_rate(cpu.context_switches_per_sec)}  ·  "
            f"System calls {fmt_rate(cpu.syscalls_per_sec)}  ·  "
            f"Interrupts {fmt_rate(cpu.interrupts_per_sec)}",
        )


class MemoryCard(Card):
    def __init__(self, master: tk.Misc) -> None:
        super().__init__(master, "Memory")
        self.hero = ctk.CTkLabel(self, text="–", font=_font(24, "bold"), text_color=theme.INK, anchor="w")
        self.hero.grid(row=1, column=0, sticky="w", padx=14)
        self.meter = Meter(self, color=theme.MEMORY, height=8)
        self.meter.grid(row=2, column=0, sticky="ew", padx=14, pady=(4, 2))
        self.chart = TimeSeriesChart(
            self, color=theme.MEMORY, fill=theme.MEMORY_FILL, label="Memory", height=64
        )
        self.chart.grid(row=3, column=0, sticky="nsew", padx=10, pady=(6, 2))
        self.grid_rowconfigure(3, weight=1)
        self.commit = StatLine(self)
        self.commit.grid(row=4, column=0, sticky="ew", padx=14)
        self.pools = StatLine(self)
        self.pools.grid(row=5, column=0, sticky="ew", padx=14)
        self.paging = StatLine(self)
        self.paging.grid(row=6, column=0, sticky="ew", padx=14, pady=(0, 10))

    def frame(self, s: TelSnapshot, now: float) -> None:
        mem = s.memory
        self.chart.push(now, mem.memory_load_percent)
        self.chart.render(now)
        total = mem.physical_total_bytes or 1
        self.meter.render(mem.physical_used_bytes / total)

    def text(self, s: TelSnapshot) -> None:
        mem = s.memory
        set_text(
            self.hero,
            f"{fmt_gb(mem.physical_used_bytes)} of {fmt_gb(mem.physical_total_bytes)}  ·  "
            f"{mem.memory_load_percent:.0f}%",
        )
        set_text(
            self.commit,
            f"Committed {fmt_gb(mem.commit_total_bytes)} of {fmt_gb(mem.commit_limit_bytes)} "
            f"({mem.commit_percent:.0f}%)  ·  Cache {fmt_gb(mem.system_cache_bytes)}",
        )
        set_text(
            self.pools,
            f"Paged pool {fmt_gb(mem.kernel_paged_pool_bytes)}  ·  "
            f"Non-paged pool {fmt_gb(mem.kernel_nonpaged_pool_bytes)}",
        )
        set_text(
            self.paging,
            f"Page faults {fmt_rate(mem.page_faults_per_sec)}  ·  "
            f"Pages in {fmt_rate(mem.hard_faults_per_sec)}  ·  "
            f"Pages out {fmt_rate(mem.pages_output_per_sec)}",
        )


class ProcessCard(Card):
    COLUMNS = (("Process", "w", 0), ("CPU", "e", 64), ("Memory", "e", 80), ("Threads", "e", 64))

    def __init__(self, master: tk.Misc) -> None:
        super().__init__(master, "Processes")
        self.summary = StatLine(self)
        self.summary.grid(row=1, column=0, sticky="ew", padx=14)
        self.threads = StatLine(self)
        self.threads.grid(row=2, column=0, sticky="ew", padx=14)

        table = tk.Frame(self, bg=theme.SURFACE)
        table.grid(row=3, column=0, sticky="nsew", padx=14, pady=(6, 10))
        table.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(3, weight=1)
        header_font = (theme.FONT_FAMILY, 9, "bold")
        cell_font = (theme.FONT_FAMILY, 10)
        headers = []
        for col, (name, anchor, width) in enumerate(self.COLUMNS):
            header = tk.Label(
                table, text=name, font=header_font, fg=theme.INK_MUTED, bg=theme.SURFACE, anchor=anchor
            )
            header.grid(row=0, column=col, sticky="ew", padx=(0 if col == 0 else 8, 0))
            headers.append(header)
            if width:
                table.grid_columnconfigure(col, minsize=width)
        tk.Frame(table, bg=theme.GRID, height=1).grid(
            row=1, column=0, columnspan=4, sticky="ew", pady=(SEPARATOR_PADY, SEPARATOR_PADY)
        )
        self._header = headers[0]
        self._cells: list[list[tk.Label]] = []
        for r in range(TABLE_ROWS):
            row = []
            for col, (_, anchor, _) in enumerate(self.COLUMNS):
                label = tk.Label(
                    table,
                    text="",
                    font=cell_font,
                    fg=theme.INK if col == 0 else theme.INK_SECONDARY,
                    bg=theme.SURFACE,
                    anchor=anchor,
                )
                label.grid(row=r + 2, column=col, sticky="ew", padx=(0 if col == 0 else 8, 0))
                row.append(label)
            self._cells.append(row)
        self._last_rows: list[tuple[str, ...]] = []

        # The table asks for TABLE_PREFERRED_ROWS rows and shows as many whole rows as the
        # height it gets holds, so a short window drops the last processes instead of cutting
        # a row. It does not propagate its size: the rows shown never change what it asks
        # for, so hiding one cannot change the height it is given.
        top, row_height = self._table_metrics()
        table.configure(height=top + TABLE_PREFERRED_ROWS * row_height)
        table.grid_propagate(False)
        self._shown_rows = TABLE_ROWS
        table.bind("<Configure>", self._fit_rows)

    @property
    def shown_rows(self) -> int:
        """Rows of the table that are drawn; the others do not fit the card's height."""
        return self._shown_rows

    def _table_metrics(self) -> tuple[int, int]:
        """Height of the headers and separator above the first row, and of one row."""
        top = self._header.winfo_reqheight() + 1 + 2 * SEPARATOR_PADY
        return top, max(self._cells[0][0].winfo_reqheight(), 1)

    def _fit_rows(self, event: tk.Event) -> None:
        top, row_height = self._table_metrics()
        self._show_rows(min(max(0, (event.height - top) // row_height), TABLE_ROWS))

    def _show_rows(self, count: int) -> None:
        if count == self._shown_rows:
            return
        for r, row in enumerate(self._cells):
            for label in row:
                if r < count:
                    label.grid()
                else:
                    label.grid_remove()
        self._shown_rows = count

    def text(self, s: TelSnapshot) -> None:
        p = s.processes
        set_text(
            self.summary, f"{fmt_count(p.process_count)} processes  ·  {fmt_count(p.handle_count)} handles"
        )
        t = p.threads
        set_text(
            self.threads,
            f"{fmt_count(t.total)} threads  ·  {t.running} running  ·  {t.ready} ready  ·  "
            f"{fmt_count(t.waiting)} waiting",
        )
        rows = []
        for proc in top_processes(s)[:TABLE_ROWS]:
            rows.append(
                (
                    proc.name,
                    f"{proc.cpu_percent:.1f}%",
                    f"{proc.working_set_bytes / MIB:,.0f} MB",
                    fmt_count(proc.thread_count),
                )
            )
        rows += [("", "", "", "")] * (TABLE_ROWS - len(rows))
        if rows == self._last_rows:
            return
        for r, values in enumerate(rows):
            for c, value in enumerate(values):
                cell = self._cells[r][c]
                if cell.cget("text") != value:
                    cell.configure(text=value)
        self._last_rows = rows


class HardwareMonitor(ctk.CTkFrame):
    """CPU card on top, memory and processes side by side below."""

    def __init__(self, master: tk.Misc) -> None:
        super().__init__(master, fg_color="transparent")
        self.grid_columnconfigure((0, 1), weight=1, uniform="monitor")
        # Extra height goes mostly to the CPU card; the lower row gets enough of it for the
        # process table to reach TABLE_ROWS rows at the default window size.
        self.grid_rowconfigure(0, weight=3)
        self.grid_rowconfigure(1, weight=1)
        self.cpu = CpuCard(self)
        self.cpu.grid(row=0, column=0, columnspan=2, sticky="nsew", pady=(0, 10))
        self.memory = MemoryCard(self)
        self.memory.grid(row=1, column=0, sticky="nsew", padx=(0, 5))
        self.processes = ProcessCard(self)
        self.processes.grid(row=1, column=1, sticky="nsew", padx=(5, 0))
        self._last_text = 0.0
        self._last_sequence = -1

    def show_message(self, text: str) -> None:
        set_text(self.cpu.hero, "–")
        set_text(self.cpu.detail, text)

    def update_frame(self, s: TelSnapshot, now: float, visible: bool = True) -> None:
        """Records the sample in the chart histories; draws only while `visible`, so a
        hidden dashboard keeps its 60 s history without spending time on canvases."""
        if not visible:
            self.cpu.chart.push(now, s.cpu.total_utilization)
            self.memory.chart.push(now, s.memory.memory_load_percent)
            return
        self.cpu.frame(s, now)
        self.memory.frame(s, now)
        if now - self._last_text >= 1.0 / TEXT_HZ:
            self._last_text = now
            self.cpu.text(s)
            self.memory.text(s)
            if s.sequence != self._last_sequence:
                self._last_sequence = s.sequence
                self.processes.text(s)
