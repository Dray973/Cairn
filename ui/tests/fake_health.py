"""Health part of FakeEngine: the `health_*` functions of the security checkup, the Windows
Update search and the boot history.

Mixed into `FakeEngine`, which calls `_init_health` from its constructor. Nothing is read
from the system. The checkup lists all 24 checks in the engine's order with generic texts:
by default "File name extensions" (low, with the tweak fix) and "Remote Assistance" (low,
with a Windows tool fix) need attention, Remote Desktop is not applicable, drive encryption
is unknown without administrator rights (with the elevate fix) and "Drive C: is not
encrypted" (medium) with them, "Waiting updates" follows the fake Windows Update search and
every other check passed. The score is computed with a copy of the engine's penalty table.

The Windows Update search never ends on its own: `update_scan_finish(updates, error)` ends it
the way the engine's search thread would.

Options (constructor keywords, popped here):
- `health_failure`: `health_security_checkup` raises RuntimeError with this text.
- `health_check_overrides`: check id -> fields merged into that check.
- `health_update_scan_due` (True): whether a checkup asks for an offline search while none
  finished. A search that was stopped or failed counts as finished, as in the engine (which
  asks again 30 minutes later).
- `health_update_scan_error`: `health_update_scan_start` raises RuntimeError with this text.
- `health_boot_access` ("ok"): the access of an elevated boot history read ("log_disabled",
  "log_missing"); without elevation it is always "needs_admin".
- `health_boot_failure`: `health_boot_history` raises RuntimeError with this text.
- `health_boot_count` (12): starts in the history, one a day from 2026-09-01, newest first;
  all are full starts, the only starts Windows times.
- `health_boot_slow`: slow-item specs `(kind, name, title, path, startup_ids)`, optionally
  followed by the phase ("startup", the default, or "shutdown"); the default is the Discord app
  (matched to the startup entry `user_run:Discord`) and a storage driver. Startup items slow
  every fourth start, shutdown items the oldest listed shutdown.
- `health_boot_notes`, `health_boot_errors`: the `notes` and `errors` of a history that was
  read (access "ok" or "log_disabled"); the attributes of the same names may be changed later.
"""

from __future__ import annotations

import copy
import threading
from datetime import UTC, datetime, timedelta
from typing import TYPE_CHECKING, Any

TAKEN_AT = "2026-09-25T10:00:00+00:00"
READ_AT = "2026-09-28T09:00:00+00:00"
FIRST_BOOT = datetime(2026, 9, 1, 8, 0, tzinfo=UTC)

# Engine order of the checks: (id, group, title, most severe outcome).
CHECKS: tuple[tuple[str, str, str, str], ...] = (
    ("antivirus", "protection", "Antivirus", "critical"),
    ("realtime_protection", "protection", "Real-time protection", "critical"),
    ("security_intelligence", "protection", "Virus definitions", "high"),
    ("tamper_protection", "protection", "Tamper Protection", "medium"),
    ("threat_actions", "protection", "Threat actions", "critical"),
    ("firewall", "network", "Firewall", "critical"),
    ("remote_desktop", "network", "Remote Desktop", "high"),
    ("remote_assistance", "network", "Remote Assistance", "low"),
    ("smb1", "network", "SMB 1.0 file sharing", "high"),
    ("windows_update", "updates", "Windows Update", "high"),
    ("pending_updates", "updates", "Waiting updates", "high"),
    ("update_restart", "updates", "Restart for updates", "medium"),
    ("encryption", "device", "Drive encryption", "high"),
    ("secure_boot", "device", "Secure Boot", "high"),
    ("tpm", "device", "TPM", "medium"),
    ("memory_integrity", "device", "Memory integrity", "medium"),
    ("uac", "accounts", "User Account Control", "critical"),
    ("admin_account", "accounts", "Your account", "low"),
    ("builtin_accounts", "accounts", "Built-in accounts", "high"),
    ("auto_sign_in", "accounts", "Automatic sign-in", "high"),
    ("smartscreen", "apps", "SmartScreen for apps and files", "high"),
    ("edge_smartscreen", "apps", "SmartScreen in Microsoft Edge", "medium"),
    ("smart_app_control", "apps", "Smart App Control", "info"),
    ("file_extensions", "apps", "File name extensions", "low"),
)
PER_USER = ("edge_smartscreen", "file_extensions")
# Points an attention finding takes off the score (the engine's table).
PENALTY = {"critical": 40, "high": 20, "medium": 8, "low": 3, "info": 0}
SEVERITY_ORDER = ("info", "low", "medium", "high", "critical")

