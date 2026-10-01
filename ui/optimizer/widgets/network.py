"""Network section: every adapter with its status, addresses and DNS servers, a DNS menu per
adapter, and the DNS cache, DHCP lease and network stack reset actions.

The pure helpers format the engine's `network_*` results; the widgets never call the
engine themselves, they hand every action to the callbacks they were built with.
"""

from __future__ import annotations

import ipaddress
import logging
import re
import tkinter as tk
from collections.abc import Callable, Iterable, Mapping, Sequence
from typing import Any

import customtkinter as ctk

from .. import theme
from .brand import set_window_icon

log = logging.getLogger(__name__)

AdapterCallback = Callable[[dict[str, Any]], None]

# Menu label of the "automatic" preset and of a configuration that no preset describes.
AUTOMATIC_LABEL = "Automatic (DHCP)"
CUSTOM_LABEL = "Custom"
# Last entry of every DNS menu; it maps to the preset id "custom".
CUSTOM_CHOICE = "Custom…"
CUSTOM_PRESET = "custom"
FAMILIES = (("ipv4", "IPv4"), ("ipv6", "IPv6"))
# Most DNS servers per address family, as the engine accepts them.
MAX_SERVERS = 4
MAX_LISTED_ADDRESSES = 3
MAX_WARNINGS = 3
# Output lines of a failed reset step repeated in the result dialog.
MAX_OUTPUT_LINES = 20

KIND_LABELS = {
    "ethernet": "Ethernet",
    "wifi": "Wi-Fi",
    "cellular": "Cellular",
    "bluetooth": "Bluetooth",
    "vpn": "VPN",
    "virtual": "Virtual",
    "tunnel": "Tunnel",
    "other": "Other",
}
# status -> (icon, label, colour)
STATUS_STYLE = {
    "connected": ("✓", "Connected", theme.GOOD),
    "disconnected": ("○", "Disconnected", theme.INK_MUTED),
    "not_present": ("–", "Not present", theme.INK_MUTED),
    "unknown": ("–", "Status unknown", theme.INK_MUTED),
}
LIMITED_STYLE = ("⚠", "Connected, no address from the network", theme.WARNING)
ORIGIN_LABELS = {
    "manual": "manual",
    "dhcp": "DHCP",
    "autoconfigured": "autoconfigured",
    "temporary": "temporary",
    "link_local": "link-local",
}
STEP_STATUS_TEXT = {
    "planned": "planned",
    "succeeded": "done",
    "completed_with_errors": "finished with errors",
    "failed": "failed",
    "timed_out": "did not finish within a minute",
    "not_run": "not run",
}
# Step states whose output is repeated in the result dialog.
PROBLEM_STEPS = ("failed", "timed_out", "completed_with_errors")

POLICY_WARNING = "⚠ DNS servers are set by policy ({servers}); adapter DNS settings have no effect."
VPN_WARNING = "⚠ A VPN is connected, so DNS changes are paused until it disconnects."
ADMIN_NOTE = "Changing DNS or renewing needs administrator rights."
RESET_CAVEAT = (
    "Resets Winsock and TCP/IP to Windows defaults. Manual IP and DNS settings are lost, a restart is "
    "needed, and it cannot be undone."
)
CUSTOM_HINT = "Separate addresses with commas. Leave a field empty to keep that family's current setting."
PLACEHOLDER = "Adapters, their addresses and DNS servers appear here."
# Reset plan without manual settings. The engine also reads the settings Windows keeps for
# unplugged and disabled adapters, and says so in a warning when it cannot.
NO_MANUAL_SETTINGS = "No manual IP addresses or DNS servers were found."

_SEPARATORS = re.compile(r"[\s,;]+")
_BROADCAST = ipaddress.IPv4Address("255.255.255.255")


def _font(size: int, weight: str = "normal") -> ctk.CTkFont:
    return ctk.CTkFont(family=theme.FONT_FAMILY, size=size, weight=weight)


def plural(count: int, noun: str) -> str:
    return f"{count} {noun}{'' if count == 1 else 's'}"


def canonical_id(value: str | None) -> str:
    """Interface GUID as the engine reports it: braces around the lowercase GUID."""
    text = (value or "").strip().strip("{}").lower()
    return "{" + text + "}" if text else ""


def family_label(family: str | None) -> str:
    return "IPv6" if family == "ipv6" else "IPv4"


def _trim(value: float) -> str:
    text = f"{value:.1f}"
    return text[:-2] if text.endswith(".0") else text


def speed_text(bps: int | None) -> str:
    """Link speed: "1.2 Gbps", "100 Mbps", "3 Mbps"; "" when unknown."""
    if not bps or bps <= 0:
        return ""
    if bps >= 1_000_000_000:
        return f"{_trim(bps / 1e9)} Gbps"
    if bps >= 1_000_000:
        mbps = bps / 1e6
        return f"{mbps:.0f} Mbps" if mbps >= 10 else f"{_trim(mbps)} Mbps"
    return f"{bps / 1e3:.0f} kbps"


def _address_text(address: Mapping[str, Any]) -> str:
    text = str(address.get("address", ""))
    prefix = address.get("prefix_length")
    if prefix is not None:
        text += f"/{prefix}"
    origin = ORIGIN_LABELS.get(str(address.get("origin")))
    return f"{text} ({origin})" if origin else text


