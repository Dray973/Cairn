# Cairn profile format

A profile is a small JSON file that lists Cairn settings: catalog tweaks, Store apps to remove,
startup apps to turn off, a DNS preset per adapter kind, Windows Update choices and a scheduled
maintenance plan. You can export one from a PC, open it on another, see exactly what it would
change there and apply the rows you choose. Every change a profile makes goes through the same
journaled changes as the rest of Cairn, so it can be undone from the Profiles section
("Undo these changes"), from History or with Revert All Changes.

A profile never names a registry path, service, scheduled task, file path, command, URL or DNS
server address. It only holds ids that Cairn compares with what it knows (its tweak catalog, its
list of removable Store apps, its DNS presets) and with what is listed on the PC (startup
entries, network adapters). Everything Cairn writes comes from those, never from a string in the
file.

## Example

This is what "Export this PC's settings…" writes (pretty JSON, two-space indent, fields in this
order, a line feed at the end):

```json
{
  "format": "cairn.profile",
  "schema": 1,
  "name": "Gaming PC",
  "description": "My desk PC settings",
  "created": "2026-09-28",
  "created_with": "Cairn 0.2.0",
  "tweaks": ["gaming.game_mode", "privacy.advertising_id"],
  "apps": ["Microsoft.BingNews", "king.com.*"],
  "startup": [
    {"id": "user_run:Discord", "name": "Discord"},
    {"id": "packaged_task:Microsoft.WindowsTerminal_8wekyb3d8bbwe\\StartTerminalOnLoginTask", "name": "Terminal"}
  ],
  "dns": {"ethernet": {"ipv4": "cloudflare", "ipv6": "cloudflare"}, "wifi": {"ipv4": "quad9"}},
  "windows_update": {"active_hours": {"start": 8, "end": 23}, "exclude_drivers": true},
  "maintenance": {"enabled": true, "day": "sunday", "time": "12:00", "clean": ["user_temp"], "sfc_verify": true, "dism_check": true}
}
```

The smallest valid profile names itself and holds one setting:

```json
{"format": "cairn.profile", "schema": 1, "name": "Game Mode", "tweaks": ["gaming.game_mode"]}
```

## Reading rules

- The file is UTF-8 text (a byte order mark is allowed and ignored) of at most 256 KB. UTF-16
  files are refused with a hint to save them as UTF-8.
- It is one JSON object. Every object in it refuses unknown fields and duplicate fields, and
  every field must have the type below. Any such problem rejects the whole file, with the line
  and column where JSON is concerned.
- `format` must be `"cairn.profile"`. `schema` must be a whole number: 1 is read; a larger number
  means the file was written by a newer Cairn, and this version asks you to update instead of
  guessing; 0, negative, fractional or quoted numbers are invalid.
- After the types, every string is checked against the grammar below, trimmed and deduplicated.
  A well-formed id that this Cairn does not know (a tweak added in a newer version, an unknown
  DNS preset, a startup entry that is not on this PC) does not reject the file: it becomes a
  skipped row with the reason, so the rest of the profile still applies.

## Top-level fields

| Field | Required | Rule |
|---|---|---|
| `format` | yes | Exactly `"cairn.profile"`. |
| `schema` | yes | `1`. |
| `name` | yes | 1 to 60 characters after trimming, no control characters. Shown in Cairn, in the journal session label (`profile: <name>`) and in the activity log. |
| `description` | no | At most 300 characters, no control characters. |
| `created` | no | A date written `YYYY-MM-DD`. Informational. |
| `created_with` | no | At most 40 characters, no control characters, for example `"Cairn 0.2.0"`. Informational. |
| `tweaks`, `apps`, `startup`, `dns`, `windows_update`, `maintenance` | at least one | The sections below. A profile without any setting is refused. |

## Sections

### `tweaks`

A list of at most 256 catalog tweak ids, such as `"gaming.game_mode"`. An id is a lowercase
letter followed by lowercase letters or digits, a dot, then 1 to 60 lowercase letters, digits or
underscores (64 characters at most). Duplicates are dropped. The Optimize section shows every id
Cairn knows.

### `apps`

A list of at most 128 Store package Names to remove for the signed-in account, such as
`"Microsoft.BingNews"`. A Name starts with a letter or digit and continues with letters, digits,
dots or dashes (2 to 50 characters). It may end with `*` to name a pattern, such as
`"king.com.*"`; a pattern is used only when it is exactly one of the patterns in Cairn's own list
of removable apps. Names are compared ignoring ASCII case, and a Name Cairn does not remove, or
never removes (the Store and other protected apps), is a skipped row.

### `startup`

A list of at most 256 startup entries to turn off, each `{"id": "<source>:<key>", "name": "…"}`.
The source is a lowercase letter followed by up to 31 lowercase letters, digits or underscores
(Cairn lists `user_run`, `machine_run`, `machine_run32`, `user_folder`, `common_folder` and
`packaged_task`); the key is 1 to 260 characters without control characters. `name` (optional,
at most 120 characters) is shown only when the entry is not on the PC. An entry matches only a
startup entry that is listed on the PC with the same id, ignoring ASCII case; a program listed in
a related place (another Run key, or the other Startup folder) matches when exactly one such
entry exists. Entries set by Group Policy are never changed.