DEFAULT_SLOW: tuple[tuple[Any, ...], ...] = (
    ("app", "Discord.exe", "Discord", "C:\\Apps\\Discord.exe", ("user_run:Discord",)),
    ("driver", "contoso_storage.sys", "Contoso Storage Driver", None, ()),
)
# Degradation event ids of a shutdown by kind (the engine's 201 to 203).
SHUTDOWN_EVENTS = {"app": 201, "device": 202, "service": 203}


def slow_spec(spec: tuple[Any, ...]) -> tuple[str, str, str, str | None, tuple[str, ...], str]:
    """A slow-item spec with its phase ("startup" unless given)."""
    kind, name, title, path, ids = spec[:5]
    phase = spec[5] if len(spec) > 5 else "startup"
    return kind, name, title, path, tuple(ids), phase


def idle_update_scan() -> dict[str, Any]:
    """The Windows Update search's view before any search ran."""
    return {
        "state": "idle",
        "online": False,
        "started_at": None,
        "finished_at": None,
        "elapsed_ms": 0,
        "updates": [],
        "error": None,
    }


def uri_fix(label: str, uri: str) -> dict[str, Any]:
    return {"label": label, "note": None, "action": {"kind": "uri", "uri": uri}}


def tool_fix(label: str, tool: str, requires_admin: bool, note: str | None = None) -> dict[str, Any]:
    return {
        "label": label,
        "note": note,
        "action": {"kind": "windows_tool", "tool": tool, "requires_admin": requires_admin},
    }


def check(check_id: str, state: str, summary: str, severity: str | None = None) -> dict[str, Any]:
    """A check of the engine's shape with generic texts and no fixes."""
    spec = next(c for c in CHECKS if c[0] == check_id)
    return {
        "id": check_id,
        "group": spec[1],
        "title": spec[2],
        "state": state,
        "severity": severity or spec[3],
        "summary": summary,
        "detail": f"What the {spec[2]} check looks at.",
        "facts": [],
        "fixes": [],
        "needs_admin": False,
        "per_user": check_id in PER_USER,
    }


def score_of(checks: list[dict[str, Any]]) -> dict[str, Any]:
    """The engine's score of `checks`."""
    attention = [c for c in checks if c["state"] == "attention"]
    value = max(0, 100 - sum(PENALTY[c["severity"]] for c in attention))
    critical = sum(1 for c in attention if c["severity"] == "critical")
    high = any(c["severity"] == "high" for c in attention)
    if critical or value < 60:
        grade = "at_risk"
    elif high or value < 90:
        grade = "fair"
    else:
        grade = "good"
    return {
        "value": value,
        "grade": grade,
        "to_fix": sum(1 for c in attention if c["severity"] != "info"),
        "critical": critical,
        "unknown": sum(1 for c in checks if c["state"] in ("unknown", "checking")),
        "checked": sum(1 for c in checks if c["state"] in ("good", "attention")),
    }


def _iso(moment: datetime) -> str:
    return moment.isoformat().replace("+00:00", "Z")


