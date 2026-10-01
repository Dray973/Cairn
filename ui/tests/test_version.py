"""One version for the whole app: the Rust workspace, the Python package and its project file
carry the same one, and the deployed engine reports it."""

from __future__ import annotations

import tomllib
from pathlib import Path

import pytest

from optimizer import APP_NAME, __version__
from optimizer.bridge.engine import EngineUnavailable, load_engine_module
from optimizer.widgets import brand

ROOT = Path(__file__).resolve().parents[2]


def cargo_version() -> str:
    with (ROOT / "Cargo.toml").open("rb") as f:
        return str(tomllib.load(f)["workspace"]["package"]["version"])


def pyproject_version() -> str:
    with (ROOT / "ui" / "pyproject.toml").open("rb") as f:
        return str(tomllib.load(f)["project"]["version"])


def test_every_place_carries_the_same_version() -> None:
    assert cargo_version() == pyproject_version() == __version__
    parts = __version__.split(".")
    assert len(parts) == 3 and all(p.isdigit() for p in parts), __version__


def test_the_name_is_cairn_everywhere() -> None:
    assert APP_NAME == "Cairn"
    assert brand.NAME == APP_NAME


def test_the_deployed_engine_reports_the_app_version() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    engine = str(module.version())
    assert engine == __version__
