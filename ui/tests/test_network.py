"""Pure helpers of the Network section: speeds, addresses, DNS texts and choices, server list
validation, plan and reset texts, and the fake engine's network model.

No window is created and no native module is loaded.
"""

from __future__ import annotations

import copy
import re
from typing import Any

import pytest

from optimizer import theme
from optimizer.widgets.network import (
    ADMIN_NOTE,
    AUTOMATIC_LABEL,
    POLICY_WARNING,
    VPN_WARNING,
    adapter_choice,
    address_lines,
    can_change_dns,
    can_renew,
    canonical_id,
    choice_label,
    config_text,
    dns_done_text,
    dns_text,
    failed_step_output_lines,
    meta_text,
    nothing_to_change_text,
    parse_server_list,
    plan_lines,
    report_choice_label,
    reset_details,
    reset_result_lines,
    restore_point_sentence,
    speed_text,
    status_badge,
    summary_text,
    visible_adapters,
    warning_lines,
)

from .fake_engine import FakeEngine
from .fake_network import (
    ADAPTERS,
    DEFAULT_SWITCH,
    DNS_PRESETS,
    ETHERNET,
    PROTECTION_OFF,
    PROTON_VPN,
    WIFI,
    WIFI_DIRECT,
)
from .fake_network import RESET_STEPS as FAKE_RESET_STEPS

PRESETS = list(DNS_PRESETS)


def automatic(*servers: str) -> dict[str, Any]:
    return {"mode": "automatic", "servers": list(servers), "preset": None, "profile_servers": []}


def manual(*servers: str, preset: str | None = None) -> dict[str, Any]:
    return {"mode": "manual", "servers": list(servers), "preset": preset, "profile_servers": []}


def profile(*servers: str) -> dict[str, Any]:
    return {"mode": "profile", "servers": list(servers), "preset": None, "profile_servers": list(servers)}


def adapter(adapter_id: str, **fields: Any) -> dict[str, Any]:
    base = copy.deepcopy(next(a for a in ADAPTERS if a["id"] == adapter_id))
    base.update(fields)
    return base


def test_speed_text() -> None:
    assert speed_text(1_200_000_000) == "1.2 Gbps"
    assert speed_text(1_000_000_000) == "1 Gbps"
    assert speed_text(2_500_000_000) == "2.5 Gbps"
    assert speed_text(100_000_000) == "100 Mbps"
    assert speed_text(866_700_000) == "867 Mbps"
    assert speed_text(3_000_000) == "3 Mbps"
    assert speed_text(1_500_000) == "1.5 Mbps"
    assert speed_text(56_000) == "56 kbps"
    assert speed_text(0) == ""
    assert speed_text(None) == ""


def test_address_lines_show_origin_gateway_and_turned_off_families() -> None:
    wifi = adapter(WIFI)
    assert address_lines(wifi) == ["IPv4: 192.168.0.23/24 (DHCP)  ·  gateway 192.168.0.1", "IPv6: turned off"]
    switch = adapter(DEFAULT_SWITCH)
    assert address_lines(switch) == ["IPv4: 172.28.64.1/20 (manual)", "IPv6: fe80::5d:3/64 (link-local)"]
    # A disconnected adapter holds only tentative addresses, which are not listed.
    ethernet = adapter(ETHERNET)
    assert [a["preferred"] for a in ethernet["ipv4"] + ethernet["ipv6"]] == [False, False]
    assert address_lines(ethernet) == []
    connected = adapter(ETHERNET, status="connected")
    assert address_lines(connected) == ["IPv4: no address", "IPv6: no address"]
    in_use = {"address": "192.168.1.20", "prefix_length": 24, "origin": "manual", "preferred": True}
    mixed = adapter(ETHERNET, status="connected", ipv4=[*ethernet["ipv4"], in_use], ipv6_enabled=False)
    assert address_lines(mixed) == ["IPv4: 192.168.1.20/24 (manual)", "IPv6: turned off"]
    # A gateway left from an old lease is listed only while the adapter is connected.
    routed = dict(mixed, gateways=["192.168.1.1"])
    assert address_lines(routed)[0] == "IPv4: 192.168.1.20/24 (manual)  ·  gateway 192.168.1.1"
    assert address_lines(dict(routed, status="disconnected"))[0] == "IPv4: 192.168.1.20/24 (manual)"
    # An address without the flag counts as in use.
    unflagged = adapter(ETHERNET, ipv4=[{"address": "10.0.0.2", "prefix_length": 8}], ipv6=[])
    assert address_lines(unflagged) == ["IPv4: 10.0.0.2/8"]
    many = adapter(
        ETHERNET,
        status="connected",
        ipv4=[],
        ipv6=[
            {"address": f"2001:db8::{n}", "prefix_length": 64, "origin": "temporary", "preferred": True}
            for n in range(1, 6)
        ],
        gateways=["192.168.1.1", "fe80::1"],
    )
    assert address_lines(many) == [
        "IPv4: no address",
        "IPv6: 2001:db8::1/64 (temporary), 2001:db8::2/64 (temporary), 2001:db8::3/64 (temporary), "
        "2 more  ·  gateway fe80::1",
    ]


