"""Cairn dashboard package.

Layout:
    optimizer/__main__.py         entry point: `python -m optimizer` starts the window
    optimizer/app.py              CustomTkinter root window: sidebar, sections, 60 Hz frame loop
    optimizer/sections.py         the sections: order, sidebar group, glyph and App hooks
    optimizer/system.py           Windows process helpers (elevation, dialogs, Explorer, app id)
    optimizer/bridge/telemetry.py ctypes binding for native/optimizer_telemetry.dll
    optimizer/bridge/engine.py    thread-safe wrapper over native/optimizer_engine.pyd (PyO3)
    optimizer/features/           each section's state and flows, as mixins of the main window
    optimizer/widgets/            sidebar, brand mark, hardware monitor, panels and dialogs
    optimizer/native/             build artifacts dropped here by scripts/deploy_natives.ps1
"""

from __future__ import annotations

import sys
from pathlib import Path

__version__ = "0.2.0"

APP_NAME = "Cairn"
APP_ID = "Cairn.App"  # AppUserModelID of the installed app (shortcut and taskbar)
DEV_APP_ID = "Cairn.App.Dev"  # AppUserModelID of `python -m optimizer`
PROJECT_URL = "https://github.com/Dray973/Cairn"
PACKAGE_DIR = Path(__file__).resolve().parent
NATIVE_DIR = PACKAGE_DIR / "native"
ICON_PATH = PACKAGE_DIR / "assets" / "cairn.ico"

# Make the PyO3 module importable as a top-level name (`import optimizer_engine`)
# and let ctypes find the telemetry DLL next to it.
if str(NATIVE_DIR) not in sys.path:
    sys.path.insert(0, str(NATIVE_DIR))
