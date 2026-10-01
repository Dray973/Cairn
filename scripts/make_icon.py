"""Writes Cairn's icon and logo from the brand renderer.

    ui/optimizer/assets/cairn.ico  the window, taskbar, launcher and installer icon
    docs/cairn.png                 the README logo

Run with the development interpreter from the repository root:

    .venv\\Scripts\\python.exe scripts\\make_icon.py

The files are drawn, never downloaded; `ui/tests/test_brand.py` checks that the committed
icon equals a fresh render.
"""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ICON = ROOT / "ui" / "optimizer" / "assets" / "cairn.ico"
LOGO = ROOT / "docs" / "cairn.png"


def main() -> int:
    sys.path.insert(0, str(ROOT / "ui"))
    from optimizer.widgets.brand import write_icon, write_logo

    ICON.parent.mkdir(parents=True, exist_ok=True)
    LOGO.parent.mkdir(parents=True, exist_ok=True)
    write_icon(ICON)
    write_logo(LOGO)
    for path in (ICON, LOGO):
        print(f"wrote {path.relative_to(ROOT)} ({path.stat().st_size:,} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