def test_meta_text_and_status_badge() -> None:
    wifi = adapter(WIFI)
    assert meta_text(wifi) == "Wi-Fi  ·  Contoso Wi-Fi 6E Adapter  ·  1.2 Gbps  ·  MAC 00-00-5E-00-53-01"
    asymmetric = adapter(WIFI, transmit_bps=600_000_000)
    assert "1.2 Gbps down, 600 Mbps up" in meta_text(asymmetric)
    assert meta_text(adapter(PROTON_VPN)) == "VPN  ·  ProtonVPN Tunnel"
    assert status_badge(wifi) == ("✓ Connected", theme.GOOD)
    assert status_badge(adapter(ETHERNET)) == ("○ Disconnected", theme.INK_MUTED)
    assert status_badge(adapter(ETHERNET, status="not_present")) == ("– Not present", theme.INK_MUTED)
    text, color = status_badge(adapter(WIFI, limited=True))
    assert text.startswith("⚠ ") and color == theme.WARNING


def test_dns_text_per_family_with_preset_names_and_profile() -> None:
    wifi = adapter(WIFI, dns_ipv4=automatic("192.168.0.1"))
    assert dns_text(wifi, PRESETS) == "IPv4 DNS: automatic (192.168.0.1)"
    both = adapter(
        ETHERNET,
        dns_ipv4=manual("1.1.1.1", "1.0.0.1", preset="cloudflare"),
        dns_ipv6=manual("2001:db8::53"),
    )
    assert dns_text(both, PRESETS) == "IPv4 DNS: 1.1.1.1, 1.0.0.1 (Cloudflare)  ·  IPv6 DNS: 2001:db8::53"
    per_network = adapter(WIFI, dns_ipv4=profile("9.9.9.9"))
    assert dns_text(per_network, PRESETS) == "IPv4 DNS: Set for this Wi-Fi network: 9.9.9.9"
    unreadable = adapter(ETHERNET, dns_ipv4={"mode": "unknown", "servers": [], "preset": None})
    assert "IPv4 DNS: could not be read" in dns_text(unreadable, PRESETS)
    assert config_text(automatic("192.168.0.1", "198.51.100.53")) == "automatic (192.168.0.1, 198.51.100.53)"
    assert config_text(manual("1.1.1.1", "1.0.0.1", preset="cloudflare")) == "1.1.1.1, 1.0.0.1"
    assert config_text(profile("9.9.9.9")) == "set for this Wi-Fi network (9.9.9.9)"


def test_choice_label() -> None:
    cloudflare_v4 = manual("1.1.1.1", "1.0.0.1", preset="cloudflare")
    cloudflare_v6 = manual("2606:4700:4700::1111", "2606:4700:4700::1001", preset="cloudflare")
    assert choice_label(automatic("192.168.0.1"), automatic(), PRESETS) == AUTOMATIC_LABEL
    assert choice_label(cloudflare_v4, cloudflare_v6, PRESETS) == "Cloudflare"
    assert choice_label(cloudflare_v4, None, PRESETS) == "Cloudflare"
    assert choice_label(cloudflare_v4, automatic(), PRESETS) == "Cloudflare (IPv4 only)"
    google_v6 = manual("2001:4860:4860::8888", "2001:4860:4860::8844", preset="google")
    assert choice_label(cloudflare_v4, google_v6, PRESETS) == "Custom"
    assert choice_label(manual("192.168.0.53"), automatic(), PRESETS) == "Custom"
    assert choice_label(profile("9.9.9.9"), automatic(), PRESETS) == "Custom"
    assert choice_label(None, None, PRESETS) == "Custom"
    # Families that are turned off do not count.
    ipv6_off = adapter(WIFI, dns_ipv4=cloudflare_v4, dns_ipv6=automatic())
    assert adapter_choice(ipv6_off, PRESETS) == "Cloudflare"
    assert adapter_choice(adapter(WIFI), PRESETS) == AUTOMATIC_LABEL


