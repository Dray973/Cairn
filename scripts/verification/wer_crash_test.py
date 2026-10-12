"""Checks whether Windows Error Reporting records a crash of a program on this PC.

Starts a child Python process that ends itself with RaiseFailFastException, which bypasses every
exception handler and hands the crash to Windows Error Reporting, then watches the Windows Error
Reporting folders of the signed-in user and of the machine for up to 60 seconds:

    %LOCALAPPDATA%\\Microsoft\\Windows\\WER\\ReportArchive and ReportQueue
    %ProgramData%\\Microsoft\\Windows\\WER\\ReportArchive and ReportQueue

and prints the report folders that appeared. Run it before applying the error reporting tweak
and again after applying it: a new report before and none after shows that the setting stops
Windows Error Reporting from recording crashes.

    py -3.12 scripts\\verification\\wer_crash_test.py          crash the child and watch
    py -3.12 scripts\\verification\\wer_crash_test.py --list   only count the reports there are

It changes no setting. The crash leaves a report in those folders whenever Windows Error
Reporting is on, as any crashing program does. Exit code 0: a new report appeared; 1: none
appeared; 2: the child did not crash.
"""

from __future__ import annotations

import os
import subprocess
import sys
import time
from pathlib import Path

WAIT_SECONDS = 60
CRASH = "import ctypes; ctypes.windll.kernel32.RaiseFailFastException(None, None, 0)"
# A process ended by an exception exits with its NTSTATUS error code (0xC0000602 for a fail-fast
# exception); every error code has both top bits set.
NTSTATUS_ERROR = 0xC0000000


def report_folders() -> list[Path]:
    roots = [os.environ.get("LOCALAPPDATA", ""), os.environ.get("ProgramData", "")]
    folders = []
    for root in roots:
        if not root:
            continue
        for name in ("ReportArchive", "ReportQueue"):
            folders.append(Path(root) / "Microsoft" / "Windows" / "WER" / name)
    return folders


def reports() -> set[Path]:
    found: set[Path] = set()
    for folder in report_folders():
        try:
            found.update(entry for entry in folder.iterdir() if entry.is_dir())
        except OSError:
            continue
    return found


def main(argv: list[str]) -> int:
    before = reports()
    if "--list" in argv:
        for folder in report_folders():
            count = sum(1 for report in before if report.parent == folder)
            print(f"{folder}: {count} report folders")
        return 0
    child = subprocess.run([sys.executable, "-c", CRASH], check=False)
    code = child.returncode & 0xFFFFFFFF
    if code & NTSTATUS_ERROR != NTSTATUS_ERROR:
        print(f"the child did not crash as expected (exit code {code:#x})")
        return 2
    print("the child crashed; watching the Windows Error Reporting folders")
    deadline = time.monotonic() + WAIT_SECONDS
    while time.monotonic() < deadline:
        new = sorted(reports() - before)
        if new:
            for report in new:
                print(f"new report: {report.parent.name}\\{report.name}")
            return 0
        time.sleep(1)
    print(f"no new report within {WAIT_SECONDS} seconds")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
