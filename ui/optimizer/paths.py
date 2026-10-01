"""Per-user data folder of Cairn, as the engine finds it: the journal, tool logs and Cairn's own
logs live there. Nothing here creates a folder."""

from __future__ import annotations

import os
from collections.abc import Mapping
from pathlib import Path

DATA_DIR_VARIABLE = "OPTIMIZER_DATA_DIR"
# The folder keeps its original name: renaming it would need a migration for every account.
DATA_FOLDER_NAME = "PCOptimizer"
LOG_FOLDER_NAME = "logs"


def data_dir(env: Mapping[str, str] = os.environ) -> Path:
    """`OPTIMIZER_DATA_DIR` when set, else `%LOCALAPPDATA%\\PCOptimizer`; the parent of the
    journal the engine opens (`state_log::data_dir`)."""
    if DATA_DIR_VARIABLE in env:
        return Path(env[DATA_DIR_VARIABLE])
    return Path(env.get("LOCALAPPDATA") or ".") / DATA_FOLDER_NAME


def log_dir(env: Mapping[str, str] = os.environ) -> Path:
    """Folder of Cairn's own logs (`cairn.log`, `native.log`)."""
    return data_dir(env) / LOG_FOLDER_NAME
