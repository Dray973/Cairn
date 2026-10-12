"""Checks whether a long-path-aware program can use file paths longer than 260 characters.

Creates nested folders and a file under %LOCALAPPDATA%\\PCOptimizerDev\\lp-test whose folder
path is more than 300 characters long, prints the length when that works and the error when it
does not, and always removes the test folder again. python.exe declares itself long-path aware,
so the result follows Windows' LongPathsEnabled setting as it was when this process started:
run it in a new console after each change.

    py -3.12 scripts\\verification\\long_path_test.py

Exit code 0: long paths work; 1: they do not; 2: the test folder could not be removed.
"""

from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path

# Six levels of 50 characters: well over the 260-character limit.
LEVELS = 6
NAME = "d" * 50


def work_folder() -> Path:
    return Path(os.environ["LOCALAPPDATA"]) / "PCOptimizerDev" / "lp-test"


def extended(path: Path) -> str:
    r"""The \\?\ form of an absolute path, which every Windows file API accepts at any length."""
    return "\\\\?\\" + str(path)


def main() -> int:
    base = work_folder()
    deep = base.joinpath(*([NAME] * LEVELS))
    status = 1
    try:
        deep.mkdir(parents=True, exist_ok=True)
        (deep / "f.txt").write_text("x", encoding="ascii")
        print(f"long paths work: wrote a file in a folder path of {len(str(deep))} characters")
        status = 0
    except OSError as e:
        print(f"long paths do not work: {e!r}")
    finally:
        # The cleanup uses the extended form, so it removes what was created whatever the
        # setting is now.
        try:
            if base.exists():
                shutil.rmtree(extended(base))
        except OSError as e:
            print(f"could not remove {base}: {e!r}", file=sys.stderr)
            status = 2
    return status


if __name__ == "__main__":
    sys.exit(main())