class FakeHealth:
    """Security checkup, Windows Update search and boot history from fixtures."""

    if TYPE_CHECKING:
        elevated: bool
        other_user: bool | None

        def _record(self, name: str, *args: Any) -> None: ...
        def _record_quick(self, name: str, *args: Any) -> None: ...
        def _startup_entries(self) -> list[dict[str, Any]]: ...

    def _init_health(self, options: dict[str, Any]) -> None:
        """Pops the health options this fake understands from `options`."""
        self.health_failure: str | None = options.pop("health_failure", None)
        self.health_check_overrides: dict[str, dict[str, Any]] = dict(
            options.pop("health_check_overrides", None) or {}
        )
        self.health_update_scan_due: bool = options.pop("health_update_scan_due", True)
        self.health_update_scan_error: str | None = options.pop("health_update_scan_error", None)
        self.health_boot_access: str = options.pop("health_boot_access", "ok")
        self.health_boot_failure: str | None = options.pop("health_boot_failure", None)
        self.health_boot_count: int = options.pop("health_boot_count", 12)
        self.health_boot_slow = tuple(options.pop("health_boot_slow", DEFAULT_SLOW))
        self.health_boot_notes: list[str] = list(options.pop("health_boot_notes", None) or [])
        self.health_boot_errors: list[str] = list(options.pop("health_boot_errors", None) or [])
        self._scan = idle_update_scan()
        self._scan_started: datetime | None = None
        self._scan_lock = threading.Lock()

    # -- fixtures ------------------------------------------------------------------------

    def _pending_check(self, scan: dict[str, Any]) -> dict[str, Any]:
        state = scan["state"]
        if state == "running":
            text = "Checking Windows Update online…" if scan["online"] else "Checking for waiting updates…"
            result = check("pending_updates", "checking", text)
        elif state == "idle":
            result = check("pending_updates", "unknown", "Not checked yet")
        elif state == "cancelled":
            result = check("pending_updates", "unknown", "The check was stopped")
        elif state == "failed":
            result = check("pending_updates", "unknown", str(scan["error"] or "The check failed"))
        else:
            updates = scan["updates"]
            security = [u for u in updates if u.get("security")]
            if security:
                count = len(security)
                result = check(
                    "pending_updates",
                    "attention",
                    f"{count} security update{'' if count == 1 else 's'} waiting",
                    "medium",
                )
            elif updates:
                count = len(updates)
                result = check(
                    "pending_updates",
                    "attention",
                    f"{count} other update{'' if count == 1 else 's'} available",
                    "low",
                )
            elif scan["online"]:
                result = check("pending_updates", "good", "None found online just now")
            else:
                result = check("pending_updates", "good", "None found in Windows Update's last check")
            result["facts"] = [{"label": "", "value": f"• {u['title']}"} for u in updates[:5]]
        result["fixes"] = [
            {"label": "Check online now", "note": None, "action": {"kind": "update_scan", "online": True}},
            uri_fix("Open Windows Update", "ms-settings:windowsupdate"),
        ]
        return result

    def _checks(self, scan: dict[str, Any]) -> list[dict[str, Any]]:
        checks: list[dict[str, Any]] = []
        for check_id, _group, _title, _severity in CHECKS:
            if check_id == "pending_updates":
                item = self._pending_check(scan)
            elif check_id == "file_extensions":
                item = check(check_id, "attention", "Hidden", "low")
                item["fixes"] = [
                    {
                        "label": "Show file extensions",
                        "note": None,
                        "action": {"kind": "tweak", "id": "interface.file_extensions"},
                    }
                ]
            elif check_id == "remote_assistance":
                item = check(check_id, "attention", "Invitations are allowed", "low")
                item["fixes"] = [tool_fix("Open Remote settings", "remote_settings", True)]
            elif check_id == "remote_desktop":
                item = check(check_id, "not_applicable", "Not available on Windows 11 Home")
            elif check_id == "encryption":
                if self.elevated:
                    item = check(check_id, "attention", "Drive C: is not encrypted", "medium")
                    item["facts"] = [{"label": "Drives", "value": "C: not encrypted"}]
                else:
                    item = check(check_id, "unknown", "Needs administrator rights to check")
                    item["needs_admin"] = True
                    item["fixes"] = [
                        {"label": "Restart as administrator", "note": None, "action": {"kind": "elevate"}}
                    ]
                item["fixes"].append(
                    uri_fix("Open BitLocker", "shell:::{D9EF8727-CAC2-4E60-809E-86F80A666C91}")
                )
            elif check_id == "firewall":
                item = check(check_id, "good", "On for every network type")
                item["facts"] = [{"label": "Current network", "value": "Private"}]
                item["fixes"] = [uri_fix("Open Firewall & network protection", "windowsdefender://network/")]
            elif check_id == "uac":
                item = check(check_id, "good", "Notify when apps make changes (default)")
                item["fixes"] = [tool_fix("Open UAC settings", "uac_settings", False)]
            else:
                item = check(check_id, "good", "On")
            override = self.health_check_overrides.get(check_id)
            if override:
                item.update(copy.deepcopy(override))
            checks.append(item)
        return checks

    def _scan_view(self) -> dict[str, Any]:
        with self._scan_lock:
            view = dict(self._scan)
            view["updates"] = [dict(u) for u in self._scan["updates"]]
            if view["state"] == "running" and self._scan_started is not None:
                elapsed = datetime.now(UTC) - self._scan_started
                view["elapsed_ms"] = int(elapsed.total_seconds() * 1000)
            return view

    # -- module surface ------------------------------------------------------------------

    def health_security_checkup(self) -> dict[str, Any]:
        self._record("health_security_checkup")
        if self.health_failure is not None:
            raise RuntimeError(self.health_failure)
        scan = self._scan_view()
        checks = self._checks(scan)
        notes = [] if self.elevated else ["Drive encryption can only be checked with administrator rights."]
        return {
            "taken_at": TAKEN_AT,
            "duration_ms": 1300,
            "elevated": self.elevated,
            "other_user": self.other_user,
            "home_edition": True,
            "score": score_of(checks),
            "checks": checks,
            "update_scan": scan,
            "update_scan_due": self.health_update_scan_due and scan["state"] == "idle",
            "notes": notes,
            "errors": [],
        }

    def health_update_scan_start(self, online: bool = False) -> dict[str, Any]:
        self._record_quick("health_update_scan_start", online)
        if self.health_update_scan_error is not None:
            raise RuntimeError(self.health_update_scan_error)
        with self._scan_lock:
            if self._scan["state"] == "running":
                raise RuntimeError("Windows Update is already being checked.")
            self._scan_started = datetime.now(UTC)
            self._scan = {
                **idle_update_scan(),
                "state": "running",
                "online": online,
                "started_at": _iso(self._scan_started),
            }
        return self._scan_view()

    def health_update_scan(self) -> dict[str, Any]:
        self._record_quick("health_update_scan")
        return self._scan_view()

    def health_update_scan_cancel(self) -> bool:
        self._record_quick("health_update_scan_cancel")
        with self._scan_lock:
            if self._scan["state"] != "running":
                return False
            self._scan = {**self._scan, "state": "cancelled", "finished_at": _iso(datetime.now(UTC))}
            return True

    def health_boot_history(self, limit: int = 60) -> dict[str, Any]:
        self._record("health_boot_history", limit)
        if not 1 <= limit <= 500:
            raise ValueError(f"limit must be between 1 and 500, got {limit}")
        if self.health_boot_failure is not None:
            raise RuntimeError(self.health_boot_failure)
        access = self.health_boot_access if self.elevated else "needs_admin"
        if access in ("needs_admin", "log_missing"):
            return self._boot_history(access, count=0)
        return self._boot_history(access, count=min(self.health_boot_count, limit))

    # -- test drivers --------------------------------------------------------------------

    def update_scan_finish(
        self, updates: list[dict[str, Any]] | None = None, error: str | None = None
    ) -> None:
        """Ends the running search: with `error` it failed, otherwise it found `updates`
        (dicts with at least a title; `security` defaults to False)."""
        with self._scan_lock:
            assert self._scan["state"] == "running", "no Windows Update search is running"
            found = [
                {
                    "title": u["title"],
                    "kb": list(u.get("kb", [])),
                    "msrc_severity": u.get("msrc_severity"),
                    "security": bool(u.get("security", False)),
                    "downloaded": bool(u.get("downloaded", False)),
                    "released_at": u.get("released_at"),
                }
                for u in updates or []
            ]
            started = self._scan_started or datetime.now(UTC)
            elapsed = int((datetime.now(UTC) - started).total_seconds() * 1000)
            self._scan = {
                **self._scan,
                "state": "failed" if error is not None else "done",
                "finished_at": _iso(datetime.now(UTC)),
                "elapsed_ms": elapsed,
                "updates": [] if error is not None else found,
                "error": error,
            }

    # -- boot fixture --------------------------------------------------------------------

    def _slow_events(self, start: datetime, phase: str = "startup") -> list[dict[str, Any]]:
        """The degradation events of one start (or shutdown) that began at `start`."""
        events = []
        for index, spec in enumerate(self.health_boot_slow):
            kind, name, title, path, _ids, spec_phase = slow_spec(spec)
            if spec_phase != phase:
                continue
            if phase == "shutdown":
                event_id = SHUTDOWN_EVENTS.get(kind, 203)
            else:
                event_id = 101 if kind == "app" else 102
            events.append(
                {
                    "event_id": event_id,
                    "kind": kind,
                    "name": name,
                    "title": title,
                    "path": path,
                    "company": "Contoso",
                    "version": "1.0",
                    "total_ms": 4000 + index * 1000,
                    "degradation_ms": 2500 + index * 1000,
                    "at": _iso(start + timedelta(seconds=10)),
                }
            )
        return events

    def _boot_history(self, access: str, *, count: int) -> dict[str, Any]:
        boots = []
        for day in range(count):
            start = FIRST_BOOT + timedelta(days=day)
            boot_ms = 38_000 + (day % 5) * 1500
            boots.append(
                {
                    "record_id": 1000 + day,
                    "started_at": _iso(start),
                    "logged_at": _iso(start + timedelta(milliseconds=boot_ms + 30_000)),
                    "boot_ms": boot_ms,
                    "main_path_ms": boot_ms // 2,
                    "post_boot_ms": boot_ms - boot_ms // 2,
                    "startup_apps": 12,
                    "after_update": day == 3,
                    "degraded": day % 4 == 1,
                    "boot_type": "full",
                    "after_unexpected_shutdown": False,
                    "phases": {
                        "kernel_ms": 1500,
                        "drivers_ms": None,
                        "devices_ms": None,
                        "user_profile_ms": None,
                        "explorer_ms": None,
                    },
                    "slow": self._slow_events(start) if day % 4 == 1 else [],
                }
            )
        boots.reverse()
        shutdowns = [
            {
                "record_id": 2000 + day,
                "started_at": _iso(FIRST_BOOT + timedelta(days=day, hours=15)),
                "logged_at": _iso(FIRST_BOOT + timedelta(days=day, hours=15, seconds=20)),
                "shutdown_ms": 12_300,
                "degraded": day == 0,
                "slow": (
                    self._slow_events(FIRST_BOOT + timedelta(days=day, hours=15), "shutdown")
                    if day == 0
                    else []
                ),
            }
            for day in range(min(count, 3))
        ]
        shutdowns.reverse()
        slowed = {
            "startup": [b for b in boots if b["slow"]],
            "shutdown": [s for s in shutdowns if s["slow"]],
        }
        slow_items = []
        matched: set[str] = set()
        for index, spec in enumerate(self.health_boot_slow):
            kind, name, title, path, ids, phase = slow_spec(spec)
            records = slowed[phase]
            if not records:
                continue
            slow_items.append(
                {
                    "key": f"{phase}:{kind}:{name.lower()}",
                    "phase": phase,
                    "kind": kind,
                    "name": name,
                    "title": title,
                    "path": path,
                    "company": "Contoso",
                    "count": len(records),
                    "last_seen": records[0]["logged_at"],
                    "median_degradation_ms": 2500 + index * 1000,
                    "max_degradation_ms": 3000 + index * 1000,
                    "startup_ids": list(ids),
                }
            )
            matched.update(ids)
        entries = [e for e in self._startup_entries() if e["id"] in matched]
        times = sorted(b["boot_ms"] for b in boots)
        median = None
        if times:
            mid = len(times) // 2
            median = times[mid] if len(times) % 2 else (times[mid - 1] + times[mid]) // 2
        full = [b for b in boots if b["boot_type"] == "full"]
        # Nothing is read without access to the log, so nothing can be reported about it.
        read = access in ("ok", "log_disabled")
        trend = None
        if len(boots) >= 8:
            trend = {
                "boot_type": "full" if len(full) >= 8 else None,
                "recent_median_ms": 40_000,
                "earlier_median_ms": 38_000,
                "change_pct": 5.26,
            }
        return {
            "read_at": READ_AT,
            "access": access,
            "boots": boots,
            "shutdowns": shutdowns,
            "slow_items": slow_items,
            "unexpected_shutdowns": [],
            "stats": {
                "count": len(boots),
                "latest_ms": boots[0]["boot_ms"] if boots else None,
                "median_ms": median,
                "full_count": len(full),
                "fast_count": sum(1 for b in boots if b["boot_type"] == "fast_startup"),
                "trend": trend,
            },
            "fast_startup": True,
            "startup_entries": entries,
            "notes": list(self.health_boot_notes) if read else [],
            "errors": list(self.health_boot_errors) if read else [],
        }


__all__ = [
    "CHECKS",
    "PENALTY",
    "SEVERITY_ORDER",
    "FakeHealth",
    "check",
    "idle_update_scan",
    "score_of",
]
