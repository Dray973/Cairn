# privacy.search_history

| Field | Value |
|---|---|
| Source | https://support.microsoft.com/en-us/windows/windows-search-and-privacy-99fb8251-7260-1cd6-1bbb-15c2370eb168 |
| Mechanism | setting |
| Observable effect | 'Search history' in Settings › Privacy & security › Search reads Off, and a word searched afterwards is not listed under Recent in the search home. |
| Restart | none |
| Range | every version and edition |

## Checks

| Date | Version | Edition | Build | Result | Evidence | Kept after restart | Undo exact | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | the switch Off at once; after the sign-out a never-searched word was not listed under Recent; kept after the sign-out the batch used (no Windows restart in this batch); after Undo a new search was listed under Recent again; Undo exact (snapshot) |
