"""Profiles part of FakeEngine: the `profile_*` functions.

Mixed into `FakeEngine`, which calls `_init_profiles` from its constructor. Nothing is read
from or written to disk: files come from the `profile_files` option and exports are kept in
`exported` (path -> text). Plans read the fake's own state (tweaks, the Store app, startup
entries, adapters), and applying changes that state the way the engine's mutators would, so
the fake's `revert_targets` undoes it with the report's `undo` filter. Store app removals are
kept in `_profile_removed_apps` and are not part of the fake journal.

Options (`FakeEngine(**options)`):

- `profile_files`: normalized path -> text that `profile_read` can read; any other path
  raises RuntimeError("The file could not be read: not found");
- `profile_invalid`: `profile_read` and `profile_check` raise ValueError with this text;
- `profile_apply_error`: a real (not dry-run) `profile_apply` raises RuntimeError with it;
- `profile_fail`: row keys whose apply outcome is "failed";
- `profile_candidates_error`: `profile_candidates` raises RuntimeError with this text;
- `profile_extra_rows`: plan rows added as they are after the DNS rows, for Windows Update
  and scheduled maintenance rows; applying one reports it applied;
- `profile_restarts`: tweak id -> restart need ("explorer", "sign_out", "restart");
- `other_account`: the plan's `other_account` text; per-user rows are then skipped.
"""

from __future__ import annotations

import copy
import json
import os
from collections.abc import Iterable, Mapping
from typing import TYPE_CHECKING, Any

FORMAT = "cairn.profile"
SCHEMA = 1
ELEVATION_ERROR = "this operation requires an elevated (Administrator) process"
NOT_FOUND = "The file could not be read: not found"
OTHER_ACCOUNT_ROW = "Belongs to your user account, but Cairn is running as another account."
OTHER_ACCOUNT_CAUTION = "Read from the account Cairn runs as, not yours."
NOT_IN_PLAN = "Not part of this profile's plan on this PC."
NOT_INSTALLED = "Not installed for your account."
NOTHING_SELECTED = "Nothing was selected, so no profile was written."
DNS_POLICY_TEXT = "DNS servers are set by Group Policy on this PC, so adapter settings have no effect."
STORE_APP = "Microsoft.BingNews"
STORE_APP_TITLE = "Microsoft News"
STORE_APP_FAMILY = "Microsoft.BingNews_8wekyb3d8bbwe"
# Sources whose startup entries belong to the signed-in user.
PER_USER_SOURCES = ("user_run", "user_folder", "packaged_task")
RESTART_ORDER = ("none", "explorer", "sign_out", "restart")
REASON_TEXT = {
    "unsupported": "Not supported by this version",
    "unreadable": "Couldn't be read",
    "cannot_change": "Can't be changed here",
    "other_account": "Belongs to another account",
    "edition": "Not on this edition of Windows",
    "not_on_this_pc": "Not on this PC",
    "unknown_id": "Unknown to this version of Cairn",
}
CATEGORY_LABELS = {
    "privacy": "Privacy",
    "gaming": "Gaming",
    "performance": "Performance",
    "interface": "Interface",
}
KIND_LABELS = {"ethernet": "Ethernet", "wifi": "Wi-Fi"}
FAMILY_LABELS = {"ipv4": "IPv4", "ipv6": "IPv6"}


def _profile(name: str, description: str, tweaks: list[str], apps: list[str]) -> dict[str, Any]:
    profile: dict[str, Any] = {"format": FORMAT, "schema": SCHEMA, "name": name, "description": description}
    if tweaks:
        profile["tweaks"] = tweaks
    if apps:
        profile["apps"] = apps
    return profile


def profile_text(profile: Mapping[str, Any]) -> str:
    """A profile as the engine writes it: pretty JSON with a trailing line feed."""
    return json.dumps(profile, indent=2, ensure_ascii=False) + "\n"