def address_lines(adapter: Mapping[str, Any]) -> list[str]:
    """One line per address family: its addresses with their origin and the family's gateways.

    Only addresses in use are listed (`preferred`): an adapter that is not connected keeps
    tentative addresses Windows cannot use, and gateways are listed only while it is
    connected. A family that is turned off says so; a connected adapter without an address of
    an enabled family says "no address"; a disconnected one leaves the family out.
    """
    lines: list[str] = []
    gateways = [str(g) for g in adapter.get("gateways") or []]
    connected = adapter.get("status") == "connected"
    for family, label in FAMILIES:
        if not adapter.get(f"{family}_enabled", True):
            lines.append(f"{label}: turned off")
            continue
        addresses = [a for a in adapter.get(family) or [] if a.get("preferred", True)]
        parts = [_address_text(a) for a in addresses[:MAX_LISTED_ADDRESSES]]
        if len(addresses) > MAX_LISTED_ADDRESSES:
            parts.append(f"{len(addresses) - MAX_LISTED_ADDRESSES} more")
        if not parts:
            if connected:
                lines.append(f"{label}: no address")
            continue
        line = f"{label}: " + ", ".join(parts)
        family_gateways = [g for g in gateways if (":" in g) == (family == "ipv6")] if connected else []
        if family_gateways:
            line += "  ·  gateway " + ", ".join(family_gateways)
        lines.append(line)
    return lines


def preset_title(presets: Iterable[Mapping[str, Any]], preset_id: str | None) -> str | None:
    if not preset_id:
        return None
    for preset in presets:
        if preset.get("id") == preset_id:
            return str(preset.get("title") or preset_id)
    return None


def config_text(config: Mapping[str, Any]) -> str:
    """One family's DNS configuration without preset names, as plan lines show it:
    "automatic (192.168.0.1)", "1.1.1.1, 1.0.0.1"."""
    mode = config.get("mode")
    servers = ", ".join(str(s) for s in config.get("servers") or [])
    if mode == "automatic":
        return f"automatic ({servers})" if servers else "automatic"
    if mode == "manual":
        return servers or "automatic"
    if mode == "profile":
        profile = ", ".join(str(s) for s in config.get("profile_servers") or []) or servers
        return f"set for this Wi-Fi network ({profile})" if profile else "set for this Wi-Fi network"
    return "unknown"


def family_dns_text(config: Mapping[str, Any], presets: Iterable[Mapping[str, Any]] = ()) -> str:
    """One family's DNS configuration as the adapter row shows it."""
    mode = config.get("mode")
    servers = ", ".join(str(s) for s in config.get("servers") or [])
    if mode == "automatic":
        return f"automatic ({servers})" if servers else "automatic"
    if mode == "manual":
        title = preset_title(presets, config.get("preset"))
        if title and servers:
            return f"{servers} ({title})"
        return servers or "automatic"
    if mode == "profile":
        profile = ", ".join(str(s) for s in config.get("profile_servers") or []) or servers
        return f"Set for this Wi-Fi network: {profile}"
    return "could not be read"


def dns_text(adapter: Mapping[str, Any], presets: Iterable[Mapping[str, Any]] = ()) -> str:
    """DNS line of an adapter: "IPv4 DNS: automatic (192.168.0.1)  ·  IPv6 DNS: automatic".

    Families that are turned off are left out.
    """
    presets = list(presets)
    parts = [
        f"{label} DNS: {family_dns_text(adapter.get(f'dns_{family}') or {}, presets)}"
        for family, label in FAMILIES
        if adapter.get(f"{family}_enabled", True)
    ]
    return "  ·  ".join(parts) if parts else "DNS: none (IPv4 and IPv6 are turned off)"


def choice_label(
    ipv4: Mapping[str, Any] | None,
    ipv6: Mapping[str, Any] | None,
    presets: Iterable[Mapping[str, Any]],
) -> str:
    """Name of a DNS configuration: "Automatic (DHCP)", a preset title, "<title> (IPv4 only)"
    or "Custom".

    `ipv4` and `ipv6` are the families' DNS configurations; None leaves a family out (turned
    off, or not part of a change).
    """
    configs = [c for c in (ipv4, ipv6) if c is not None]
    if not configs:
        return CUSTOM_LABEL
    if all(c.get("mode") == "automatic" for c in configs):
        return AUTOMATIC_LABEL
    if ipv4 is not None and ipv4.get("mode") == "manual" and ipv4.get("preset"):
        preset = str(ipv4["preset"])
        title = preset_title(presets, preset) or preset
        if ipv6 is None or (ipv6.get("mode") == "manual" and ipv6.get("preset") == preset):
            return title
        if ipv6.get("mode") == "automatic":
            return f"{title} (IPv4 only)"
    if ipv4 is None and ipv6 is not None and ipv6.get("mode") == "manual" and ipv6.get("preset"):
        return preset_title(presets, str(ipv6["preset"])) or str(ipv6["preset"])
    return CUSTOM_LABEL


def adapter_choice(adapter: Mapping[str, Any], presets: Iterable[Mapping[str, Any]]) -> str:
    """Menu label of the adapter's current DNS configuration (families turned off left out)."""
    ipv4 = adapter.get("dns_ipv4") if adapter.get("ipv4_enabled", True) else None
    ipv6 = adapter.get("dns_ipv6") if adapter.get("ipv6_enabled", True) else None
    return choice_label(ipv4, ipv6, presets)


