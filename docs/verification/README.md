# Verification records

Every tweak Cairn offers has a record here, `<id>.md`, with the evidence that it does what its description says on a real PC: the Microsoft page it rests on, the effect someone looked for, and every check of that effect, including whether it survived the restart the check used (see **Kept after restart** below) and whether Undo put the registry back exactly. A tweak whose effect was looked for and not found is not offered: a new tweak is left out, and a tweak of an earlier version is retired, so the changes it made still undo. A tweak that no available PC can run as offered (it needs hardware, a Windows version or an Office edition none of them has) ships with a `not-offered` check and a `documented` check that names what was missing; its record repeats what Microsoft documents. A check whose effect a PC could not show for another reason (the feature is absent there, already in the tweak's state, or has no control on that Windows version), or that did not make the step that shows the effect (Turn off Fast Startup acts only on a shutdown, and its check made none), is recorded as `documented` too, with the reason in its Notes, and still shows that the values were set, kept after a restart and undone exactly. Turn on Game Mode is the exception: Game Mode was already on there, so Settings showed it On before the apply as well as after it; its record keeps that check as `works` with `ui-state` evidence and says so in its Notes. A tweak whose state every available PC already had could not be applied or undone there: its record holds a `documented` check with `n/a` in both of those columns and says what that state showed, and the test lists such tweaks in `ALREADY_IN_STATE`. A tweak whose effect no check observed (only `documented` checks) is offered but never recommended. A tweak whose check found an Undo that was not exact, because of a fault Cairn has fixed since, keeps that check (`Undo exact` is `no`) and is listed in `RECHECK` until a check of the fixed version shows an exact undo; release builds fail while the list names any tweak. When that later check made no restart, the tweak is listed in `UNDO_CHECKED_AGAIN` instead, and its record holds both checks: the first kept after the restart, the second undone exactly. A tweak of an earlier version whose change Windows no longer lets programs make is retired with a `broken` check.

The test `crates/core/tests/verification_records.rs` reads every record and fails when one is missing, malformed or does not support its tweak (the rules are below).

Records hold no personal data: no computer or account name, no e-mail address, no list of installed apps, no file path with a user name, no serial number, MAC or IP address, and no screenshots. Only `.md` files belong in this folder.

## Format

````markdown
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
| 2026-10-07 | 26H2 | Home | 26300.9457 | works | functional | yes | yes | a file written in a 357-character folder path after the restart; refused again after Undo |
````

- The heading is the tweak id.
- **Fields.** `Source` is required and may appear on several rows; at least one Source is an `https://` link, Microsoft's own page wherever one exists (Microsoft Learn, Microsoft Support, the Edge policy reference, the Office policy reference). A third-party page is acceptable only for a value Microsoft does not document, and then only next to a `works` check. `Mechanism` is one of `policy`, `setting`, `policy and setting`, `service`, `scheduled task`, `power scheme`. `Observable effect` names the Windows screen, app behaviour or command that shows the change. `Restart` is the restart the tweak asks for: `none`, `File Explorer`, `sign out`, `Windows restart` or, for Edge and Office, `app restart`; the Notes say when the effect appeared. `Range` repeats the tweak's Windows range and editions; Edge and Office tweaks name the app, and the Office tweaks the Office editions they are offered for, instead.
- **Checks.** One row per check, oldest first. No cell contains a `|`.

| Column | Values |
|---|---|
| Date | `YYYY-MM-DD` |
| Version | the Windows version, such as `26H2`; for Edge and Office tweaks the Notes also name the app version |
| Edition | `Home`, `Pro`, `Enterprise` or `Education` |
| Build | `CurrentBuild.UBR`, five digits, a dot and the revision, such as `26300.9457` |
| Result | see below |
| Evidence | `functional`, `ui-state` or `none` |
| Kept after restart | `yes`, `no` or `n/a` |
| Undo exact | `yes`, `no` or `n/a` |
| Notes | free text: what was seen, the restart that was needed, and, for a value that had nothing to observe while the tweak's other values did, which one |

**Result:**

| Result | Meaning |
|---|---|
| `works` | Every effect the description claims was observed. |
| `partial` | Some claimed effect was not observed. The tweak ships only after its description is limited to what was observed and a new check passes. |
| `no-effect` | The effect was looked for and nothing observable changed. A tweak of an earlier version is retired with this result; a new tweak is left out. |
| `broken` | The change failed, was not kept, or Undo did not restore it. |
| `documented` | Nothing was observed on the PCs available, because none of them has the hardware (a Copilot+ PC for Recall), the Windows version (Windows 11 23H2 or earlier for the legacy Copilot policy) or the Office edition (Microsoft 365 Apps for business or enterprise, or a volume-licensed Office, for the Office policies) the tweak needs, because the feature the tweak changes is absent there, already in the tweak's state, or has no control on that Windows version, or because the check did not make the step that shows the effect (Turn off Fast Startup acts only on a shutdown). The record repeats what Microsoft documents; when the PC could apply the tweak, the check still shows that the values were set, kept after a restart and undone exactly, and otherwise both of those columns are `n/a`. |
| `not-offered` | The scan shows the tweak as not available on this PC, with the right reason and text. |

**Evidence:**

- `functional`: the feature behaves differently. A menu, button, banner or notification is gone, a program refuses, a test program shows the change, or an API answers differently.
- `ui-state`: Windows' own interface reflects the value Cairn wrote. A Settings switch reads the new state or says some settings are managed by your organization, a dialog option is greyed, or `edge://policy` lists the policy with status OK.
- `none`: nothing to show (a `documented`, `not-offered` or `no-effect` check).

Reading a value back after writing it is never evidence: it shows that the value is set, not that Windows uses it. A switch flipped by hand in Settings only shows which value Settings writes.

A policy Microsoft documents only for Pro and higher, checked on Home with `ui-state` evidence only, is never recommended, and its description says: "Microsoft documents this setting for Windows 11 Pro and higher; on Home, Cairn could only confirm that Windows Settings shows it as managed."

**Kept after restart** is `yes` when the effect and every value were still there after the restart the check used, and the scan still showed the tweak as applied, and `n/a` when the check made no restart (a later check of a fixed undo, for example). Tweaks were checked in batches, each with one restart at the highest level a tweak of the batch needs: a Windows restart, a sign-out, a File Explorer restart or an app restart; the Notes name the restart that was made. **Undo exact** is `yes` when, after Undo, a snapshot of the tweak's registry values, keys, services and tasks equals the snapshot taken before the tweak was applied: values absent again or back to their old type and data, keys Cairn created removed, keys that existed before still there. Undo must also reverse the effect.

## How a check is done

1. Snapshot: `scripts\live_state_snapshot.ps1 -Out <file>` (it reads every catalog target listed in `scripts\verification\targets.json`). Note the version, edition and build, and look at the effect the record names.
2. Apply the tweak in Cairn. Before doing the restart the tweak asks for, look once: an effect that is already there means the tweak needs less.
3. Do the restart the tweak asks for and observe the effect.
4. Restart Windows and observe again. The tweak still shows as applied, and History lists no change for it. A batch of tweaks that need less than a Windows restart may use the restart its highest level needs instead; the Notes say which restart was made.
5. Undo it in History, do the same restart, and check that the effect is gone.
6. Snapshot again and compare with the first one for the tweak's targets.

A check whose effect the PC cannot show (the feature is missing, absent or already in the tweak's state), or that does not make the step that shows it (such as the shutdown Turn off Fast Startup acts on), still does steps 4 to 6 and records `documented`, with what was missing in its Notes. Edge tweaks are checked in `edge://policy` too (Reload policies, or `edge://restart` when the description says "Applies after Edge restarts"): every value of the tweak with source Platform, level Mandatory and status OK. The scripts in `scripts/verification` help with the checks that need a test program; their README says how.

The checks were made on Windows 11 Home 26H2, build 26300. That version shares its servicing family (26100) and its cumulative updates with 24H2 and 25H2, so a record stands for those versions too unless a check on another version is listed; Pro, Enterprise and Education were not checked separately.

## Rules the test enforces

1. Every tweak id has a record unless it is listed in `PENDING`; an id in `PENDING` has no record, and no record names an id outside the catalog. With `CAIRN_RELEASE=1` in the environment, `PENDING` and `RECHECK` must be empty: release builds set it, and the failure lists the ids still pending or waiting for a new check.
2. The heading equals the id, and at least one Source starts with `https://`.
3. A tweak that is offered has a check with `Kept after restart` and `Undo exact` both `yes`, made on a build inside its Windows range and on an edition it is offered on: a `works` check, or a `documented` check when no available PC could show the effect of the values it set. The exceptions are a tweak no available PC can run as offered (listed in `NO_MACHINE_IN_RANGE`), which needs a `not-offered` check on build 26100 or later plus a `documented` check and is never recommended; a tweak whose state every available PC already had (listed in `ALREADY_IN_STATE`), which needs a `documented` check of that state made on such a build and edition and is never recommended; a tweak waiting for a check of a fixed undo (listed in `RECHECK`), whose `works` check may have `Undo exact` `no` and which leaves the list once a `works` check shows an exact undo; and a tweak whose undo was checked again on the fixed version without a restart (listed in `UNDO_CHECKED_AGAIN`), which needs a `works` check kept after the restart and a `works` check no older than it with an exact undo, and leaves the list once one `works` check shows both.
4. A recommended tweak has a `works` check as rule 3 describes, not only a `documented` one, and every `works` check of a recommended tweak has Evidence `functional` or `ui-state`.
5. A retired tweak has a `no-effect` or `not-offered` check, or a `broken` one when Windows no longer lets programs make its change.
6. Dates match `YYYY-MM-DD`, builds match five digits, a dot and a number, and Result, Evidence, Kept after restart and Undo exact use the values above.