def counts_of(profile: Mapping[str, Any]) -> dict[str, int]:
    dns = profile.get("dns") or {}
    wu = profile.get("windows_update") or {}
    return {
        "tweaks": len(profile.get("tweaks") or []),
        "apps": len(profile.get("apps") or []),
        "startup": len(profile.get("startup") or []),
        "dns": len([k for k in ("ethernet", "wifi") if dns.get(k)]),
        "windows_update": len([k for k, v in wu.items() if v not in (None, False)]),
        "maintenance": 1 if profile.get("maintenance") else 0,
    }


# (id, profile) of the starters, with ids the fake knows.
FAKE_STARTER_PROFILES = (
    (
        "gaming",
        _profile(
            "Gaming",
            "Game Mode on and SysMain set to manual.",
            ["gaming.game_mode", "performance.sysmain"],
            [],
        ),
    ),
    (
        "privacy",
        _profile(
            "Privacy",
            "Activity history and Cortana off, and the news app removed.",
            ["privacy.activity_history", "privacy.cortana"],
            [STORE_APP],
        ),
    ),
    (
        "clean",
        _profile(
            "Clean",
            "File extensions shown and the news app removed.",
            ["interface.file_extensions"],
            [STORE_APP],
        ),
    ),
)
FAKE_STARTERS: tuple[dict[str, Any], ...] = tuple(
    {
        "id": starter_id,
        "name": profile["name"],
        "description": profile["description"],
        "counts": counts_of(profile),
        "text": profile_text(profile),
    }
    for starter_id, profile in FAKE_STARTER_PROFILES
)


def starter_text(starter_id: str) -> str:
    return next(s["text"] for s in FAKE_STARTERS if s["id"] == starter_id)


def _restart_max(values: Iterable[str]) -> str:
    best = 0
    for value in values:
        if value in RESTART_ORDER:
            best = max(best, RESTART_ORDER.index(value))
    return RESTART_ORDER[best]


def _row(
    key: str,
    section: str,
    title: str,
    status: str,
    detail: str,
    *,
    reason: str | None = None,
    per_user: bool = False,
    risk: str | None = None,
    restart: str = "none",
    caution: str | None = None,
) -> dict[str, Any]:
    return {
        "key": key,
        "section": section,
        "title": title,
        "status": status,
        "detail": detail,
        "reason": reason,
        "caution": caution,
        "risk": risk,
        "restart": restart,
        "per_user": per_user,
        "selected": status == "change" and caution is None,
    }


def _empty_filter() -> dict[str, Any]:
    return {
        "registry": [],
        "services": [],
        "appx_families": [],
        "power": False,
        "scheduled_tasks": [],
        "dns": [],
        "task_definitions": [],
    }