def test_report_choice_label_uses_the_changed_families() -> None:
    quad9_v4 = manual("9.9.9.9", "149.112.112.112", preset="quad9")
    quad9_v6 = manual("2620:fe::fe", "2620:fe::9", preset="quad9")
    report = {
        "changes": [
            {"family": "ipv4", "outcome": "applied", "target": quad9_v4},
            {"family": "ipv6", "outcome": "skipped", "target": quad9_v6},
        ]
    }
    assert report_choice_label(report, PRESETS) == "Quad9 (blocks malware)"
    both_automatic = {
        "changes": [
            {"family": "ipv4", "outcome": "already_set", "target": automatic()},
            {"family": "ipv6", "outcome": "applied", "target": automatic()},
        ]
    }
    assert report_choice_label(both_automatic, PRESETS) == AUTOMATIC_LABEL


REASON_STATIC_IPV4 = (
    "this adapter has a manually set IPv4 address, so it would get no IPv4 DNS servers automatically"
)


def skipped_ipv4_report(warnings: list[str] | None = None) -> dict[str, Any]:
    """Automatic DNS chosen on an adapter with a static IPv4 address that used Cloudflare."""
    return {
        "adapter_id": DEFAULT_SWITCH,
        "adapter_name": "vEthernet (Default Switch)",
        "changes": [
            {
                "family": "ipv4",
                "outcome": "skipped",
                "detail": REASON_STATIC_IPV4,
                "previous": manual("1.1.1.1", "1.0.0.1", preset="cloudflare"),
                "target": automatic(),
            },
            {
                "family": "ipv6",
                "outcome": "applied",
                "previous": manual("2606:4700:4700::1111", "2606:4700:4700::1001", preset="cloudflare"),
                "target": automatic(),
            },
        ],
        "warnings": warnings or [],
    }


def test_report_choice_label_names_the_configuration_a_skip_leaves() -> None:
    report = skipped_ipv4_report()
    switch = adapter(DEFAULT_SWITCH)
    assert report_choice_label(report, PRESETS, switch) == "Cloudflare (IPv4 only)"
    # Without the adapter only the changed families count.
    assert report_choice_label(report, PRESETS) == AUTOMATIC_LABEL
    # A family turned off on the adapter is left out.
    wifi = adapter(WIFI)
    cloudflare_v4 = manual("1.1.1.1", "1.0.0.1", preset="cloudflare")
    ipv6_off = {
        "changes": [
            {"family": "ipv4", "outcome": "applied", "target": cloudflare_v4},
            {
                "family": "ipv6",
                "outcome": "skipped",
                "detail": "IPv6 is turned off on this adapter",
                "previous": automatic(),
            },
        ]
    }
    assert report_choice_label(ipv6_off, PRESETS, wifi) == "Cloudflare"


def test_dns_done_text_names_skipped_families_as_a_warning() -> None:
    switch = adapter(DEFAULT_SWITCH)
    assert dns_done_text(skipped_ipv4_report(), switch, PRESETS) == (
        "Done: vEthernet (Default Switch) now uses Cloudflare (IPv4 only) DNS; its IPv4 DNS servers were "
        "not changed. Undo it from History.",
        theme.WARNING,
    )
    flushed = skipped_ipv4_report(["The DNS cache could not be flushed; old lookups may be used."])
    text, color = dns_done_text(flushed, switch, PRESETS)
    assert text.endswith("Undo it from History. The DNS cache could not be flushed; old lookups may be used.")
    assert color == theme.WARNING
    # An adapter that is not listed counts every family as turned on.
    assert dns_done_text(skipped_ipv4_report(), {}, PRESETS)[1] == theme.WARNING

    wifi = adapter(WIFI)
    cloudflare_v4 = manual("1.1.1.1", "1.0.0.1", preset="cloudflare")
    applied = {
        "adapter_name": "Wi-Fi",
        "changes": [
            {"family": "ipv4", "outcome": "applied", "target": cloudflare_v4},
            {"family": "ipv6", "outcome": "skipped", "detail": "IPv6 is turned off on this adapter"},
        ],
        "warnings": [],
    }
    assert dns_done_text(applied, wifi, PRESETS) == (
        "Done: Wi-Fi now uses Cloudflare DNS. Undo it from History.",
        theme.GOOD,
    )


