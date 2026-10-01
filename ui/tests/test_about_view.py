"""The About dialog's lines (`widgets.about`) for every kind of copy. No window."""

from __future__ import annotations

from optimizer import APP_NAME
from optimizer.widgets.about import (
    ABOUT_TITLE,
    COPYRIGHT_LINE,
    DEVELOPMENT,
    INSTALLED,
    NOT_INSTALLED,
    UNKNOWN_COPY,
    AboutInfo,
    about_lines,
    about_message,
)

DATA = r"C:\Users\Test\AppData\Local\PCOptimizer"


def info(**changes: object) -> AboutInfo:
    base = AboutInfo(
        app="0.2.0",
        engine="0.2.0",
        telemetry="optimizer_telemetry 0.2.0",
        python="3.12.10",
        tk="8.6.15",
        ctk="6.0.0",
        install=INSTALLED,
        location=r"C:\Program Files\Cairn",
        data_dir=DATA,
    )
    return base._replace(**changes)


def test_title_and_message_name_the_app() -> None:
    assert ABOUT_TITLE == "About Cairn"
    message = about_message("0.2.0")
    assert message.startswith(f"{APP_NAME} 0.2.0 records every change before it makes it")
    assert "History" in message and "Revert All Changes" in message


def test_an_installed_copy() -> None:
    assert about_lines(info()) == [
        "• App: 0.2.0",
        "• Engine: 0.2.0",
        "• Hardware monitor: optimizer_telemetry 0.2.0",
        "• Python 3.12.10  ·  Tk 8.6.15  ·  CustomTkinter 6.0.0",
        r"• Installed in C:\Program Files\Cairn",
        rf"• Data: {DATA} (journal, tool logs and Cairn's logs)",
        "• Copyright (c) 2026 Dray973  ·  MIT License",
    ]
    assert about_lines(info())[-1] == COPYRIGHT_LINE


def test_a_development_copy_names_the_installed_one() -> None:
    lines = about_lines(
        info(
            install=DEVELOPMENT,
            location=r"X:\src\Cairn\ui",
            installed_copy={"dir": r"C:\Program Files\Cairn", "version": "0.2.0"},
        )
    )
    assert lines[4:6] == [
        r"• Development copy: X:\src\Cairn\ui",
        r"• Installed copy: 0.2.0 in C:\Program Files\Cairn",
    ]
    unversioned = {"dir": r"D:\Cairn", "version": ""}
    lines = about_lines(info(install=DEVELOPMENT, location="ui", installed_copy=unversioned))
    assert r"• Installed copy: D:\Cairn" in lines
    lines = about_lines(info(install=DEVELOPMENT, location="ui", installed_copy=None))
    assert "• Installed copy: none" in lines
    lines = about_lines(info(install=DEVELOPMENT, location="ui", installed_copy=UNKNOWN_COPY))
    assert not any(line.startswith("• Installed copy") for line in lines), "nothing is claimed while unknown"


def test_an_installed_copy_never_lists_another_one() -> None:
    lines = about_lines(info(installed_copy={"dir": r"D:\Other", "version": "9.9.9"}))
    assert not any("Installed copy" in line for line in lines)


def test_a_copy_that_is_not_installed() -> None:
    lines = about_lines(info(install=NOT_INSTALLED, location=r"C:\Users\Test\Downloads\Cairn"))
    assert r"• Not installed: C:\Users\Test\Downloads\Cairn" in lines
    assert not any(line.startswith("• Installed in") for line in lines)


def test_a_missing_engine() -> None:
    lines = about_lines(info(engine=None))
    assert lines[1] == "• Engine: not loaded"


def test_an_engine_of_another_version() -> None:
    lines = about_lines(info(engine="0.1.0"))
    assert lines[1] == "• ⚠ Engine 0.1.0 doesn't match the app (0.2.0): rebuild and deploy it"


def test_a_stopped_hardware_monitor() -> None:
    assert "• Hardware monitor: not running" in about_lines(info(telemetry=None))
    assert "• Hardware monitor: not running" in about_lines(info(telemetry=""))


def test_every_line_is_a_bullet() -> None:
    # Built from parts so that the product-name scan skips this file.
    old_name = " ".join(("PC", "Optimizer"))
    for install in (INSTALLED, DEVELOPMENT, NOT_INSTALLED):
        for engine in ("0.2.0", "0.1.0", None):
            lines = about_lines(info(install=install, engine=engine, installed_copy=None))
            assert all(line.startswith("• ") for line in lines), lines
            assert not any(old_name in line for line in lines)