class FakeProfiles:
    """Profiles over the fake's own tweaks, Store app, startup entries and adapters."""

    if TYPE_CHECKING:
        elevated: bool
        applied: set[str]
        preset: set[str]
        tweaks: list[tuple[str, str, bool]]
        task_tweaks: dict[str, tuple[str, ...]]
        startup: list[Any]
        appx_inventory_error: str | None
        dns_policy: list[str]
        dns_fail: set[str]
        _startup_disabled: set[str]
        _static_dns: dict[tuple[str, str], list[str]]

        def _record(self, name: str, *args: Any) -> None: ...
        def _record_quick(self, name: str, *args: Any) -> None: ...
        def _state(self, item_id: str) -> str: ...
        def _items(self) -> list[dict[str, Any]]: ...
        def _startup_entries(self) -> list[dict[str, Any]]: ...
        def _startup_record(self, entry_id: str) -> tuple[str, str, str]: ...
        def _adapters(self) -> list[dict[str, Any]]: ...
        def _skip_reason(self, adapter: dict[str, Any], family: str, target: list[str]) -> str | None: ...
        def _record_dns(
            self, adapter: dict[str, Any], family: str, previous: list[str], target: list[str]
        ) -> bool: ...
        def _update_dns_target(self, guid: str, family: str, target: list[str]) -> None: ...

    def _init_profiles(self, options: dict[str, Any]) -> None:
        """Pops the profile options this fake understands from `options`."""
        files = options.pop("profile_files", None) or {}
        self.profile_files: dict[str, str] = {os.path.normpath(k): v for k, v in files.items()}
        self.profile_invalid: str | None = options.pop("profile_invalid", None)
        self.profile_apply_error: str | None = options.pop("profile_apply_error", None)
        self.profile_fail = set(options.pop("profile_fail", ()))
        self.profile_candidates_error: str | None = options.pop("profile_candidates_error", None)
        self.profile_extra_rows = [dict(r) for r in options.pop("profile_extra_rows", ())]
        self.profile_restarts: dict[str, str] = dict(options.pop("profile_restarts", None) or {})
        self.profile_other_account: str | None = options.pop("other_account", None)
        self.exported: dict[str, str] = {}
        self._profile_removed_apps: set[str] = set()

    # -- parsing ---------------------------------------------------------------------

    def _profile_parse(self, text: str) -> dict[str, Any]:
        """The profile in `text`; ValueError with a reason like the engine's."""
        if self.profile_invalid is not None:
            raise ValueError(self.profile_invalid)
        try:
            profile = json.loads(text)
        except json.JSONDecodeError as exc:
            raise ValueError(f"The file is not valid JSON: {exc}.") from None
        if not isinstance(profile, dict) or profile.get("format") != FORMAT:
            raise ValueError("This file is not a Cairn profile.")
        schema = profile.get("schema")
        if isinstance(schema, int) and not isinstance(schema, bool) and schema > SCHEMA:
            raise ValueError(
                f"This profile was made by a newer version of Cairn (profile format {schema}). "
                "Update Cairn to open it."
            )
        if schema != SCHEMA:
            raise ValueError('The profile is not valid: "schema" must be a whole number of at least 1.')
        name = profile.get("name")
        if not isinstance(name, str) or not name.strip():
            raise ValueError("The profile needs a name.")
        profile["name"] = name.strip()
        if not any(counts_of(profile).values()):
            raise ValueError("This profile contains no settings.")
        return profile

    @staticmethod
    def _profile_summary(profile: dict[str, Any]) -> dict[str, Any]:
        return {
            "name": profile["name"],
            "description": profile.get("description", ""),
            "created": profile.get("created"),
            "created_with": profile.get("created_with"),
            "counts": counts_of(profile),
            "text": profile_text(profile),
        }

    # -- planning --------------------------------------------------------------------

    def _profile_tweak_rows(self, profile: dict[str, Any]) -> list[dict[str, Any]]:
        known = {i: c for i, c, _ in self.tweaks}
        rows = []
        for tweak_id in profile.get("tweaks") or []:
            key = f"tweak:{tweak_id}"
            if tweak_id not in known:
                rows.append(
                    _row(
                        key,
                        "tweaks",
                        tweak_id,
                        "skipped",
                        "Cairn 0.0.0-test doesn't know this setting; a newer version may.",
                        reason="unknown_id",
                    )
                )
                continue
            title = tweak_id.split(".", 1)[1].replace("_", " ").title()
            per_user = tweak_id not in self.task_tweaks and not tweak_id.startswith("privacy.")
            restart = self.profile_restarts.get(tweak_id, "none")
            state = self._state(tweak_id)
            if state == "applied":
                detail = "Already applied" if tweak_id in self.applied else "Already set on this PC"
                row = _row(
                    key, "tweaks", title, "already", detail, per_user=per_user, risk="low", restart=restart
                )
            elif state == "unavailable":
                row = _row(
                    key,
                    "tweaks",
                    title,
                    "skipped",
                    "Not on this PC.",
                    reason="not_on_this_pc",
                    per_user=per_user,
                    risk="low",
                    restart=restart,
                )
            else:
                detail = f"{CATEGORY_LABELS.get(known[tweak_id], known[tweak_id])} · not applied"
                row = _row(
                    key, "tweaks", title, "change", detail, per_user=per_user, risk="low", restart=restart
                )
            rows.append(row)
        return rows

    def _profile_app_rows(self, profile: dict[str, Any]) -> list[dict[str, Any]]:
        rows = []
        for name in profile.get("apps") or []:
            key = f"app:{name}"
            if self.appx_inventory_error is not None:
                detail = f"The Store app list couldn't be read: {self.appx_inventory_error}"
                rows.append(_row(key, "apps", name, "skipped", detail, reason="unreadable", per_user=True))
            elif name.lower() != STORE_APP.lower():
                rows.append(_row(key, "apps", name, "already", NOT_INSTALLED, per_user=True, risk="low"))
            elif STORE_APP in self._profile_removed_apps:
                rows.append(
                    _row(
                        f"app:{STORE_APP}",
                        "apps",
                        STORE_APP_TITLE,
                        "already",
                        "Removed by Cairn",
                        per_user=True,
                    )
                )
            else:
                detail = f"Store app {STORE_APP} · removed for your account"
                rows.append(
                    _row(
                        f"app:{STORE_APP}",
                        "apps",
                        STORE_APP_TITLE,
                        "change",
                        detail,
                        per_user=True,
                        risk="low",
                    )
                )
        return rows

    def _profile_startup_rows(self, profile: dict[str, Any]) -> list[dict[str, Any]]:
        entries = self._startup_entries()
        rows = []
        for choice in profile.get("startup") or []:
            wanted = str(choice.get("id", ""))
            entry = next((e for e in entries if e["id"].lower() == wanted.lower()), None)
            if entry is None:
                title = choice.get("name") or wanted.split(":", 1)[-1]
                rows.append(
                    _row(
                        f"startup:{wanted}",
                        "startup",
                        title,
                        "skipped",
                        "Not listed on this PC.",
                        reason="not_on_this_pc",
                    )
                )
                continue
            key = f"startup:{entry['id']}"
            per_user = entry["source"] in PER_USER_SOURCES
            if not entry["can_toggle"]:
                row = _row(
                    key,
                    "startup",
                    entry["name"],
                    "skipped",
                    entry["note"],
                    reason="cannot_change",
                    per_user=per_user,
                )
            elif not entry["enabled"]:
                row = _row(key, "startup", entry["name"], "already", "Already turned off", per_user=per_user)
            else:
                detail = f"{entry['location']} · {entry['publisher']} · turned off at sign-in"
                row = _row(key, "startup", entry["name"], "change", detail, per_user=per_user)
            rows.append(row)
        return rows

    @staticmethod
    def _profile_preset_servers(preset_id: str, family: str) -> list[str] | None:
        from .fake_network import DNS_PRESETS

        preset = next((p for p in DNS_PRESETS if p["id"] == preset_id), None)
        return None if preset is None else list(preset[family])

    def _profile_dns_changes(
        self, adapter: dict[str, Any], families: Mapping[str, str]
    ) -> list[dict[str, Any]]:
        """Per family of `families`: its target servers and "change", "already" or "skipped"."""
        changes = []
        for family in ("ipv4", "ipv6"):
            preset_id = families.get(family)
            if preset_id is None:
                continue
            target = self._profile_preset_servers(preset_id, family) or []
            reason = self._skip_reason(adapter, family, target)
            current = self._static_dns.get((adapter["id"], family), [])
            if reason is not None:
                status = "skipped"
            elif current == target:
                status = "already"
            else:
                status = "change"
            changes.append({"family": family, "target": target, "status": status, "reason": reason})
        return changes

    def _profile_dns_rows(self, profile: dict[str, Any]) -> list[dict[str, Any]]:
        from .fake_network import DNS_PRESETS

        rows = []
        choices = profile.get("dns") or {}
        adapters = self._adapters()
        for kind in ("ethernet", "wifi"):
            families = choices.get(kind)
            if not families:
                continue
            label = KIND_LABELS[kind]
            candidates = [
                a for a in adapters if a["kind"] == kind and a["hardware"] and a["status"] != "not_present"
            ]
            if not candidates:
                rows.append(
                    _row(
                        f"dns:{kind}",
                        "dns",
                        f"DNS servers ({label})",
                        "skipped",
                        f"This PC has no {label} adapter.",
                        reason="not_on_this_pc",
                    )
                )
                continue
            known = {p["id"] for p in DNS_PRESETS}
            unknown = next((v for v in families.values() if v not in known), None)
            for adapter in candidates:
                key = f"dns:{adapter['id']}"
                title = f"DNS servers of {adapter['name']} ({label})"
                if self.dns_policy:
                    rows.append(_row(key, "dns", title, "skipped", DNS_POLICY_TEXT, reason="cannot_change"))
                elif unknown is not None:
                    offered = ", ".join(p["id"] for p in DNS_PRESETS)
                    detail = f"Unknown DNS choice “{unknown}”; this version offers: {offered}."
                    rows.append(_row(key, "dns", title, "skipped", detail, reason="unknown_id"))
                elif not adapter["can_change_dns"]:
                    rows.append(
                        _row(key, "dns", title, "skipped", adapter["note"] or "", reason="cannot_change")
                    )
                else:
                    changes = self._profile_dns_changes(adapter, families)
                    detail = " · ".join(
                        f"{FAMILY_LABELS[c['family']]}: {', '.join(c['target']) or 'Automatic'}"
                        for c in changes
                    )
                    statuses = {c["status"] for c in changes}
                    if "change" in statuses:
                        rows.append(_row(key, "dns", title, "change", detail))
                    elif "already" in statuses:
                        rows.append(_row(key, "dns", title, "already", detail))
                    else:
                        rows.append(_row(key, "dns", title, "skipped", detail, reason="cannot_change"))
        return rows

    def _profile_plan(self, profile: dict[str, Any]) -> dict[str, Any]:
        rows = (
            self._profile_tweak_rows(profile)
            + self._profile_startup_rows(profile)
            + self._profile_dns_rows(profile)
            + [dict(r) for r in self.profile_extra_rows]
            + self._profile_app_rows(profile)
        )
        other = self.profile_other_account
        if other is not None:
            for row in rows:
                if row["per_user"] and row["status"] != "skipped":
                    row.update(
                        status="skipped",
                        reason="other_account",
                        detail=OTHER_ACCOUNT_ROW,
                        caution=None,
                        selected=False,
                    )
        for row in rows:
            row["selected"] = row["status"] == "change" and not row.get("caution")
        return {
            "dry_run": True,
            "name": profile["name"],
            "rows": rows,
            "changes": len([r for r in rows if r["status"] == "change"]),
            "already": len([r for r in rows if r["status"] == "already"]),
            "skipped": len([r for r in rows if r["status"] == "skipped"]),
            "restart": _restart_max(r["restart"] for r in rows if r["selected"]),
            "elevated": self.elevated,
            "other_account": other,
            "warnings": [],
            "duration_ms": 9,
        }

    # -- applying --------------------------------------------------------------------

    def _profile_apply_row(
        self, row: dict[str, Any], profile: dict[str, Any], undo: dict[str, Any]
    ) -> dict[str, Any]:
        key = row["key"]
        result = {
            "key": key,
            "section": row["section"],
            "title": row["title"],
            "outcome": "applied",
            "details": [],
        }
        if key in self.profile_fail:
            result.update(outcome="failed", details=["Access is denied."])
            return result
        prefix, _, rest = key.partition(":")
        if prefix == "tweak":
            self.applied.add(rest)
            if rest in self.task_tweaks:
                undo["scheduled_tasks"] += list(self.task_tweaks[rest])
            else:
                undo["registry"].append({"hive": "HKLM", "key_path": "Test", "value_name": rest})
            result["details"] = [f"{rest} detail"]
        elif prefix == "startup":
            self._startup_disabled.add(rest)
            hive, key_path, value_name = self._startup_record(rest)
            undo["registry"].append({"hive": hive, "key_path": key_path, "value_name": value_name})
            result["details"] = ["Turned off at sign-in"]
        elif prefix == "dns":
            result.update(self._profile_apply_dns(rest, profile, undo))
        elif prefix == "app":
            self._profile_removed_apps.add(rest)
            undo["appx_families"].append(STORE_APP_FAMILY)
            result["details"] = ["Removed for your account"]
        return result

    def _profile_apply_dns(self, guid: str, profile: dict[str, Any], undo: dict[str, Any]) -> dict[str, Any]:
        adapter = next(a for a in self._adapters() if a["id"] == guid)
        families = (profile.get("dns") or {}).get(adapter["kind"]) or {}
        outcomes = []
        for change in self._profile_dns_changes(adapter, families):
            if change["status"] != "change":
                outcomes.append("already_set" if change["status"] == "already" else "skipped")
                continue
            family, target = change["family"], change["target"]
            key = (adapter["id"], family)
            current = self._static_dns.get(key, [])
            captured = self._record_dns(adapter, family, current, target)
            if family in self.dns_fail:
                outcomes.append("failed")
                continue
            self._static_dns[key] = list(target)
            if not captured:
                self._update_dns_target(adapter["id"], family, target)
            outcomes.append("applied")
        if "applied" in outcomes or "failed" in outcomes:
            undo["dns"].append(adapter["id"])
        if "failed" in outcomes:
            return {"outcome": "failed", "details": ["Access is denied."]}
        if "applied" in outcomes:
            return {"outcome": "applied", "details": ["DNS servers set"]}
        if outcomes and all(o == "already_set" for o in outcomes):
            return {"outcome": "already_set", "details": ["Already set"]}
        return {"outcome": "skipped", "details": ["Nothing to change"]}

    def _profile_apply(self, profile: dict[str, Any], keys: list[str] | None) -> dict[str, Any]:
        plan = self._profile_plan(profile)
        rows = plan["rows"]
        results: list[dict[str, Any]] = []
        chosen: list[dict[str, Any]] = []
        if keys is None:
            chosen = [r for r in rows if r["selected"]]
        else:
            seen: set[str] = set()
            for key in keys:
                if key.lower() in seen:
                    continue
                seen.add(key.lower())
                row = next((r for r in rows if r["key"].lower() == key.lower()), None)
                if row is None:
                    results.append(
                        {
                            "key": key,
                            "section": "tweaks",
                            "title": key,
                            "outcome": "skipped",
                            "details": [NOT_IN_PLAN],
                        }
                    )
                elif row["status"] == "already":
                    results.append(
                        {
                            **self._profile_result_head(row),
                            "outcome": "already_set",
                            "details": [row["detail"]],
                        }
                    )
                elif row["status"] == "skipped":
                    reason = REASON_TEXT.get(row["reason"] or "", "Skipped")
                    results.append(
                        {
                            **self._profile_result_head(row),
                            "outcome": "skipped",
                            "details": [f"{reason}: {row['detail']}"],
                        }
                    )
                elif row not in chosen:
                    chosen.append(row)
            chosen.sort(key=rows.index)
        undo = _empty_filter()
        restart = "none"
        for row in chosen:
            result = self._profile_apply_row(row, profile, undo)
            results.append(result)
            if result["outcome"] == "applied":
                restart = _restart_max([restart, row["restart"]])

        def count(outcome: str) -> int:
            return len([r for r in results if r["outcome"] == outcome])

        return {
            "dry_run": False,
            "name": profile["name"],
            "session_id": 1 if chosen else None,
            "restore_point": None,
            "results": results,
            "applied": count("applied"),
            "already": count("already_set"),
            "skipped": count("skipped"),
            "failed": count("failed"),
            "restart": restart,
            "warnings": [],
            "undo": undo,
        }

    @staticmethod
    def _profile_result_head(row: dict[str, Any]) -> dict[str, Any]:
        return {"key": row["key"], "section": row["section"], "title": row["title"]}

    # -- export ----------------------------------------------------------------------

    def _profile_candidates(self) -> list[tuple[dict[str, Any], tuple[str, Any]]]:
        """(row, (section, value)) of every setting this PC can export."""
        out: list[tuple[dict[str, Any], tuple[str, Any]]] = []
        for item in self._items():
            if item["kind"] != "tweak" or item["state"] != "applied":
                continue
            item_id = item["id"]
            per_user = item_id not in self.task_tweaks and not item_id.startswith("privacy.")
            detail = "Changed by Cairn" if item["revertible"] else "Already set on this PC"
            row = {
                "key": f"tweak:{item_id}",
                "section": "tweaks",
                "title": item["title"],
                "detail": detail,
                "selected": True,
                "caution": None,
                "per_user": per_user,
            }
            out.append((row, ("tweaks", item_id)))
        for entry in self._startup_entries():
            if entry["enabled"]:
                continue
            row = {
                "key": f"startup:{entry['id']}",
                "section": "startup",
                "title": entry["name"],
                "detail": f"{entry['location']} · turned off",
                "selected": True,
                "caution": None,
                "per_user": entry["source"] in PER_USER_SOURCES,
            }
            out.append((row, ("startup", {"id": entry["id"], "name": entry["name"]})))
        if STORE_APP in self._profile_removed_apps:
            row = {
                "key": f"app:{STORE_APP}",
                "section": "apps",
                "title": STORE_APP_TITLE,
                "detail": "Removed by Cairn",
                "selected": True,
                "caution": None,
                "per_user": True,
            }
            out.append((row, ("apps", STORE_APP)))
        if self.profile_other_account is not None:
            for row, _ in out:
                if row["per_user"]:
                    row.update(selected=False, caution=OTHER_ACCOUNT_CAUTION)
        return out

    # -- module surface ------------------------------------------------------------------

    def profile_starters(self) -> list[dict[str, Any]]:
        self._record_quick("profile_starters")
        return copy.deepcopy(list(FAKE_STARTERS))

    def profile_check(self, text: str) -> dict[str, Any]:
        self._record_quick("profile_check", text)
        return self._profile_summary(self._profile_parse(text))

    def profile_read(self, path: str) -> dict[str, Any]:
        self._record("profile_read", path)
        if self.profile_invalid is not None:
            raise ValueError(self.profile_invalid)
        if not os.path.isabs(path):
            raise ValueError(f"the profile path must be absolute, got {path!r}")
        text = self.profile_files.get(os.path.normpath(path))
        if text is None:
            raise RuntimeError(NOT_FOUND)
        return self._profile_summary(self._profile_parse(text))

    def profile_candidates(self) -> dict[str, Any]:
        self._record("profile_candidates")
        if self.profile_candidates_error is not None:
            raise RuntimeError(self.profile_candidates_error)
        other = None
        if self.profile_other_account is not None:
            other = (
                "Cairn is running as a different account than the signed-in user, so settings that belong "
                "to a user account were read from that account, not yours."
            )
        return {
            "rows": [row for row, _ in self._profile_candidates()],
            "other_account": other,
            "warnings": [],
        }

    def profile_export(
        self, path: str, name: str, description: str = "", keys: list[str] | None = None
    ) -> dict[str, Any]:
        self._record("profile_export", path, name, description, None if keys is None else list(keys))
        if not name.strip():
            raise ValueError("The profile needs a name.")
        if not os.path.isabs(path) or not path.lower().endswith(".json"):
            raise ValueError("Profiles are saved as .json files.")
        candidates = self._profile_candidates()
        present = {row["key"] for row, _ in candidates}
        missing = [] if keys is None else [k for k in keys if k not in present]
        chosen = [
            value for row, value in candidates if (row["selected"] if keys is None else row["key"] in keys)
        ]
        if not chosen:
            raise RuntimeError(NOTHING_SELECTED)
        profile: dict[str, Any] = {
            "format": FORMAT,
            "schema": SCHEMA,
            "name": name.strip(),
            "created": "2026-09-28",
            "created_with": "Cairn 0.0.0-test",
        }
        if description.strip():
            profile["description"] = description.strip()
        for section, value in chosen:
            profile.setdefault(section, []).append(value)
        self.exported[path] = profile_text(profile)
        return {"path": path, "name": name.strip(), "counts": counts_of(profile), "missing": missing}

    def profile_apply(
        self, text: str, keys: list[str] | None = None, restore_point: str = "try", dry_run: bool = False
    ) -> dict[str, Any]:
        self._record("profile_apply", text, None if keys is None else list(keys), restore_point, dry_run)
        profile = self._profile_parse(text)
        if dry_run:
            return self._profile_plan(profile)
        # Refused before the plan is built, as the engine refuses before it reads anything.
        if not self.elevated:
            raise RuntimeError(ELEVATION_ERROR)
        if self.profile_apply_error is not None:
            raise RuntimeError(self.profile_apply_error)
        return self._profile_apply(profile, None if keys is None else list(keys))