@pytest.mark.parametrize(
    ("text", "family", "servers"),
    [
        ("", "ipv4", []),
        ("   ", "ipv6", []),
        ("1.1.1.1", "ipv4", ["1.1.1.1"]),
        ("1.1.1.1, 1.0.0.1", "ipv4", ["1.1.1.1", "1.0.0.1"]),
        ("9.9.9.9 149.112.112.112;8.8.8.8", "ipv4", ["9.9.9.9", "149.112.112.112", "8.8.8.8"]),
        ("1.1.1.1, 1.1.1.1, 1.0.0.1", "ipv4", ["1.1.1.1", "1.0.0.1"]),
        ("127.0.0.1", "ipv4", ["127.0.0.1"]),
        ("::1", "ipv6", ["::1"]),
        ("2606:4700:4700:0:0:0:0:1111", "ipv6", ["2606:4700:4700::1111"]),
        ("2620:FE::FE, 2620:fe::fe", "ipv6", ["2620:fe::fe"]),
        ("1.1.1.1,1.0.0.1,8.8.8.8,8.8.4.4", "ipv4", ["1.1.1.1", "1.0.0.1", "8.8.8.8", "8.8.4.4"]),
        # Duplicates are dropped before the servers are counted.
        ("1.1.1.1,1.0.0.1,8.8.8.8,8.8.4.4,1.1.1.1", "ipv4", ["1.1.1.1", "1.0.0.1", "8.8.8.8", "8.8.4.4"]),
        (" , ", "ipv4", []),
        (" 8.8.8.8 ", "ipv4", ["8.8.8.8"]),
    ],
)
def test_parse_server_list_accepts(text: str, family: str, servers: list[str]) -> None:
    assert parse_server_list(text, family) == (servers, None)


@pytest.mark.parametrize(
    ("text", "family", "reason"),
    [
        ("300.1.1.1", "ipv4", "300.1.1.1 is not a valid IPv4 address"),
        ("1.1.1", "ipv4", "not a valid IPv4 address"),
        ("dns.google", "ipv4", "not a valid IPv4 address"),
        ("2606:4700:4700::1111", "ipv4", "not a valid IPv4 address"),
        ("1.1.1.1", "ipv6", "not a valid IPv6 address"),
        ("fe80::1%12", "ipv6", "not a valid IPv6 address"),
        ("0.0.0.0", "ipv4", "unspecified"),
        ("::", "ipv6", "unspecified"),
        ("224.0.0.1", "ipv4", "multicast"),
        ("224.0.0.251", "ipv4", "multicast"),
        ("ff02::1", "ipv6", "multicast"),
        ("ff02::fb", "ipv6", "multicast"),
        ("255.255.255.255", "ipv4", "broadcast"),
        ("fe80::1", "ipv6", "link-local"),
        ("febf::1", "ipv6", "link-local"),
        ("fec0::1", "ipv6", "site-local"),
        ("fec0:0:0:ffff::1", "ipv6", "site-local"),
        ("feff::1", "ipv6", "site-local"),
        ("1.1.1.1, bad", "ipv4", "bad is not a valid IPv4 address"),
        ("1.1.1.1 1.0.0.1 8.8.8.8 8.8.4.4 9.9.9.9", "ipv4", "at most 4 IPv4 DNS servers"),
    ],
)
def test_parse_server_list_refuses(text: str, family: str, reason: str) -> None:
    servers, error = parse_server_list(text, family)
    assert servers == []
    assert error is not None and reason in error


def test_visible_adapters_hide_minor_ones_unless_show_all() -> None:
    adapters = [copy.deepcopy(a) for a in ADAPTERS]
    listed = [a["id"] for a in visible_adapters(adapters, False)]
    assert listed == [WIFI, ETHERNET, DEFAULT_SWITCH, PROTON_VPN]
    assert [a["id"] for a in visible_adapters(adapters, True)][-1] == WIFI_DIRECT


