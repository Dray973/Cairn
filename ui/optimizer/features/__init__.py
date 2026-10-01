"""Feature mixins of the main window: each adds one section's state, widgets and flows to `App`.

Sections are listed in `optimizer.sections.SECTIONS` (name, sidebar group, glyph and the
names of two App methods). A mixin defines methods only, never `__init__`, and names its
hooks after its section with the `_tab` suffix:

- `_init_<feature>_state()` sets the mixin's state; `App.__init__` calls it (from
  `app.INIT_HOOKS`) before the window is built.
- `_build_<feature>_tab(frame)` builds the section into its frame, once, and handles a
  missing or outdated engine itself.
- `_<feature>_tab_shown()` runs each time the section is shown while the engine is loaded.

The window calls the other hooks by name from its registries in `optimizer.app`, looking
each method up when it calls it: `BUSY_HOOKS` (the busy state changed), `AFTER_MUTATION_HOOKS`
(a change or an undo finished), `TITLE_PROVIDERS` (History titles of journal targets),
`STARTUP_HOOKS` (once the engine is loaded) and `JOB_LANES` (engine-owned background jobs:
polled every frame, asked before the window closes and stopped before the engine shuts
down). One failing hook is reported and the others still run.

Visibility is read only through `section_visible(name)`; a section is shown with
`show_section(name)`. Mixins never import `optimizer.app`, so close kinds are passed as
plain strings ("read", "journaled", "irreversible"), and they call process helpers through
the `optimizer.system` module so tests can replace them. User text names a section ("Undo it
from History."), never a tab.
"""

from __future__ import annotations

from typing import Any

PLACEHOLDER_TEXT = "This section is not available in this build yet."


def build_placeholder(frame: Any) -> None:
    """Fills a section's frame with one centred line saying the section is not available."""
    import customtkinter as ctk

    from .. import theme

    ctk.CTkLabel(
        frame,
        text=PLACEHOLDER_TEXT,
        font=ctk.CTkFont(family=theme.FONT_FAMILY, size=12),
        text_color=theme.INK_MUTED,
    ).grid(row=0, column=0)
