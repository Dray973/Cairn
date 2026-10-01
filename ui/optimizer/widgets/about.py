"""The About dialog's facts: the versions of every part, where this copy runs from and where
it keeps its data. Pure: the window gathers an `AboutInfo` and shows `about_lines`."""

from __future__ import annotations

from typing import Any, NamedTuple

from .. import APP_NAME

INSTALLED = "installed"
DEVELOPMENT = "development"
NOT_INSTALLED = "not_installed"

ABOUT_TITLE = f"About {APP_NAME}"
COPYRIGHT_LINE = "• Copyright (c) 2026 Dray973  ·  MIT License"
OPEN_DATA_TEXT = "Open data folder"
PROJECT_PAGE_TEXT = "Project page"
NO_DATA_YET_TEXT = f"{APP_NAME} hasn't stored anything yet."
# `AboutInfo.installed_copy` when nothing is known about an installed copy.
UNKNOWN_COPY = "unknown"


class AboutInfo(NamedTuple):
    app: str
    engine: str | None  # None: the engine is not loaded
    telemetry: str | None  # None: the hardware monitor is not running
    python: str
    tk: str
    ctk: str
    install: str  # INSTALLED | DEVELOPMENT | NOT_INSTALLED
    location: str  # the install folder, or the ui folder of a development copy
    data_dir: str
    # For a development copy: the installed copy as `install_info` describes it, None when
    # Cairn is not installed, UNKNOWN_COPY when that was not read.
    installed_copy: dict[str, Any] | str | None = UNKNOWN_COPY


def about_message(version: str) -> str:
    return (
        f"{APP_NAME} {version} records every change before it makes it, so each one can be undone "
        "from History or with Revert All Changes."
    )


def about_lines(info: AboutInfo) -> list[str]:
    """The detail lines of the About dialog."""
    lines = [f"• App: {info.app}"]
    if info.engine is None:
        lines.append("• Engine: not loaded")
    elif info.engine != info.app:
        lines.append(f"• ⚠ Engine {info.engine} doesn't match the app ({info.app}): rebuild and deploy it")
    else:
        lines.append(f"• Engine: {info.engine}")
    lines.append(
        f"• Hardware monitor: {info.telemetry}" if info.telemetry else "• Hardware monitor: not running"
    )
    lines.append(f"• Python {info.python}  ·  Tk {info.tk}  ·  CustomTkinter {info.ctk}")
    if info.install == INSTALLED:
        lines.append(f"• Installed in {info.location}")
    elif info.install == NOT_INSTALLED:
        lines.append(f"• Not installed: {info.location}")
    else:
        lines.append(f"• Development copy: {info.location}")
        copy = info.installed_copy
        if isinstance(copy, dict):
            version = str(copy.get("version") or "")
            where = str(copy.get("dir") or "")
            lines.append(
                f"• Installed copy: {version} in {where}" if version else f"• Installed copy: {where}"
            )
        elif copy is None:
            lines.append("• Installed copy: none")
    lines.append(f"• Data: {info.data_dir} (journal, tool logs and {APP_NAME}'s logs)")
    lines.append(COPYRIGHT_LINE)
    return lines