def test_plan_lines_cover_every_outcome() -> None:
    report = {
        "changes": [
            {
                "family": "ipv4",
                "previous": automatic("192.168.0.1", "198.51.100.53"),
                "target": manual("1.1.1.1", "1.0.0.1", preset="cloudflare"),
                "outcome": "planned",
                "detail": None,
            },
            {
                "family": "ipv6",
                "previous": automatic(),
                "target": manual("2606:4700:4700::1111", preset="cloudflare"),
                "outcome": "skipped",
                "detail": "IPv6 is turned off on this adapter",
            },
        ],
        "warnings": ["The DNS cache could not be flushed; old lookups may be used for a few minutes."],
    }
    assert plan_lines(report) == [
        "• IPv4: automatic (192.168.0.1, 198.51.100.53) → 1.1.1.1, 1.0.0.1",
        "• IPv6: not changed: IPv6 is turned off on this adapter",
        "⚠ The DNS cache could not be flushed; old lookups may be used for a few minutes.",
    ]
    already = {
        "changes": [
            {
                "family": "ipv4",
                "previous": manual("9.9.9.9"),
                "target": manual("9.9.9.9"),
                "outcome": "already_set",
            },
            {
                "family": "ipv6",
                "previous": automatic(),
                "target": automatic(),
                "outcome": "failed",
                "detail": "denied",
            },
        ]
    }
    assert plan_lines(already) == ["• IPv4: already set to 9.9.9.9", "• IPv6: failed: denied"]


def test_nothing_to_change_text() -> None:
    wifi = adapter(WIFI)
    # IPv6 is turned off on the Wi-Fi adapter, so its skip is not a reason worth naming.
    already = {
        "changes": [
            {"family": "ipv4", "outcome": "already_set"},
            {"family": "ipv6", "outcome": "skipped", "detail": "IPv6 is turned off on this adapter"},
        ]
    }
    assert nothing_to_change_text(already, wifi) == (
        "Nothing to change: Wi-Fi already uses these DNS servers.",
        theme.INK_SECONDARY,
    )
    reason = "this adapter has a manually set IPv4 address, so it would get no IPv4 DNS servers automatically"
    skipped = {
        "changes": [
            {"family": "ipv4", "outcome": "skipped", "detail": reason},
            {"family": "ipv6", "outcome": "already_set"},
        ]
    }
    assert nothing_to_change_text(skipped, adapter(DEFAULT_SWITCH)) == (
        f"Nothing to change on vEthernet (Default Switch): {reason}.",
        theme.INK_SECONDARY,
    )
    failed = {"changes": [{"family": "ipv4", "outcome": "failed", "detail": "access denied"}]}
    text, color = nothing_to_change_text(failed, wifi)
    assert text == "Could not read the DNS servers of Wi-Fi: access denied" and color == theme.WARNING
    # Without a name on the adapter, the report's name is used.
    named = {"adapter_name": "Wi-Fi 2", "changes": [{"family": "ipv4", "outcome": "already_set"}]}
    text, _ = nothing_to_change_text(named, {})
    assert text == "Nothing to change: Wi-Fi 2 already uses these DNS servers."


def test_can_change_dns_and_can_renew_need_engine_elevation_and_the_adapter() -> None:
    wifi = adapter(WIFI, can_change_dns=True, can_renew=True)
    assert can_change_dns(wifi, True, True)
    assert not can_change_dns(wifi, True, False)
    assert not can_change_dns(wifi, False, True)
    assert not can_change_dns(adapter(PROTON_VPN), True, True)
    assert can_renew(wifi, True, True)
    assert not can_renew(wifi, True, False)
    assert not can_renew(adapter(ETHERNET, can_renew=False), True, True)


RESET_STEPS = [
    {"id": step_id, "title": title, "command": command, "status": "planned"}
    for step_id, title, command in FAKE_RESET_STEPS
]


def test_reset_details_with_and_without_manual_settings() -> None:
    manual_settings = [{"adapter": "Ethernet", "detail": "static IPv4 192.168.1.20/24, gateway 192.168.1.1"}]
    lines = reset_details({"steps": RESET_STEPS, "manual_settings": manual_settings, "warnings": []})
    assert lines[:4] == [
        "Steps:",
        "• Reset the Winsock catalog  (netsh winsock reset)",
        "• Reset TCP/IP for IPv4  (netsh int ip reset)",
        "• Reset TCP/IP for IPv6  (netsh int ipv6 reset)",
    ]
    assert lines[4].startswith("Manual settings that are lost")
    assert lines[5] == "• Ethernet: static IPv4 192.168.1.20/24, gateway 192.168.1.1"
    unread = "cannot read the settings of unplugged or disabled adapters: Access is denied."
    empty = reset_details({"steps": RESET_STEPS, "manual_settings": [], "warnings": [unread]})
    # Nothing claims that every adapter gets its settings automatically: the engine's warning
    # says what could not be checked.
    assert empty[4:] == ["No manual IP addresses or DNS servers were found.", f"⚠ {unread}"]


