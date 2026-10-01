"""Network part of FakeEngine: the `network_*` functions and the DNS journal records.

Mixed into `FakeEngine`, which calls `_init_network` from its constructor and the DNS
journal hooks from its summary, export and revert functions.

Five adapters (`ADAPTERS`) are listed with the engine's result shapes. Static DNS servers
live in `_static_dns` per (interface GUID, family); a change records the servers it
replaces in `_dns_baseline` before it writes, the first change of a family wins (a later
successful write only updates the record's target), and reverting writes the baseline back.
Options (`FakeEngine(**options)`):

- `network_error`: `network_list` raises RuntimeError with this text;
- `dns_fail`: families ("ipv4", "ipv6") whose DNS write fails (the record is kept);
- `reset_fail` / `reset_partial`: reset step ids ("winsock", "ipv4", "ipv6") that end
  failed / completed with errors, both with exit code 1;
- `vpn_connected`: ProtonVPN is connected, which pauses DNS changes on every adapter;
- `profile_dns`: interface GUID -> servers set for that adapter's Wi-Fi network;
- `dns_policy`: servers set by the DNS Client policy.

A real reset with the "try" policy reports restore point #9, or no point and a warning when
System Protection is off (the FakeTools option `restore_enabled=False`).
"""

from __future__ import annotations

import copy
import ipaddress
from collections.abc import Iterable, Sequence
from typing import Any

WIFI = "{aaaaaaaa-0000-0000-0000-000000000001}"
ETHERNET = "{aaaaaaaa-0000-0000-0000-000000000002}"
DEFAULT_SWITCH = "{aaaaaaaa-0000-0000-0000-000000000003}"
PROTON_VPN = "{aaaaaaaa-0000-0000-0000-000000000004}"
WIFI_DIRECT = "{aaaaaaaa-0000-0000-0000-000000000005}"

ELEVATION_ERROR = "this operation requires an elevated (Administrator) process"
PROTECTION_OFF = "System Protection is off for the system drive, so no restore point can be created"
FAMILY_LABELS = {"ipv4": "IPv4", "ipv6": "IPv6"}
RECORDED_AT = "2026-09-25T10:00:00+00:00"

DNS_PRESETS: tuple[dict[str, Any], ...] = (
    {
        "id": "automatic",
        "title": "Automatic (DHCP)",
        "description": "Uses the DNS servers the network hands out, usually your router's.",
        "ipv4": [],
        "ipv6": [],
    },
    {
        "id": "cloudflare",
        "title": "Cloudflare",
        "description": "Cloudflare's fast public DNS, without filtering.",
        "ipv4": ["1.1.1.1", "1.0.0.1"],
        "ipv6": ["2606:4700:4700::1111", "2606:4700:4700::1001"],
    },
    {
        "id": "cloudflare_security",
        "title": "Cloudflare (blocks malware)",
        "description": "Cloudflare DNS that blocks known malware sites.",
        "ipv4": ["1.1.1.2", "1.0.0.2"],
        "ipv6": ["2606:4700:4700::1112", "2606:4700:4700::1002"],
    },
    {
        "id": "cloudflare_family",
        "title": "Cloudflare (blocks malware and adult content)",
        "description": "Cloudflare DNS that blocks malware and adult content.",
        "ipv4": ["1.1.1.3", "1.0.0.3"],
        "ipv6": ["2606:4700:4700::1113", "2606:4700:4700::1003"],
    },
    {
        "id": "google",
        "title": "Google Public DNS",
        "description": "Google's public DNS, without filtering.",
        "ipv4": ["8.8.8.8", "8.8.4.4"],
        "ipv6": ["2001:4860:4860::8888", "2001:4860:4860::8844"],
    },
    {
        "id": "quad9",
        "title": "Quad9 (blocks malware)",
        "description": "Quad9 DNS that blocks domains known to be malicious.",
        "ipv4": ["9.9.9.9", "149.112.112.112"],
        "ipv6": ["2620:fe::fe", "2620:fe::9"],
    },
    {
        "id": "quad9_unfiltered",
        "title": "Quad9 (no filtering)",
        "description": "Quad9 DNS without any blocking.",
        "ipv4": ["9.9.9.10", "149.112.112.10"],
        "ipv6": ["2620:fe::10", "2620:fe::fe:10"],
    },
)

