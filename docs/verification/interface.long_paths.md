# interface.long_paths

| Field | Value |
|---|---|
| Source | https://learn.microsoft.com/en-us/windows/win32/fileio/maximum-file-path-limitation |
| Mechanism | setting |
| Observable effect | A long-path-aware program can create a file in a folder path of more than 260 characters (scripts/verification/long_path_test.py exits 0). |
| Restart | Windows restart |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the test program failed at the baseline, wrote a file in a 357-character folder path after the Windows restart, and failed again after Undo (a new process reads the value, so no further restart was needed for the test); Undo exact (the value back to 0, which existed before) |