def skipped_changes(report: Mapping[str, Any], adapter: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    """The changes of a report that were skipped for a family turned on on `adapter`, for
    example automatic IPv4 DNS on an adapter with a manually set IPv4 address. A family
    turned off on the adapter is not worth naming."""
    return [
        c
        for c in report.get("changes") or []
        if c.get("outcome") == "skipped" and adapter.get(f"{c.get('family')}_enabled", True)
    ]


def report_choice_label(
    report: Mapping[str, Any],
    presets: Iterable[Mapping[str, Any]],
    adapter: Mapping[str, Any] | None = None,
) -> str:
    """Name of the DNS configuration a change report leaves: the targets of the families
    that were changed or already set and, with `adapter`, the unchanged configuration of the
    families skipped while turned on on it."""
    configs: dict[str, Mapping[str, Any]] = {}
    for change in report.get("changes") or []:
        if change.get("outcome") in ("applied", "already_set", "planned"):
            configs[str(change.get("family"))] = change.get("target") or {}
    if adapter is not None:
        for change in skipped_changes(report, adapter):
            configs[str(change.get("family"))] = change.get("previous") or {}
    return choice_label(configs.get("ipv4"), configs.get("ipv6"), presets)


def dns_done_text(
    report: Mapping[str, Any], adapter: Mapping[str, Any], presets: Iterable[Mapping[str, Any]]
) -> tuple[str, str]:
    """Status text and colour of a DNS change that was applied.

    The configuration the adapter is left with is named, including families that were
    skipped, and the skipped families are named as unchanged (the confirmed plan gave the
    reason); a skip or a warning turns the status into a warning.
    """
    name = str(report.get("adapter_name") or report.get("adapter_id") or "the adapter")
    text = f"Done: {name} now uses {report_choice_label(report, presets, adapter)} DNS"
    skipped = skipped_changes(report, adapter)
    labels = list(dict.fromkeys(family_label(c.get("family")) for c in skipped))
    if labels:
        text += f"; its {' and '.join(labels)} DNS servers were not changed"
    text += ". Undo it from History."
    warnings = [str(w) for w in report.get("warnings") or []]
    if warnings:
        text += f" {warnings[0]}"
    return text, theme.WARNING if skipped or warnings else theme.GOOD


def _server_error(item: str, family: str) -> str | None:
    label = family_label(family)
    if "%" in item:
        return f"{item} is not a valid {label} address"
    try:
        address: ipaddress.IPv4Address | ipaddress.IPv6Address = (
            ipaddress.IPv6Address(item) if family == "ipv6" else ipaddress.IPv4Address(item)
        )
    except ValueError:
        return f"{item} is not a valid {label} address"
    if address.is_unspecified:
        return f"{item} is the unspecified address, not a DNS server"
    if address.is_multicast:
        return f"{item} is a multicast address, not a DNS server"
    if address == _BROADCAST:
        return f"{item} is the broadcast address, not a DNS server"
    if isinstance(address, ipaddress.IPv6Address):
        first = int(address) >> 112
        if first & 0xFFC0 == 0xFE80:
            return f"{item} is a link-local address, which Windows can't use as a DNS server"
        if first & 0xFFC0 == 0xFEC0:
            return f"{item} is a site-local address, which Windows can't use as a DNS server"
    return None


def parse_server_list(text: str, family: str) -> tuple[list[str], str | None]:
    """Parses DNS servers of one family ("ipv4" or "ipv6") as the engine validates them.

    Addresses are separated by commas, semicolons or spaces. Unspecified, multicast and
    broadcast addresses and IPv6 link-local (fe80::/10) and site-local (fec0::/10) addresses
    are refused; loopback is allowed. Duplicates are dropped, keeping the order; at most four
    per family. Returns the normalized addresses and None, or an empty list and the reason.
    An empty text gives an empty list: that family is left unchanged.
    """
    servers: list[str] = []
    for item in _SEPARATORS.split(text.strip()):
        if not item:
            continue
        error = _server_error(item, family)
        if error is not None:
            return [], error
        value = str(ipaddress.ip_address(item))
        if value not in servers:
            servers.append(value)
    if len(servers) > MAX_SERVERS:
        return [], f"at most {MAX_SERVERS} {family_label(family)} DNS servers"
    return servers, None


def visible_adapters(adapters: Iterable[dict[str, Any]], show_all: bool) -> list[dict[str, Any]]:
    """The adapters listed: every one with `show_all`, else those not marked minor."""
    return [a for a in adapters if show_all or not a.get("minor")]


def change_line(change: Mapping[str, Any]) -> str:
    label = family_label(change.get("family"))
    outcome = change.get("outcome")
    detail = str(change.get("detail") or "")
    previous = config_text(change.get("previous") or {})
    target = config_text(change.get("target") or {})
    if outcome == "skipped":
        return f"• {label}: not changed: {detail}" if detail else f"• {label}: not changed"
    if outcome == "already_set":
        return f"• {label}: already set to {target}"
    if outcome == "failed":
        return f"• {label}: failed: {detail}" if detail else f"• {label}: failed"
    return f"• {label}: {previous} → {target}"


def plan_lines(report: Mapping[str, Any]) -> list[str]:
    """Detail lines of a DNS change report, for example
    "• IPv4: automatic (192.168.0.1) → 1.1.1.1, 1.0.0.1", followed by its warnings."""
    lines = [change_line(c) for c in report.get("changes") or []]
    lines += [f"⚠ {w}" for w in report.get("warnings") or []]
    return lines


def nothing_to_change_text(report: Mapping[str, Any], adapter: Mapping[str, Any]) -> tuple[str, str]:
    """Status text and colour of a DNS plan of `adapter` with no planned change.

    A family skipped for a reason other than being turned off on the adapter (for example
    automatic IPv4 DNS on a static address) is named; otherwise the adapter already uses the
    chosen servers.
    """
    name = str(adapter.get("name") or report.get("adapter_name") or "the adapter")
    changes = list(report.get("changes") or [])
    failed = [c for c in changes if c.get("outcome") == "failed"]
    if failed:
        detail = failed[0].get("detail") or "unknown error"
        return f"Could not read the DNS servers of {name}: {detail}", theme.WARNING
    reasons: list[str] = []
    for change in changes:
        detail = str(change.get("detail") or "")
        enabled = adapter.get(f"{change.get('family')}_enabled", True)
        if change.get("outcome") == "skipped" and enabled and detail and detail not in reasons:
            reasons.append(detail)
    if reasons:
        return f"Nothing to change on {name}: {'; '.join(reasons)}.", theme.INK_SECONDARY
    return f"Nothing to change: {name} already uses these DNS servers.", theme.INK_SECONDARY


def can_change_dns(adapter: Mapping[str, Any], engine_ready: bool, elevated: bool) -> bool:
    """Whether the DNS menu of `adapter` is offered: the engine allows it and the process is
    elevated."""
    return engine_ready and elevated and bool(adapter.get("can_change_dns"))


def can_renew(adapter: Mapping[str, Any], engine_ready: bool, elevated: bool) -> bool:
    """Whether Renew lease is offered for `adapter`."""
    return engine_ready and elevated and bool(adapter.get("can_renew"))


def status_badge(adapter: Mapping[str, Any]) -> tuple[str, str]:
    """Status text with its icon, and its colour."""
    status = adapter.get("status")
    if status == "connected" and adapter.get("limited"):
        icon, label, color = LIMITED_STYLE
    else:
        icon, label, color = STATUS_STYLE.get(str(status), STATUS_STYLE["unknown"])
    return f"{icon} {label}", color


def meta_text(adapter: Mapping[str, Any]) -> str:
    """Kind, description, link speed and MAC address, joined with "  ·  "."""
    parts = [KIND_LABELS.get(str(adapter.get("kind")), "Other")]
    if adapter.get("description"):
        parts.append(str(adapter["description"]))
    receive = speed_text(adapter.get("receive_bps"))
    transmit = speed_text(adapter.get("transmit_bps"))
    if receive and transmit and receive != transmit:
        parts.append(f"{receive} down, {transmit} up")
    elif receive or transmit:
        parts.append(receive or transmit)
    if adapter.get("mac"):
        parts.append(f"MAC {adapter['mac']}")
    return "  ·  ".join(parts)


def summary_text(report: Mapping[str, Any], show_all: bool = True) -> str:
    """Adapter and connection counts, the minor adapters hidden and the read time.

    Minor adapters are adapters without hardware of their own that are not connected
    (Wi-Fi Direct, Bluetooth networking, unused virtual switches).
    """
    adapters = list(report.get("adapters") or [])
    connected = sum(1 for a in adapters if a.get("status") == "connected")
    parts = [plural(len(adapters), "adapter"), f"{connected} connected"]
    hidden = 0 if show_all else sum(1 for a in adapters if a.get("minor"))
    if hidden:
        parts.append(f"{plural(hidden, 'idle virtual adapter')} hidden")
    duration = report.get("duration_ms")
    if isinstance(duration, int):
        parts.append(f"read in {duration} ms")
    return "  ·  ".join(parts)


def warning_lines(report: Mapping[str, Any]) -> list[str]:
    """Banner lines: a DNS policy, a connected VPN, then the engine's read warnings."""
    lines: list[str] = []
    policy = [str(s) for s in report.get("dns_policy") or []]
    if policy:
        lines.append(POLICY_WARNING.format(servers=", ".join(policy)))
    if report.get("vpn_connected"):
        lines.append(VPN_WARNING)
    warnings = [str(w) for w in report.get("warnings") or []]
    lines += [f"⚠ {w}" for w in warnings[:MAX_WARNINGS]]
    if len(warnings) > MAX_WARNINGS:
        lines.append(f"⚠ …and {len(warnings) - MAX_WARNINGS} more")
    return lines


def reset_details(plan: Mapping[str, Any]) -> list[str]:
    """Detail lines of the reset confirmation: the steps, the manual settings that are lost
    and the plan's warnings."""
    lines = ["Steps:"]
    for step in plan.get("steps") or []:
        command = step.get("command")
        lines.append(f"• {step.get('title')}" + (f"  ({command})" if command else ""))
    manual = list(plan.get("manual_settings") or [])
    if manual:
        lines.append("Manual settings that are lost (note them so you can enter them again):")
        lines += [f"• {m.get('adapter')}: {m.get('detail')}" for m in manual]
    else:
        lines.append(NO_MANUAL_SETTINGS)
    lines += [f"⚠ {w}" for w in plan.get("warnings") or []]
    return lines


def _step_status(step: Mapping[str, Any]) -> str:
    status = str(step.get("status"))
    text = STEP_STATUS_TEXT.get(status, status)
    code = step.get("exit_code")
    if status in ("failed", "completed_with_errors") and code is not None:
        text += f" (exit code {code})"
    return text


def reset_result_lines(report: Mapping[str, Any]) -> list[str]:
    """Result lines of a reset: the restore point outcome, warnings, each step's outcome and
    the manual settings to enter again ("Re-enter: <adapter>: <setting>")."""
    point = report.get("restore_point")
    if point:
        lines = [f"Restore point #{point.get('sequence')} was created before the reset."]
    else:
        lines = ["No restore point was created."]
    lines += [f"⚠ {w}" for w in report.get("warnings") or []]
    lines += [f"• {s.get('title')}: {_step_status(s)}" for s in report.get("steps") or []]
    lines += [f"Re-enter: {m.get('adapter')}: {m.get('detail')}" for m in report.get("manual_settings") or []]
    return lines


def failed_step_output_lines(report: Mapping[str, Any]) -> list[str]:
    """The last output lines of every step that failed, timed out or finished with errors."""
    lines: list[str] = []
    for step in report.get("steps") or []:
        output = str(step.get("output") or "").strip()
        if step.get("status") not in PROBLEM_STEPS or not output:
            continue
        lines.append(f"Output of {step.get('title')}:")
        lines += [f"    {line}" for line in output.splitlines()[-MAX_OUTPUT_LINES:]]
    return lines


def restore_point_sentence(restore_points: bool, enabled: bool | None) -> str:
    """Sentence of the reset confirmation about the restore point (with a leading space).

    `restore_points` is the window's restore point preference; `enabled` whether System
    Protection is on (None when it could not be read).
    """
    if not restore_points:
        return " No restore point is created."
    if enabled is None:
        return " Cairn couldn't check System Protection, so a restore point may not be created."
    if enabled:
        return " A restore point is created first."
    return " No restore point can be created because System Protection is off."


class AdapterRow(ctk.CTkFrame):
    """One adapter: status, kind, primary tag, description, speed, MAC, addresses and DNS
    servers, with its DNS menu, "Undo DNS change" and "Renew lease".

    `choose(preset_id)` is the single entry point of a DNS choice, from the menu or a test;
    the menu's "Custom…" maps to "custom". `reset_choice()` shows the adapter's current
    configuration in the menu again. The adapter's note (why DNS can't be changed here, or
    what may replace the servers) is shown in `hint_label`.
    """

    def __init__(
        self,
        master: tk.Misc,
        adapter: dict[str, Any],
        presets: Sequence[Mapping[str, Any]],
        *,
        engine_ready: bool,
        elevated: bool,
        on_dns: Callable[[dict[str, Any], str], None],
        on_renew: AdapterCallback,
        on_undo_dns: AdapterCallback,
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE_RAISED, corner_radius=8)
        self.adapter = adapter
        self.adapter_id = canonical_id(adapter.get("id"))
        self._on_dns = on_dns
        self._engine_ready = engine_ready
        self._elevated = elevated
        self._titles = {str(p["id"]): str(p["title"]) for p in presets if p.get("id") and p.get("title")}
        self._preset_ids = {title: preset_id for preset_id, title in self._titles.items()}
        self.choice = adapter_choice(adapter, presets)
        self.grid_columnconfigure(0, weight=1)

        head = ctk.CTkFrame(self, fg_color="transparent")
        head.grid(row=0, column=0, sticky="w", padx=12, pady=(8, 0))
        ctk.CTkLabel(
            head, text=adapter.get("name") or "Unnamed adapter", font=_font(12, "bold"), text_color=theme.INK
        ).pack(side="left")
        badge, color = status_badge(adapter)
        self.status_label = ctk.CTkLabel(head, text=badge, font=_font(11), text_color=color)
        self.status_label.pack(side="left", padx=(10, 0))
        self.primary_label: ctk.CTkLabel | None = None
        if adapter.get("primary"):
            self.primary_label = ctk.CTkLabel(
                head, text="Primary connection", font=_font(10, "bold"), text_color=theme.ACCENT
            )
            self.primary_label.pack(side="left", padx=(10, 0))

        lines = [(meta_text(adapter), theme.INK_SECONDARY)]
        lines += [(line, theme.INK_MUTED) for line in address_lines(adapter)]
        lines.append((dns_text(adapter, presets), theme.INK_SECONDARY))
        note = str(adapter.get("note") or "")
        labels: list[ctk.CTkLabel] = []
        for offset, (text, text_color) in enumerate(lines, start=1):
            label = ctk.CTkLabel(
                self,
                text=text,
                font=_font(10),
                text_color=text_color,
                anchor="w",
                justify="left",
                wraplength=600,
            )
            label.grid(row=offset, column=0, sticky="w", padx=12)
            labels.append(label)
        self.dns_label = labels[-1]
        hint_row = len(lines) + 1
        self.hint_label = ctk.CTkLabel(
            self,
            text=note,
            font=_font(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=600,
        )
        if note:
            self.hint_label.grid(row=hint_row, column=0, sticky="w", padx=12, pady=(2, 8))
        else:
            self.dns_label.grid_configure(pady=(0, 8))

        actions = ctk.CTkFrame(self, fg_color="transparent")
        actions.grid(row=0, column=1, rowspan=hint_row + 1, sticky="ne", padx=10, pady=8)
        self.dns_menu = ctk.CTkOptionMenu(
            actions,
            values=[*self._titles.values(), CUSTOM_CHOICE],
            width=240,
            height=28,
            font=_font(11),
            dropdown_font=_font(11),
            fg_color=theme.BUTTON_NEUTRAL,
            button_color=theme.BUTTON_NEUTRAL_HOVER,
            button_hover_color=theme.BASELINE,
            text_color=theme.INK,
            text_color_disabled=theme.INK_MUTED,
            dropdown_fg_color=theme.SURFACE_RAISED,
            dropdown_hover_color=theme.BUTTON_NEUTRAL_HOVER,
            dropdown_text_color=theme.INK,
            dynamic_resizing=False,
            command=self._menu_chosen,
        )
        self.dns_menu.set(self.choice)
        self.dns_menu.grid(row=0, column=0, columnspan=2, sticky="e")
        self.undo_button = ctk.CTkButton(
            actions,
            text="Undo DNS change",
            width=130,
            height=26,
            font=_font(11),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=lambda: on_undo_dns(self.adapter),
        )
        if adapter.get("dns_revertible"):
            self.undo_button.grid(row=1, column=0, sticky="e", padx=(0, 6), pady=(6, 0))
        self.renew_button = ctk.CTkButton(
            actions,
            text="Renew lease",
            width=100,
            height=26,
            font=_font(11),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=lambda: on_renew(self.adapter),
        )
        if adapter.get("can_renew"):
            self.renew_button.grid(row=1, column=1, sticky="e", pady=(6, 0))
        self.busy_label = ctk.CTkLabel(actions, text="", font=_font(10), text_color=theme.INK_SECONDARY)
        self.set_idle()

    @property
    def undo_shown(self) -> bool:
        return bool(self.undo_button.winfo_manager())

    @property
    def renew_shown(self) -> bool:
        return bool(self.renew_button.winfo_manager())

    def _title_for(self, preset_id: str) -> str:
        if preset_id == CUSTOM_PRESET:
            return CUSTOM_CHOICE
        return self._titles.get(preset_id, self.choice)

    def _menu_chosen(self, title: str) -> None:
        preset_id = CUSTOM_PRESET if title == CUSTOM_CHOICE else self._preset_ids.get(title)
        if preset_id is None:
            self.reset_choice()
            return
        self.choose(preset_id)

    def choose(self, preset_id: str) -> None:
        """Asks for the DNS configuration `preset_id` ("custom" for custom servers)."""
        self.dns_menu.set(self._title_for(preset_id))
        self._on_dns(self.adapter, preset_id)

    def reset_choice(self) -> None:
        self.dns_menu.set(self.choice)

    def set_access(self, engine_ready: bool, elevated: bool) -> None:
        self._engine_ready = engine_ready
        self._elevated = elevated

    def _set_actions(self, idle: bool) -> None:
        dns = idle and can_change_dns(self.adapter, self._engine_ready, self._elevated)
        renew = idle and can_renew(self.adapter, self._engine_ready, self._elevated)
        undo = idle and self._engine_ready and self._elevated
        self.dns_menu.configure(state="normal" if dns else "disabled")
        self.renew_button.configure(state="normal" if renew else "disabled")
        self.undo_button.configure(state="normal" if undo else "disabled")

    def set_busy(self, text: str | None = None) -> None:
        """Disables the row's actions; `text` says what runs on this adapter."""
        self._set_actions(False)
        if text:
            self.busy_label.configure(text=text)
            self.busy_label.grid(row=2, column=0, columnspan=2, sticky="e", pady=(4, 0))
        else:
            self.busy_label.configure(text="")
            self.busy_label.grid_forget()

    def set_idle(self) -> None:
        self.busy_label.configure(text="")
        self.busy_label.grid_forget()
        self._set_actions(True)


class CustomDnsDialog(ctk.CTkToplevel):
    """Modal dialog asking for custom IPv4 and IPv6 DNS servers of one adapter.

    The input is validated as it is typed (`error_label`); Continue stays disabled while
    either field is invalid or both are empty. Continue calls `on_submit(ipv4, ipv6)` with
    the normalized addresses, an empty list for a family left unchanged. Cancel, Escape and
    closing the window call `on_cancel`. A family turned off on the adapter can't be entered.
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        adapter: Mapping[str, Any],
        on_submit: Callable[[list[str], list[str]], None],
        on_cancel: Callable[[], None] | None = None,
        presets: Iterable[Mapping[str, Any]] = (),
    ) -> None:
        super().__init__(master, fg_color=theme.SURFACE)
        # The icon's photos, which Tk frees with the window; also stops CustomTkinter from
        # replacing the icon with its own a moment later.
        set_window_icon(self)
        name = str(adapter.get("name") or "this adapter")
        self.title_text = f"Custom DNS servers for {name}"
        self.title(self.title_text)
        self.resizable(False, False)
        self._on_submit = on_submit
        self._on_cancel = on_cancel
        self._closed = False
        self._enabled = {family: bool(adapter.get(f"{family}_enabled", True)) for family, _ in FAMILIES}
        self.grid_columnconfigure(1, weight=1)

        ctk.CTkLabel(
            self, text=self.title_text, font=_font(16, "bold"), text_color=theme.INK, anchor="w"
        ).grid(row=0, column=0, columnspan=2, sticky="w", padx=20, pady=(18, 4))
        ctk.CTkLabel(
            self,
            text=f"Current settings: {dns_text(adapter, presets)}",
            font=_font(12),
            text_color=theme.INK_SECONDARY,
            anchor="w",
            justify="left",
            wraplength=520,
        ).grid(row=1, column=0, columnspan=2, sticky="w", padx=20, pady=(0, 10))

        examples = {"ipv4": "for example 1.1.1.1, 1.0.0.1", "ipv6": "for example 2606:4700:4700::1111"}
        entries: dict[str, ctk.CTkEntry] = {}
        for row, (family, label) in enumerate(FAMILIES, start=2):
            ctk.CTkLabel(
                self, text=f"{label} DNS servers", font=_font(12), text_color=theme.INK_SECONDARY, anchor="w"
            ).grid(row=row, column=0, sticky="w", padx=(20, 10), pady=4)
            enabled = self._enabled[family]
            entry = ctk.CTkEntry(
                self,
                width=360,
                font=_font(12),
                fg_color=theme.PAGE,
                border_color=theme.BORDER,
                text_color=theme.INK,
                placeholder_text=examples[family] if enabled else f"{label} is turned off on this adapter",
                placeholder_text_color=theme.INK_MUTED,
            )
            if not enabled:
                # Disabled once it is built: an entry created disabled cannot show its placeholder.
                entry.configure(state="disabled")
            entry.grid(row=row, column=1, sticky="ew", padx=(0, 20), pady=4)
            entry.bind("<KeyRelease>", lambda _e: self._validate())
            entry.bind("<Return>", lambda _e: self._submit())
            entries[family] = entry
        self.ipv4_entry = entries["ipv4"]
        self.ipv6_entry = entries["ipv6"]

        ctk.CTkLabel(
            self,
            text=CUSTOM_HINT,
            font=_font(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=520,
        ).grid(row=4, column=0, columnspan=2, sticky="w", padx=20, pady=(4, 0))
        self.error_label = ctk.CTkLabel(
            self,
            text="",
            font=_font(11),
            text_color=theme.WARNING,
            anchor="w",
            justify="left",
            wraplength=520,
        )
        self.error_label.grid(row=5, column=0, columnspan=2, sticky="w", padx=20, pady=(4, 0))

        buttons = ctk.CTkFrame(self, fg_color="transparent")
        buttons.grid(row=6, column=0, columnspan=2, sticky="e", padx=20, pady=18)
        ctk.CTkButton(
            buttons,
            text="Cancel",
            width=100,
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            font=_font(12),
            command=self._cancel,
        ).pack(side="left", padx=(0, 8))
        self.continue_button = ctk.CTkButton(
            buttons,
            text="Continue",
            width=140,
            font=_font(12, "bold"),
            fg_color=theme.ACCENT,
            hover_color=theme.ACCENT_HOVER,
            state="disabled",
            command=self._submit,
        )
        self.continue_button.pack(side="left")

        self.protocol("WM_DELETE_WINDOW", self._cancel)
        self.bind("<Escape>", lambda _e: self._cancel())
        self.transient(master)
        self.after(10, self._grab)

    def _grab(self) -> None:
        try:
            self.lift()
            self.focus_force()
            self.grab_set()
        except tk.TclError:
            pass

    def _values(self) -> tuple[list[str], list[str], str | None]:
        parsed: dict[str, list[str]] = {}
        for family, entry in (("ipv4", self.ipv4_entry), ("ipv6", self.ipv6_entry)):
            if not self._enabled[family]:
                parsed[family] = []
                continue
            servers, error = parse_server_list(entry.get(), family)
            if error is not None:
                return [], [], error
            parsed[family] = servers
        return parsed["ipv4"], parsed["ipv6"], None

    def _validate(self) -> bool:
        """Shows the first input error and enables Continue when the input can be used."""
        ipv4, ipv6, error = self._values()
        self.error_label.configure(text=f"⚠ {error}" if error else "")
        valid = error is None and bool(ipv4 or ipv6)
        self.continue_button.configure(state="normal" if valid else "disabled")
        return valid

    def _close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            self.grab_release()
        except tk.TclError:
            pass
        self.destroy()

    def _submit(self) -> None:
        if self._closed or not self._validate():
            return
        ipv4, ipv6, _ = self._values()
        self._close()
        self._on_submit(ipv4, ipv6)

    def _cancel(self) -> None:
        if self._closed:
            return
        self._close()
        if self._on_cancel is not None:
            self._on_cancel()


class NetworkPanel(ctk.CTkFrame):
    """The adapter list with Refresh, Flush DNS cache, Show all and Reset network stack.

    `show` lists a `network_list` report; it never raises and reports a malformed report
    through `show_error`. Enablement: the DNS menus, Renew, Undo and Reset need a loaded
    engine and administrator rights, a DNS menu also the adapter's `can_change_dns`;
    Flush needs only the engine. Everything is disabled between `set_busy` and `set_idle`.
    """

    def __init__(
        self,
        master: tk.Misc,
        *,
        on_refresh: Callable[[], None],
        on_flush: Callable[[], None],
        on_reset: Callable[[], None],
        on_dns: Callable[[dict[str, Any], str], None],
        on_renew: AdapterCallback,
        on_undo_dns: AdapterCallback,
    ) -> None:
        super().__init__(
            master, fg_color=theme.SURFACE, corner_radius=10, border_width=1, border_color=theme.BORDER
        )
        self._on_dns = on_dns
        self._on_renew = on_renew
        self._on_undo_dns = on_undo_dns
        self.loaded = False
        self.rows: list[AdapterRow] = []
        self.warnings: list[str] = []
        self._report: dict[str, Any] | None = None
        self._presets: list[Mapping[str, Any]] = []
        self._engine_ready = True
        self._elevated = False
        self._loading = False
        self._unsupported = False
        self._busy_adapter: str | None = None
        self._busy_text: str | None = None
        self._empty_label: ctk.CTkLabel | None = None
        self.grid_columnconfigure(0, weight=1)
        self.grid_rowconfigure(4, weight=1)

        header = ctk.CTkFrame(self, fg_color="transparent")
        header.grid(row=0, column=0, sticky="ew", padx=14, pady=(10, 0))
        header.grid_columnconfigure(1, weight=1)
        ctk.CTkLabel(
            header, text="Network adapters", font=_font(13, "bold"), text_color=theme.INK_SECONDARY
        ).grid(row=0, column=0, sticky="w")
        self.busy_label = ctk.CTkLabel(header, text="", font=_font(11), text_color=theme.INK_SECONDARY)
        self.busy_label.grid(row=0, column=1, sticky="e", padx=(0, 12))
        self.show_all_switch = ctk.CTkSwitch(
            header,
            text="Show all",
            font=_font(11),
            text_color=theme.INK_SECONDARY,
            progress_color=theme.ACCENT,
            button_color=theme.INK,
            button_hover_color=theme.INK_SECONDARY,
            fg_color=theme.BASELINE,
            command=self._show_all_changed,
        )
        self.show_all_switch.grid(row=0, column=2, sticky="e", padx=(0, 12))
        self.flush_button = ctk.CTkButton(
            header,
            text="Flush DNS cache",
            width=130,
            height=30,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_flush,
        )
        self.flush_button.grid(row=0, column=3, sticky="e", padx=(0, 8))
        self.refresh_button = ctk.CTkButton(
            header,
            text="Refresh",
            width=100,
            height=30,
            font=_font(12),
            fg_color=theme.BUTTON_NEUTRAL,
            hover_color=theme.BUTTON_NEUTRAL_HOVER,
            command=on_refresh,
        )
        self.refresh_button.grid(row=0, column=4, sticky="e")

        self.summary = ctk.CTkLabel(
            self,
            text=PLACEHOLDER,
            font=_font(11),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=900,
        )
        self.summary.grid(row=1, column=0, sticky="ew", padx=14, pady=(2, 4))
        self.warning_label = ctk.CTkLabel(
            self,
            text="",
            font=_font(11),
            text_color=theme.WARNING,
            anchor="w",
            justify="left",
            wraplength=900,
        )
        self.admin_label = ctk.CTkLabel(
            self, text="", font=_font(11), text_color=theme.INK_MUTED, anchor="w", justify="left"
        )

        self.list = ctk.CTkScrollableFrame(
            self,
            fg_color=theme.SURFACE,
            scrollbar_button_color=theme.BASELINE,
            scrollbar_button_hover_color=theme.INK_MUTED,
        )
        self.list.grid(row=4, column=0, sticky="nsew", padx=6, pady=(0, 6))
        self.list.grid_columnconfigure(0, weight=1)

        footer = ctk.CTkFrame(self, fg_color="transparent")
        footer.grid(row=5, column=0, sticky="ew", padx=14, pady=(0, 10))
        footer.grid_columnconfigure(0, weight=1)
        self.caveat_label = ctk.CTkLabel(
            footer,
            text=RESET_CAVEAT,
            font=_font(10),
            text_color=theme.INK_MUTED,
            anchor="w",
            justify="left",
            wraplength=700,
        )
        self.caveat_label.grid(row=0, column=0, sticky="w", padx=(0, 12))
        self.reset_button = ctk.CTkButton(
            footer,
            text="Reset network stack…",
            width=180,
            height=30,
            font=_font(12, "bold"),
            fg_color=theme.CRITICAL,
            hover_color=theme.CRITICAL_HOVER,
            text_color=theme.INK,
            command=on_reset,
        )
        self.reset_button.grid(row=0, column=1, sticky="e")
        self._apply_state()

    # -- queries -------------------------------------------------------------------

    @property
    def row_count(self) -> int:
        return len(self.rows)

    @property
    def show_all(self) -> bool:
        return bool(self.show_all_switch.get())

    def row(self, adapter_id: str) -> AdapterRow | None:
        """The listed row of the adapter with this interface GUID (braces and case ignored)."""
        wanted = canonical_id(adapter_id)
        return next((r for r in self.rows if r.adapter_id == wanted), None)

    # -- state ---------------------------------------------------------------------

    def set_access(self, engine_ready: bool, elevated: bool) -> None:
        """Records whether the engine is loaded and the process elevated, and applies it."""
        self._engine_ready = engine_ready
        self._elevated = elevated
        self._apply_state()

    def set_loading(self) -> None:
        """A reload runs; the listed rows stay until the new report arrives."""
        self._loading = True
        if not self.loaded:
            self.summary.configure(text="Reading network adapters…", text_color=theme.INK_MUTED)
        self._apply_state()

    def show_error(self, message: str) -> None:
        """Replaces the list with the error; stale rows are not left actionable."""
        self._loading = False
        self._report = None
        self._clear_rows()
        self._show_warnings([])
        self.summary.configure(
            text=f"Could not read the network adapters: {message}", text_color=theme.CRITICAL
        )
        self._apply_state()

    def set_unsupported(self, text: str) -> None:
        """Disables the whole section with `text` as the reason (engine missing or outdated)."""
        self._unsupported = True
        self._loading = False
        self._report = None
        self._clear_rows()
        self._show_warnings([])
        self.summary.configure(text=f"⚠ {text}", text_color=theme.WARNING)
        self._apply_state()

    def show(
        self,
        report: dict[str, Any],
        presets: Sequence[Mapping[str, Any]],
        engine_ready: bool,
        elevated: bool,
    ) -> None:
        """Lists the adapters of a `network_list` report with the DNS menus of `presets`."""
        self._loading = False
        self._engine_ready = engine_ready
        self._elevated = elevated
        adapters = report.get("adapters") if isinstance(report, dict) else None
        if not isinstance(adapters, list):
            self.show_error("the engine's report has no adapter list")
            return
        self._report = report
        self._presets = list(presets)
        self.loaded = True
        self._render_report()

    def set_busy(self, adapter_id: str | None, text: str) -> None:
        """Disables every action; `text` shows on the adapter's row, or in the header for
        `adapter_id` None."""
        self._busy_adapter = canonical_id(adapter_id) if adapter_id else None
        self._busy_text = text
        self._apply_state()

    def set_idle(self) -> None:
        self._busy_adapter = None
        self._busy_text = None
        self._apply_state()

    # -- rendering -----------------------------------------------------------------

    def _clear_rows(self) -> None:
        # Every child of the list is a row or the empty note, including a row whose
        # construction failed halfway on malformed adapter data.
        for child in self.list.winfo_children():
            child.destroy()
        self.rows.clear()
        self._empty_label = None

    def _show_all_changed(self) -> None:
        if self._report is not None and not self._unsupported:
            self._render_report()

    def _render_report(self) -> None:
        """Lists the last report; malformed adapter data is reported in the panel."""
        try:
            self._render()
        except Exception as exc:  # noqa: BLE001 - malformed engine data is reported in the panel, never raised
            log.exception("network report could not be shown")
            self.show_error(f"unexpected adapter data ({exc})")

    def _show_warnings(self, lines: list[str]) -> None:
        self.warnings = lines
        if not lines:
            self.warning_label.configure(text="")
            self.warning_label.grid_forget()
            return
        self.warning_label.configure(text="\n".join(lines))
        self.warning_label.grid(row=2, column=0, sticky="ew", padx=14, pady=(0, 4))

    def _render(self) -> None:
        report = self._report or {}
        self._clear_rows()
        adapters = visible_adapters(report.get("adapters") or [], self.show_all)
        self.summary.configure(text=summary_text(report, self.show_all), text_color=theme.INK_MUTED)
        self._show_warnings(warning_lines(report))
        for i, adapter in enumerate(adapters):
            row = AdapterRow(
                self.list,
                adapter,
                self._presets,
                engine_ready=self._engine_ready,
                elevated=self._elevated,
                on_dns=self._on_dns,
                on_renew=self._on_renew,
                on_undo_dns=self._on_undo_dns,
            )
            row.grid(row=i, column=0, sticky="ew", padx=6, pady=4)
            self.rows.append(row)
        if not adapters:
            self._empty_label = ctk.CTkLabel(
                self.list,
                text="No network adapters were found.",
                font=_font(11),
                text_color=theme.INK_MUTED,
            )
            self._empty_label.grid(row=0, column=0, pady=20, padx=8)
        self._apply_state()

    def _apply_state(self) -> None:
        if self._unsupported:
            for widget in (self.refresh_button, self.flush_button, self.reset_button, self.show_all_switch):
                widget.configure(state="disabled")
            self.busy_label.configure(text="")
            self.admin_label.grid_forget()
            return
        idle = self._busy_text is None
        ready = self._engine_ready
        self.refresh_button.configure(state="normal" if ready and idle and not self._loading else "disabled")
        self.flush_button.configure(state="normal" if ready and idle else "disabled")
        self.reset_button.configure(state="normal" if ready and idle and self._elevated else "disabled")
        self.show_all_switch.configure(state="normal")
        header_text = self._busy_text if not idle and self._busy_adapter is None else ""
        self.busy_label.configure(text=header_text or "")
        if ready and not self._elevated:
            self.admin_label.configure(text=ADMIN_NOTE)
            self.admin_label.grid(row=3, column=0, sticky="ew", padx=14, pady=(0, 4))
        else:
            self.admin_label.configure(text="")
            self.admin_label.grid_forget()
        for row in self.rows:
            row.set_access(ready, self._elevated)
            if idle:
                row.set_idle()
            else:
                row.set_busy(self._busy_text if row.adapter_id == self._busy_adapter else None)