NOTE_NOT_PRESENT = "This adapter is not present."
NOTE_VPN_ADAPTER = (
    "VPN and tunnel adapters get their DNS servers from the software that manages them; change DNS there."
)
NOTE_OTHER = "DNS cannot be changed on this type of adapter here."
NOTE_UNREADABLE = "Its DNS settings could not be read."
NOTE_PROFILE = (
    "DNS servers for this Wi-Fi network are set in Windows Settings (Wi-Fi > network properties); "
    "change or clear them there."
)
NOTE_VPN_CONNECTED = (
    "Disconnect the VPN to change DNS servers; while it is connected it may be managing them."
)
NOTE_VIRTUAL = (
    "Virtual adapter: the software that created it (for example Hyper-V or a VPN) may replace these DNS "
    "servers."
)

# (id, title, command) of the reset steps, in order.
RESET_STEPS = (
    ("winsock", "Reset the Winsock catalog", "netsh winsock reset"),
    ("ipv4", "Reset TCP/IP for IPv4", "netsh int ip reset"),
    ("ipv6", "Reset TCP/IP for IPv6", "netsh int ipv6 reset"),
)

_AUTOMATIC: dict[str, Any] = {"mode": "automatic", "servers": [], "preset": None, "profile_servers": []}


def _adapter(**fields: Any) -> dict[str, Any]:
    """An adapter with every key of the engine's Adapter shape; `fields` override defaults."""
    adapter: dict[str, Any] = {
        "id": "",
        "name": "",
        "description": "",
        "kind": "ethernet",
        "status": "disconnected",
        "limited": False,
        "hardware": True,
        "minor": False,
        "primary": False,
        "if_index": 0,
        "mac": "",
        "mtu": 1500,
        "receive_bps": None,
        "transmit_bps": None,
        "dhcp_enabled": True,
        "ipv4_enabled": True,
        "ipv6_enabled": True,
        "ipv4": [],
        "ipv6": [],
        "gateways": [],
        "dns_servers": [],
        "dns_ipv4": dict(_AUTOMATIC),
        "dns_ipv6": dict(_AUTOMATIC),
        "dns_suffix": "",
        "ipv4_metric": 0,
        "can_change_dns": True,
        "can_renew": False,
        "dns_revertible": False,
        "note": None,
    }
    adapter.update(fields)
    return adapter