def test_reset_result_lines_and_failed_outputs() -> None:
    steps = [
        dict(RESET_STEPS[0], status="succeeded", exit_code=0, output="OK"),
        dict(
            RESET_STEPS[1],
            status="completed_with_errors",
            exit_code=1,
            output="Resetting Neighbor, failed.\nAccess",
        ),
        dict(RESET_STEPS[2], status="timed_out", exit_code=None, output=""),
    ]
    report = {
        "restore_point": {"sequence": 9, "description": "x", "created_at": "t"},
        "steps": steps,
        "manual_settings": [{"adapter": "Wi-Fi", "detail": "IPv4 DNS 1.1.1.1 (set manually)"}],
        "warnings": [],
    }
    assert reset_result_lines(report) == [
        "Restore point #9 was created before the reset.",
        "• Reset the Winsock catalog: done",
        "• Reset TCP/IP for IPv4: finished with errors (exit code 1)",
        "• Reset TCP/IP for IPv6: did not finish within a minute",
        "Re-enter: Wi-Fi: IPv4 DNS 1.1.1.1 (set manually)",
    ]
    assert failed_step_output_lines(report) == [
        "Output of Reset TCP/IP for IPv4:",
        "    Resetting Neighbor, failed.",
        "    Access",
    ]
    without_point = dict(report, restore_point=None, warnings=["System Protection is off"])
    assert reset_result_lines(without_point)[:2] == [
        "No restore point was created.",
        "⚠ System Protection is off",
    ]


def test_restore_point_sentence() -> None:
    assert restore_point_sentence(True, True) == " A restore point is created first."
    assert restore_point_sentence(True, False) == (
        " No restore point can be created because System Protection is off."
    )
    assert restore_point_sentence(True, None) == (
        " Cairn couldn't check System Protection, so a restore point may not be created."
    )
    assert restore_point_sentence(False, True) == " No restore point is created."
    assert restore_point_sentence(False, None) == " No restore point is created."


def test_summary_and_warning_lines() -> None:
    report = FakeEngine().network_list()
    assert summary_text(report) == "5 adapters  ·  2 connected  ·  read in 18 ms"
    assert summary_text(report, show_all=False) == (
        "5 adapters  ·  2 connected  ·  1 idle virtual adapter hidden  ·  read in 18 ms"
    )
    idle = dict(report, adapters=[dict(a, minor=a["status"] != "connected") for a in report["adapters"]])
    assert summary_text(idle, show_all=False) == (
        "5 adapters  ·  2 connected  ·  3 idle virtual adapters hidden  ·  read in 18 ms"
    )
    assert summary_text({"adapters": []}) == "0 adapters  ·  0 connected"
    assert warning_lines(report) == []
    flagged = dict(report, dns_policy=["10.0.0.53"], vpn_connected=True, warnings=["a", "b", "c", "d"])
    assert warning_lines(flagged) == [
        POLICY_WARNING.format(servers="10.0.0.53"),
        VPN_WARNING,
        "⚠ a",
        "⚠ b",
        "⚠ c",
        "⚠ …and 1 more",
    ]
    assert ADMIN_NOTE == "Changing DNS or renewing needs administrator rights."
    assert canonical_id("AAAAAAAA-0000-0000-0000-000000000001") == WIFI
    assert canonical_id("{AAAAAAAA-0000-0000-0000-000000000001}") == WIFI


# -- the fake engine's network model --------------------------------------------------


def test_fake_adapters_carry_every_key_and_the_capability_rules() -> None:
    keys = set(ADAPTERS[0])
    assert all(set(a) == keys for a in ADAPTERS)
    listed = {a["id"]: a for a in FakeEngine().network_list()["adapters"]}
    assert listed[WIFI]["can_change_dns"] and listed[WIFI]["can_renew"] and listed[WIFI]["note"] is None
    assert not listed[ETHERNET]["can_renew"], "a disconnected adapter can't renew"
    assert listed[DEFAULT_SWITCH]["can_change_dns"] and "Virtual adapter" in listed[DEFAULT_SWITCH]["note"]
    assert not listed[PROTON_VPN]["can_change_dns"]

    vpn = {a["id"]: a for a in FakeEngine(vpn_connected=True).network_list()["adapters"]}
    assert all(not a["can_change_dns"] for a in vpn.values())
    assert vpn[WIFI]["note"].startswith("Disconnect the VPN")
    assert vpn[PROTON_VPN]["note"].startswith("VPN and tunnel adapters")

    per_network = {a["id"]: a for a in FakeEngine(profile_dns={WIFI: ["9.9.9.9"]}).network_list()["adapters"]}
    assert per_network[WIFI]["dns_ipv4"]["mode"] == "profile"
    assert not per_network[WIFI]["can_change_dns"]
    assert per_network[ETHERNET]["can_change_dns"]


