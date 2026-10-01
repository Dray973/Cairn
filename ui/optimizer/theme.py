"""Colour and type tokens for the dark dashboard.

Chart colours are the dark-mode steps of the validated reference palette: series 1 (blue)
for CPU and series 2 (orange) for memory, each fixed to its entity everywhere it appears.
Status colours are reserved for state and always ship with an icon and a label.
"""

from __future__ import annotations

# Surfaces and ink
PAGE = "#0d0d0d"
SURFACE = "#1a1a19"
SURFACE_RAISED = "#232322"
BORDER = "#2c2c2a"
GRID = "#2c2c2a"
BASELINE = "#383835"
INK = "#ffffff"
INK_SECONDARY = "#c3c2b7"
INK_MUTED = "#898781"

# Series (identity)
CPU = "#3987e5"
MEMORY = "#d95926"

# Status (state)
GOOD = "#0ca30c"
WARNING = "#fab219"
SERIOUS = "#ec835a"
CRITICAL = "#d03b3b"
CRITICAL_HOVER = "#b53232"

# Controls
ACCENT = CPU
ACCENT_HOVER = "#2a78d6"
BUTTON_NEUTRAL = "#2c2c2a"
BUTTON_NEUTRAL_HOVER = "#383835"

FONT_FAMILY = "Segoe UI"
# Fixed-width text such as console tool output.
MONO_FAMILY = "Consolas"


def blend(color: str, over: str, alpha: float) -> str:
    """Opaque equivalent of `color` drawn at `alpha` over `over` (Tk has no alpha)."""
    c = [int(color[i : i + 2], 16) for i in (1, 3, 5)]
    b = [int(over[i : i + 2], 16) for i in (1, 3, 5)]
    mixed = [round(bc + (cc - bc) * alpha) for cc, bc in zip(c, b, strict=True)]
    return "#" + "".join(f"{v:02x}" for v in mixed)


CPU_FILL = blend(CPU, SURFACE, 0.16)
MEMORY_FILL = blend(MEMORY, SURFACE, 0.16)