### `dns`

`{"ethernet": {...}, "wifi": {...}}`, each optional. Each names a DNS preset per address family,
`{"ipv4": "<preset>", "ipv6": "<preset>"}`, and needs at least one family; a family left out
keeps its current setting. A preset id is 1 to 32 lowercase letters, digits or underscores; this
version offers `automatic`, `cloudflare`, `cloudflare_security`, `cloudflare_family`, `google`,
`quad9` and `quad9_unfiltered`. The choice applies to every hardware adapter of that kind on the
PC, each as its own row. Adapters that DNS changes cannot reach (a connected VPN, servers set for
one Wi-Fi network, Group Policy) are skipped rows.

### `windows_update`

At least one of:

| Field | Rule |
|---|---|
| `active_hours` | `{"automatic": true}`, or `{"start": S, "end": E}` with hours 0 to 23, `S` different from `E`, at most 18 hours apart. |
| `restart_notify` | `true` or `false`: whether Windows shows restart notifications. |
| `exclude_drivers` | `true` leaves drivers out of quality updates; `false` (or leaving it out) changes nothing. |
| `defer_feature_days` | 1 to 365 days to defer feature updates. Not available on every edition of Windows; where it is, the row starts unselected with a caution. |

Pausing updates is never part of a profile.

### `maintenance`

Turns on scheduled maintenance (or saves its plan again):

| Field | Rule |
|---|---|
| `enabled` | `true`. A profile cannot turn scheduled maintenance off; `false` is accepted only without any other field and is shown as a skipped row. |
| `day` | `monday` … `sunday`. |
| `time` | `HH:MM`, 00:00 to 23:59. |
| `clean` | At most 16 cleanup target ids (1 to 32 lowercase letters or underscores), such as `user_temp`. |
| `sfc_verify`, `dism_check` | `true` adds the read-only system file check or component store check. |

At least one of `clean`, `sfc_verify` or `dism_check` is needed. Because each run permanently
deletes the files in the chosen locations, this row always starts unselected with a caution.
When scheduled maintenance is already on, the row changes its plan, and "Undo these changes"
keeps the changed schedule: turn it off in Maintenance or History to remove it.

## Previewing and applying

Opening a profile changes nothing. Cairn reads only what the profile's sections need and shows
one row per setting: **will change**, **already set**, or **skipped** with its reason (not on this
PC, unknown to this version, can't be changed here, belongs to another account, not on this
edition, couldn't be read). Change rows start checked, except rows with a caution: High-risk
tweaks, the Ultimate Performance power plan on a PC with a battery, scheduled maintenance and
deferring feature updates. A row whose setting Cairn had already changed in an earlier session
says so, because undoing it returns the setting to how it was before Cairn first changed it.

Applying needs administrator rights. The chosen rows are planned again, then applied in one
journal session labelled `profile: <name>`, in this order: tweaks, startup apps, DNS, Windows
Update, scheduled maintenance, Store apps. Every setting's current value is recorded before it
changes. The activity log gets an `apply_profile` row marked `started` before the first change
and one final row with the counts. A row that fails does not stop the others, and what it changed
before it failed is recorded, so "Undo these changes" undoes that too. When Cairn runs as a
different account than the signed-in user, settings that belong to a user account (per-user
tweaks, Store apps, per-user startup entries) are skipped.

## Exporting

"Export this PC's settings…" (or `optctl profile export`) offers the settings Cairn manages on
this PC: applied tweaks, Store apps Cairn removed, startup apps that are turned off (except those
set by Group Policy), the DNS preset of each adapter kind, and the Windows Update and scheduled
maintenance choices Cairn set. You choose the rows and a name. The file never holds the computer
name, a user name, a network adapter's name or GUID, a package version or full name, or a custom
DNS server address: custom DNS servers are left out, with a note. So are startup apps whose name
ends in an identifier of the PC or account (an underscore and 16 or more hex digits), such as a
browser's auto-start entry, which is named after a hash of the browser's profile folder
(`MicrosoftEdgeAutoLaunch_` and 32 hex digits).

## Versions

Cairn writes the lowest schema that can express the content, so a profile opens in as many
versions of Cairn as possible; today that is always schema 1. A reader accepts every schema up to
its own and refuses a higher one with a request to update Cairn. New ids inside an existing
section (a new tweak, a new DNS preset) need no new schema: older versions show them as skipped
rows.

## Starter profiles

Cairn has three built-in profiles that hold only Low-risk catalog tweaks and Store apps:
**Gaming**, **Privacy** and **Clean**. They have no startup, DNS, Windows Update or maintenance
section, and they are previewed like any other profile.

## Command line

```text
optctl profile starters
optctl profile show starter:gaming
optctl profile plan "C:\Users\Test\Documents\Gaming PC.json"
optctl profile apply starter:gaming --yes
optctl profile apply "Gaming PC.json" --only tweak:gaming.game_mode,startup:user_run:Discord --yes
optctl profile export --name "Gaming PC" --out "Gaming PC.json"
```

`show` and `plan` only read. `apply` without `--yes` prints the plan and applies nothing;
`apply --all` also applies the rows that start unselected, and `--json` prints the report with
the undo filter. `export` without `--out` prints the rows it would save and writes nothing.