def test_fake_dns_change_records_first_and_reverts() -> None:
    engine = FakeEngine(elevated=True)
    plan = engine.network_set_dns(WIFI, "cloudflare", dry_run=True)
    assert [c["outcome"] for c in plan["changes"]] == ["planned", "skipped"]
    assert plan["session_id"] is None and engine._dns_active_count() == 0

    engine.network_set_dns(WIFI, "cloudflare")
    engine.network_set_dns(WIFI, "google")
    assert engine._dns_active_count() == 1
    export = engine._dns_export()
    # The first baseline is kept; the target follows the latest write.
    assert export[0]["previous_servers"] == [] and export[0]["target_servers"] == ["8.8.8.8", "8.8.4.4"]
    wifi = next(a for a in engine.network_list()["adapters"] if a["id"] == WIFI)
    assert wifi["dns_ipv4"]["preset"] == "google" and wifi["dns_revertible"]

    actions, restored = engine._dns_revert([WIFI.upper().strip("{}")], dry_run=True)
    assert actions == ["restore IPv4 DNS servers of Wi-Fi to automatic"] and restored == 0
    actions, restored = engine._dns_revert(None, dry_run=False)
    assert restored == 1 and engine._dns_active_count() == 0
    wifi = next(a for a in engine.network_list()["adapters"] if a["id"] == WIFI)
    assert wifi["dns_ipv4"]["mode"] == "automatic" and not wifi["dns_revertible"]

    # A failed later write keeps the target of the last write that succeeded.
    failing = FakeEngine(elevated=True, dns_fail={"ipv4"})
    failing.network_set_dns(ETHERNET, "custom", ["9.9.9.9"])
    failing.dns_fail.clear()
    failing.network_set_dns(ETHERNET, "custom", ["8.8.8.8"])
    failing.dns_fail.add("ipv4")
    failing.network_set_dns(ETHERNET, "custom", ["1.1.1.1"])
    assert [r["target_servers"] for r in failing._dns_export()] == [["8.8.8.8"]]


def test_fake_adapters_without_a_link_list_no_servers_in_use() -> None:
    engine = FakeEngine(elevated=True)
    engine.network_set_dns(ETHERNET, "cloudflare")
    listed = {a["id"]: a for a in engine.network_list()["adapters"]}
    ethernet = listed[ETHERNET]
    assert ethernet["dns_ipv4"]["mode"] == "manual"
    assert ethernet["dns_ipv4"]["servers"] == ["1.1.1.1", "1.0.0.1"]
    assert ethernet["dns_servers"] == [], "the configured servers are not in use without a link"
    assert listed[WIFI]["dns_servers"] == ["192.168.0.1"]


def test_fake_reset_lists_static_ipv6_addresses() -> None:
    engine = FakeEngine(elevated=True)
    ethernet = next(a for a in engine.network_adapters if a["id"] == ETHERNET)
    ethernet["ipv6"].append(
        {"address": "2001:db8:1::50", "prefix_length": 64, "origin": "manual", "preferred": True}
    )
    ethernet["gateways"] = ["2001:db8:1::1"]
    plan = engine.network_reset(restore_point="skip", dry_run=True)
    assert {"adapter": "Ethernet", "detail": "static IPv6 2001:db8:1::50/64, gateway 2001:db8:1::1"} in (
        plan["manual_settings"]
    )
    ethernet["ipv6"].append(
        {"address": "2001:db8:2::9", "prefix_length": 64, "origin": "autoconfigured", "preferred": True}
    )
    plan = engine.network_reset(restore_point="skip", dry_run=True)
    assert {"adapter": "Ethernet", "detail": "static IPv6 2001:db8:1::50/64"} in plan["manual_settings"]