ADAPTERS: tuple[dict[str, Any], ...] = (
    _adapter(
        id=WIFI,
        name="Wi-Fi",
        description="Contoso Wi-Fi 6E Adapter",
        kind="wifi",
        status="connected",
        primary=True,
        if_index=12,
        mac="00-00-5E-00-53-01",
        receive_bps=1_201_000_000,
        transmit_bps=1_201_000_000,
        ipv6_enabled=False,
        ipv4=[{"address": "192.168.0.23", "prefix_length": 24, "origin": "dhcp", "preferred": True}],
        gateways=["192.168.0.1"],
        dns_servers=["192.168.0.1"],
        dns_ipv4={"mode": "automatic", "servers": ["192.168.0.1"], "preset": None, "profile_servers": []},
        dns_suffix="home",
        ipv4_metric=35,
        can_renew=True,
    ),
    _adapter(
        id=ETHERNET,
        name="Ethernet",
        description="Fabrikam 2.5GbE Controller",
        kind="ethernet",
        status="disconnected",
        if_index=7,
        mac="00-00-5E-00-53-02",
        # Without a link Windows keeps tentative link-local addresses it cannot use.
        ipv4=[{"address": "169.254.81.150", "prefix_length": 16, "origin": "link_local", "preferred": False}],
        ipv6=[{"address": "fe80::b03b:2", "prefix_length": 64, "origin": "link_local", "preferred": False}],
        ipv4_metric=25,
    ),
    _adapter(
        id=DEFAULT_SWITCH,
        name="vEthernet (Default Switch)",
        description="Hyper-V Virtual Ethernet Adapter",
        kind="virtual",
        status="connected",
        hardware=False,
        if_index=30,
        mac="00-00-5E-00-53-03",
        receive_bps=10_000_000_000,
        transmit_bps=10_000_000_000,
        dhcp_enabled=False,
        ipv4=[{"address": "172.28.64.1", "prefix_length": 20, "origin": "manual", "preferred": True}],
        ipv6=[{"address": "fe80::5d:3", "prefix_length": 64, "origin": "link_local", "preferred": True}],
        ipv4_metric=5000,
        note=NOTE_VIRTUAL,
    ),
    _adapter(
        id=PROTON_VPN,
        name="ProtonVPN",
        description="ProtonVPN Tunnel",
        kind="vpn",
        status="disconnected",
        hardware=False,
        if_index=41,
        dhcp_enabled=False,
        ipv4_metric=1,
        can_change_dns=False,
        note=NOTE_VPN_ADAPTER,
    ),
    _adapter(
        id=WIFI_DIRECT,
        name="Local Area Connection* 1",
        description="Microsoft Wi-Fi Direct Virtual Adapter",
        kind="virtual",
        status="disconnected",
        hardware=False,
        minor=True,
        if_index=15,
        mac="00-00-5E-00-53-05",
        note=NOTE_VIRTUAL,
    ),
)

# DNS servers the network hands out to each connected adapter, per family.
DHCP_DNS = {WIFI: {"ipv4": ["192.168.0.1"], "ipv6": []}}
# The VPN adapter's addresses while it is connected.
VPN_ADDRESS = {"address": "10.2.0.2", "prefix_length": 32, "origin": "manual", "preferred": True}


def canonical_guid(value: str) -> str:
    """Braces around the lowercase GUID, as the engine stores interface GUIDs."""
    return "{" + value.strip().strip("{}").lower() + "}"


def _copy(values: Sequence[str] | None) -> list[str] | None:
    return None if values is None else list(values)


def _preset_matching(servers: Sequence[str], family: str) -> str | None:
    if not servers:
        return None
    return next((p["id"] for p in DNS_PRESETS if p[family] and list(p[family]) == list(servers)), None)


def _parse_servers(values: Iterable[str], family: str) -> list[str]:
    """Validates custom servers of one family like the engine; ValueError on bad input."""
    label = FAMILY_LABELS[family]
    servers: list[str] = []
    for value in values:
        for item in value.replace(",", " ").replace(";", " ").split():
            try:
                address = ipaddress.ip_address(item)
            except ValueError:
                raise ValueError(f"{item} is not a valid {label} address") from None
            # The engine takes no zone index ("fe80::1%12").
            if (address.version == 6) != (family == "ipv6") or "%" in item:
                raise ValueError(f"{item} is not a valid {label} address")
            if address.is_unspecified or address.is_multicast or str(address) == "255.255.255.255":
                raise ValueError(f"{item} cannot be used as a DNS server")
            if address.version == 6 and (int(address) >> 112) & 0xFFC0 in (0xFE80, 0xFEC0):
                raise ValueError(f"{item} cannot be used as a DNS server")
            if str(address) not in servers:
                servers.append(str(address))
    if len(servers) > 4:
        raise ValueError(f"at most 4 {label} DNS servers")
    return servers


def _servers_text(servers: Sequence[str]) -> str:
    return ", ".join(servers) or "automatic"