def test_fake_dns_change_refusals() -> None:
    engine = FakeEngine(elevated=False)
    with pytest.raises(RuntimeError, match="elevated"):
        engine.network_set_dns(WIFI, "cloudflare")
    assert engine.network_set_dns(WIFI, "cloudflare", dry_run=True)["changes"][0]["outcome"] == "planned"
    unknown = '^unknown DNS preset "opendns"; valid presets: automatic, cloudflare, '
    with pytest.raises(ValueError, match=unknown):
        engine.network_set_dns(WIFI, "opendns", dry_run=True)
    assert engine.network_set_dns(WIFI, " Cloudflare ", dry_run=True)["changes"][0]["outcome"] == "planned"
    with pytest.raises(ValueError, match="only used with preset"):
        engine.network_set_dns(WIFI, "google", ["8.8.8.8"], dry_run=True)
    with pytest.raises(ValueError, match="at least one"):
        engine.network_set_dns(WIFI, "custom", [], [], dry_run=True)
    for servers, reason in (
        (["1.1.1"], "1.1.1 is not a valid IPv4 address"),
        (["2620:fe::fe"], "2620:fe::fe is not a valid IPv4 address"),
        (["255.255.255.255"], "255.255.255.255 cannot be used as a DNS server"),
        (["1.1.1.1 1.0.0.1", "8.8.8.8,8.8.4.4;9.9.9.9"], "at most 4 IPv4 DNS servers"),
    ):
        with pytest.raises(ValueError, match=f"^{re.escape(reason)}$"):
            engine.network_set_dns(WIFI, "custom", servers, dry_run=True)
    for servers, reason in (
        (["fe80::1%12"], "fe80::1%12 is not a valid IPv6 address"),
        (["fe80::1"], "fe80::1 cannot be used as a DNS server"),
        (["feff::1"], "feff::1 cannot be used as a DNS server"),
    ):
        with pytest.raises(ValueError, match=f"^{re.escape(reason)}$"):
            engine.network_set_dns(ETHERNET, "custom", None, servers, dry_run=True)
    custom = engine.network_set_dns(ETHERNET, "custom", ["9.9.9.9, 9.9.9.9"], ["2620:FE::FE"], dry_run=True)
    assert [c["target"]["servers"] for c in custom["changes"]] == [["9.9.9.9"], ["2620:fe::fe"]]
    assert [c["target"]["preset"] for c in custom["changes"]] == [None, None]
    with pytest.raises(RuntimeError, match="VPN and tunnel adapters"):
        engine.network_set_dns(PROTON_VPN, "cloudflare", dry_run=True)
    switch = engine.network_set_dns(DEFAULT_SWITCH, "automatic", dry_run=True)
    assert switch["changes"][0]["outcome"] == "skipped"
    assert "manually set IPv4 address" in switch["changes"][0]["detail"]


def test_fake_renew_and_reset() -> None:
    engine = FakeEngine(elevated=True, reset_fail={"winsock"}, reset_partial={"ipv6"})
    assert engine.network_renew_dhcp(WIFI)["ipv4"] == ["192.168.0.23"]
    with pytest.raises(RuntimeError, match="not connected"):
        engine.network_renew_dhcp(ETHERNET)
    with pytest.raises(RuntimeError, match="from DHCP"):
        engine.network_renew_dhcp(DEFAULT_SWITCH)
    assert engine.calls_named("network_renew_dhcp") == [
        (WIFI, False),
        (ETHERNET, False),
        (DEFAULT_SWITCH, False),
    ]

    plan = engine.network_reset(restore_point="skip", dry_run=True)
    assert [s["status"] for s in plan["steps"]] == ["planned"] * 3
    assert plan["restore_point"] is None and plan["session_id"] is None
    assert plan["manual_settings"] == [
        {"adapter": "vEthernet (Default Switch)", "detail": "static IPv4 172.28.64.1/20"}
    ]
    report = engine.network_reset()
    assert [s["status"] for s in report["steps"]] == ["failed", "succeeded", "completed_with_errors"]
    assert [s["exit_code"] for s in report["steps"]] == [1, 0, 1]
    assert report["restore_point"]["sequence"] == 9 and report["warnings"] == []
    assert engine.calls_named("network_reset") == [("skip", True), ("try", False)]

    protection_off = FakeEngine(elevated=True, restore_enabled=False).network_reset()
    assert protection_off["restore_point"] is None
    assert protection_off["warnings"] == [f"restore point unavailable: {PROTECTION_OFF}"]
    standard = FakeEngine(elevated=False)
    with pytest.raises(RuntimeError, match="elevated"):
        standard.network_reset()
    assert standard.network_reset(restore_point="skip", dry_run=True)["dry_run"] is True