class FakeNetwork:
    """In-memory adapters and DNS settings; nothing reaches the system."""

    # Provided by FakeEngine, with `_record` and `_record_quick`.
    elevated: bool

    def _init_network(self, options: dict[str, Any]) -> None:
        """Pops the network options this fake understands from `options`."""
        self.network_error: str | None = options.pop("network_error", None)
        self.dns_fail = set(options.pop("dns_fail", ()))
        self.reset_fail = set(options.pop("reset_fail", ()))
        self.reset_partial = set(options.pop("reset_partial", ()))
        self.vpn_connected = bool(options.pop("vpn_connected", False))
        profile = options.pop("profile_dns", None) or {}
        self.profile_dns: dict[str, list[str]] = {canonical_guid(k): list(v) for k, v in profile.items()}
        self.dns_policy = list(options.pop("dns_policy", ()))
        self.network_adapters = [copy.deepcopy(a) for a in ADAPTERS]
        # (interface GUID, family) -> static servers; missing or empty means automatic.
        self._static_dns: dict[tuple[str, str], list[str]] = {}
        # (interface GUID, family) -> servers recorded before the first change (active records).
        self._dns_baseline: dict[tuple[str, str], list[str]] = {}
        # Journal export rows of every DNS record, reverted ones included.
        self._dns_records: list[dict[str, Any]] = []

    # -- adapters ------------------------------------------------------------------

    def _profile_servers(self, guid: str, family: str) -> list[str]:
        servers = self.profile_dns.get(guid, [])
        return [s for s in servers if (":" in s) == (family == "ipv6")]

    def _dns_config(self, adapter: dict[str, Any], family: str) -> dict[str, Any]:
        guid = adapter["id"]
        profile = self._profile_servers(guid, family)
        if profile:
            return {
                "mode": "profile",
                "servers": list(profile),
                "preset": None,
                "profile_servers": list(profile),
            }
        static = self._static_dns.get((guid, family), [])
        if not static:
            dhcp = DHCP_DNS.get(guid, {}).get(family, []) if adapter["status"] == "connected" else []
            return {"mode": "automatic", "servers": list(dhcp), "preset": None, "profile_servers": []}
        return {
            "mode": "manual",
            "servers": list(static),
            "preset": _preset_matching(static, family),
            "profile_servers": [],
        }

    @staticmethod
    def _capabilities(adapter: dict[str, Any], vpn_connected: bool) -> None:
        """The engine's capability rules: the first matching row decides."""
        kind, status = adapter["kind"], adapter["status"]
        modes = {adapter["dns_ipv4"]["mode"], adapter["dns_ipv6"]["mode"]}
        can_change, note = True, None
        if status == "not_present":
            can_change, note = False, NOTE_NOT_PRESENT
        elif kind in ("vpn", "tunnel"):
            can_change, note = False, NOTE_VPN_ADAPTER
        elif kind == "other":
            can_change, note = False, NOTE_OTHER
        elif "unknown" in modes:
            can_change, note = False, NOTE_UNREADABLE
        elif "profile" in modes:
            can_change, note = False, NOTE_PROFILE
        elif vpn_connected:
            can_change, note = False, NOTE_VPN_CONNECTED
        elif kind == "virtual":
            note = NOTE_VIRTUAL
        adapter["can_change_dns"] = can_change
        adapter["note"] = note
        adapter["can_renew"] = (
            adapter["dhcp_enabled"]
            and adapter["ipv4_enabled"]
            and status == "connected"
            and kind in ("ethernet", "wifi", "virtual", "bluetooth")
        )

    def _adapters(self) -> list[dict[str, Any]]:
        adapters = []
        for base in self.network_adapters:
            adapter = copy.deepcopy(base)
            if adapter["id"] == PROTON_VPN and self.vpn_connected:
                adapter["status"] = "connected"
                adapter["ipv4"] = [dict(VPN_ADDRESS)]
            for family in ("ipv4", "ipv6"):
                adapter[f"dns_{family}"] = self._dns_config(adapter, family)
            # Like the engine, an adapter without a link lists no servers in use.
            adapter["dns_servers"] = [
                s
                for family in ("ipv4", "ipv6")
                if adapter[f"{family}_enabled"] and adapter["status"] == "connected"
                for s in adapter[f"dns_{family}"]["servers"]
            ]
            adapter["dns_revertible"] = any(
                (adapter["id"], f) in self._dns_baseline for f in ("ipv4", "ipv6")
            )
            adapters.append(adapter)
        vpn = any(a["kind"] == "vpn" and a["status"] == "connected" for a in adapters)
        for adapter in adapters:
            self._capabilities(adapter, vpn)
        return adapters

    def _find_adapter(self, adapter_id: str) -> dict[str, Any]:
        wanted = canonical_guid(adapter_id)
        adapter = next((a for a in self._adapters() if a["id"] == wanted), None)
        if adapter is None:
            raise RuntimeError(
                f"network adapter {adapter_id} is not on this PC; it may have been removed or disabled"
            )
        return adapter

    # -- module surface ------------------------------------------------------------

    def network_list(self) -> dict[str, Any]:
        self._record("network_list")
        if self.network_error is not None:
            raise RuntimeError(self.network_error)
        adapters = self._adapters()
        return {
            "adapters": adapters,
            "dns_policy": list(self.dns_policy),
            "vpn_connected": any(a["kind"] == "vpn" and a["status"] == "connected" for a in adapters),
            "warnings": [],
            "duration_ms": 18,
        }

    def network_dns_presets(self) -> list[dict[str, Any]]:
        self._record_quick("network_dns_presets")
        return copy.deepcopy(list(DNS_PRESETS))

    def _dns_request(
        self, preset: str, ipv4: Sequence[str] | None, ipv6: Sequence[str] | None
    ) -> dict[str, list[str] | None]:
        """Target servers per family; None leaves a family unchanged, [] means automatic.

        Preset ids are trimmed and compared ignoring ASCII case, like the engine.
        """
        wanted = preset.strip()
        if wanted.lower() == "custom":
            v4 = _parse_servers(ipv4 or [], "ipv4")
            v6 = _parse_servers(ipv6 or [], "ipv6")
            if not v4 and not v6:
                raise ValueError("custom DNS needs at least one IPv4 or IPv6 server")
            return {"ipv4": v4 or None, "ipv6": v6 or None}
        if ipv4 or ipv6:
            raise ValueError('ipv4 and ipv6 are only used with preset "custom"')
        match = next((p for p in DNS_PRESETS if p["id"] == wanted.lower()), None)
        if match is None:
            valid = ", ".join(p["id"] for p in DNS_PRESETS)
            raise ValueError(f'unknown DNS preset "{wanted}"; valid presets: {valid}')
        return {"ipv4": list(match["ipv4"]), "ipv6": list(match["ipv6"])}

    @staticmethod
    def _skip_reason(adapter: dict[str, Any], family: str, target: list[str]) -> str | None:
        if family == "ipv6" and not adapter["ipv6_enabled"]:
            return "IPv6 is turned off on this adapter"
        if family == "ipv4" and not adapter["ipv4_enabled"]:
            return "IPv4 is turned off on this adapter"
        if family == "ipv4" and not target and not adapter["dhcp_enabled"]:
            return (
                "this adapter has a manually set IPv4 address, so it would get no IPv4 DNS servers "
                "automatically"
            )
        return None

    def _record_dns(
        self, adapter: dict[str, Any], family: str, previous: list[str], target: list[str]
    ) -> bool:
        """Records the baseline before a write; an active record of the family wins.

        Returns whether this call recorded the baseline.
        """
        key = (adapter["id"], family)
        if key in self._dns_baseline:
            return False
        self._dns_baseline[key] = list(previous)
        self._dns_records.append(
            {
                "id": len(self._dns_records) + 1,
                "session_id": 1,
                "recorded_at": RECORDED_AT,
                "target": f"{FAMILY_LABELS[family]} DNS servers of {adapter['name']}",
                "interface_guid": adapter["id"],
                "family": family,
                "adapter_name": adapter["name"],
                "previous_servers": list(previous),
                "target_servers": list(target),
                "active": True,
                "reverted_at": None,
            }
        )
        return True

    def _update_dns_target(self, guid: str, family: str, target: list[str]) -> None:
        """The active record of a family follows the servers written over its baseline."""
        for record in self._dns_records:
            if record["active"] and record["interface_guid"] == guid and record["family"] == family:
                record["target_servers"] = list(target)

    def network_set_dns(
        self,
        adapter_id: str,
        preset: str,
        ipv4: list[str] | None = None,
        ipv6: list[str] | None = None,
        restore_point: str = "skip",
        dry_run: bool = False,
    ) -> dict[str, Any]:
        self._record("network_set_dns", adapter_id, preset, _copy(ipv4), _copy(ipv6), restore_point, dry_run)
        if not adapter_id:
            raise ValueError("adapter_id must not be empty")
        request = self._dns_request(preset, ipv4, ipv6)
        if not dry_run and not self.elevated:
            raise RuntimeError(ELEVATION_ERROR)
        adapter = self._find_adapter(adapter_id)
        if not adapter["can_change_dns"]:
            raise RuntimeError(adapter["note"])
        changes = []
        for family in ("ipv4", "ipv6"):
            target = request[family]
            if target is None:
                continue
            target_config = {
                "mode": "manual" if target else "automatic",
                "servers": list(target),
                "preset": _preset_matching(target, family),
                "profile_servers": [],
            }
            change: dict[str, Any] = {
                "family": family,
                "previous": adapter[f"dns_{family}"],
                "target": target_config,
                "outcome": "planned",
                "detail": None,
            }
            changes.append(change)
            reason = self._skip_reason(adapter, family, target)
            key = (adapter["id"], family)
            current = self._static_dns.get(key, [])
            if reason is not None:
                change.update(outcome="skipped", detail=reason)
            elif current == target:
                change["outcome"] = "already_set"
            elif not dry_run:
                captured = self._record_dns(adapter, family, current, target)
                if family in self.dns_fail:
                    change.update(
                        outcome="failed",
                        detail=f"cannot set the {FAMILY_LABELS[family]} DNS servers of {adapter['name']}: "
                        "Access is denied.",
                    )
                else:
                    self._static_dns[key] = list(target)
                    if not captured:
                        self._update_dns_target(adapter["id"], family, target)
                    change.update(
                        outcome="applied", detail=f"{_servers_text(current)} → {_servers_text(target)}"
                    )
        return {
            "dry_run": dry_run,
            "adapter_id": adapter["id"],
            "adapter_name": adapter["name"],
            "session_id": None if dry_run else 1,
            "changes": changes,
            "warnings": [],
        }

    def network_flush_dns(self) -> dict[str, Any]:
        self._record("network_flush_dns")
        return {"session_id": 1}

    def network_renew_dhcp(self, adapter_id: str, release_first: bool = False) -> dict[str, Any]:
        self._record("network_renew_dhcp", adapter_id, release_first)
        if not self.elevated:
            raise RuntimeError(ELEVATION_ERROR)
        adapter = self._find_adapter(adapter_id)
        if not adapter["can_renew"]:
            if not adapter["dhcp_enabled"]:
                raise RuntimeError("this adapter does not get its IPv4 address from DHCP")
            raise RuntimeError("this adapter is not connected")
        return {
            "adapter_id": adapter["id"],
            "adapter_name": adapter["name"],
            "session_id": 1,
            "released": release_first,
            "renewed": True,
            "ipv4": [a["address"] for a in adapter["ipv4"]],
            "duration_ms": 850,
        }

    def _manual_settings(self) -> list[dict[str, str]]:
        settings = []
        for adapter in self._adapters():
            if adapter["kind"] in ("vpn", "tunnel"):
                continue
            # An IPv6 gateway is named only without addresses from router advertisements.
            advertised = any(a["origin"] in ("autoconfigured", "dhcp", "temporary") for a in adapter["ipv6"])
            for family, label in (("ipv4", "IPv4"), ("ipv6", "IPv6")):
                gateways = [g for g in adapter["gateways"] if (":" in g) == (family == "ipv6")]
                if family == "ipv6" and advertised:
                    gateways = []
                for address in adapter[family]:
                    if address["origin"] != "manual":
                        continue
                    detail = f"static {label} {address['address']}/{address['prefix_length']}"
                    if gateways:
                        detail += f", gateway {gateways[0]}"
                    settings.append({"adapter": adapter["name"], "detail": detail})
            for family in ("ipv4", "ipv6"):
                servers = self._static_dns.get((adapter["id"], family), [])
                if servers:
                    settings.append(
                        {
                            "adapter": adapter["name"],
                            "detail": f"{FAMILY_LABELS[family]} DNS {', '.join(servers)} (set manually)",
                        }
                    )
        return settings

    def network_reset(self, restore_point: str = "try", dry_run: bool = False) -> dict[str, Any]:
        self._record("network_reset", restore_point, dry_run)
        if not dry_run and not self.elevated:
            raise RuntimeError(ELEVATION_ERROR)
        steps = []
        for step_id, title, command in RESET_STEPS:
            if dry_run:
                status, code, output = "planned", None, ""
            elif step_id in self.reset_fail:
                status, code, output = "failed", 1, f"Resetting {title}, failed.\nAccess is denied."
            elif step_id in self.reset_partial:
                status, code, output = (
                    "completed_with_errors",
                    1,
                    "Resetting Interface, OK!\nResetting Neighbor, failed.\nAccess is denied.",
                )
            else:
                status, code, output = (
                    "succeeded",
                    0,
                    "Resetting , OK!\nRestart the computer to complete this action.",
                )
            steps.append(
                {
                    "id": step_id,
                    "title": title,
                    "command": command,
                    "status": status,
                    "exit_code": code,
                    "output": output,
                }
            )
        point = None
        warnings: list[str] = []
        if restore_point == "try" and not dry_run:
            # System Protection is the FakeTools option `restore_enabled`; off means no point.
            if getattr(self, "_restore_enabled", True) is False:
                warnings.append(f"restore point unavailable: {PROTECTION_OFF}")
            else:
                point = {
                    "sequence": 9,
                    "description": "Cairn: network: reset",
                    "created_at": RECORDED_AT,
                }
        return {
            "dry_run": dry_run,
            "session_id": None if dry_run else 1,
            "restore_point": point,
            "steps": steps,
            "manual_settings": self._manual_settings(),
            "restart_required": True,
            "warnings": warnings,
        }

    # -- DNS journal hooks -------------------------------------------------------------

    def _dns_active_count(self) -> int:
        """Number of active DNS records (one per adapter and address family)."""
        return len(self._dns_baseline)

    def _dns_export(self) -> list[dict[str, Any]]:
        """Journal export rows of the DNS records, shaped like the engine's `dns` entries."""
        return copy.deepcopy(self._dns_records)

    def _dns_revert(self, guids: Iterable[str] | None, dry_run: bool) -> tuple[list[str], int]:
        """Restores the DNS records of `guids` (every active one for None).

        Returns the action lines and the number of records restored (0 in a dry run).
        """
        wanted = None if guids is None else {canonical_guid(g) for g in guids}
        records = [
            r
            for r in reversed(self._dns_records)
            if r["active"] and (wanted is None or r["interface_guid"] in wanted)
        ]
        actions = []
        for record in records:
            previous = record["previous_servers"]
            actions.append(f"restore {record['target']} to {_servers_text(previous)}")
            if dry_run:
                continue
            key = (record["interface_guid"], record["family"])
            if previous:
                self._static_dns[key] = list(previous)
            else:
                self._static_dns.pop(key, None)
            self._dns_baseline.pop(key, None)
            record["active"] = False
            record["reverted_at"] = RECORDED_AT
        return actions, 0 if dry_run else len(records)
