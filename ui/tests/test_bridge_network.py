"""EngineBridge network calls: keyword policies, the synchronous preset table, capability
checks, and parity between the FakeEngine and the real engine module (signatures and the
DNS preset table).

Of the real module only read-only functions are called: the preset table, the adapter list,
a journal summary of a private journal, and a DNS change planned as a dry run whose target
is the adapter's current setting, so that even a run that ignored the flag would write
nothing.
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Any

import pytest

from optimizer.bridge.engine import EngineBridge, EngineUnavailable, load_engine_module

from .bridge_support import assert_signatures_match, wait
from .fake_engine import FakeEngine
from .fake_network import WIFI

NETWORK_NAMES = (
    "network_list",
    "network_dns_presets",
    "network_set_dns",
    "network_flush_dns",
    "network_renew_dhcp",
    "network_reset",
)


def test_set_dns_forwards_skip_and_not_dry_run() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        wait(bridge, [bridge.set_dns(WIFI, "cloudflare")])
        assert engine.calls_named("network_set_dns")[-1] == (WIFI, "cloudflare", None, None, "skip", False)
        wait(bridge, [bridge.plan_set_dns(WIFI, "cloudflare")])
        assert engine.calls_named("network_set_dns")[-1] == (WIFI, "cloudflare", None, None, "skip", True)

        ipv4 = ["9.9.9.9"]
        ipv6 = ("2620:fe::fe",)
        future = bridge.plan_set_dns(WIFI, "custom", ipv4, ipv6)
        ipv4.append("149.112.112.112")  # the bridge copied the list when the call was queued
        wait(bridge, [future])
        assert future.exception() is None
        call = engine.calls_named("network_set_dns")[-1]
        assert call == (WIFI, "custom", ["9.9.9.9"], ["2620:fe::fe"], "skip", True)
        assert call[2] is not ipv4
    finally:
        bridge.shutdown()


def test_flush_and_renew_are_queued_with_their_defaults() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        flushed = bridge.flush_dns()
        renewed = bridge.renew_lease(WIFI)
        wait(bridge, [flushed, renewed])
        assert flushed.result() == {"session_id": 1}
        assert engine.calls_named("network_flush_dns") == [()]
        assert engine.calls_named("network_renew_dhcp") == [(WIFI, False)]
        assert renewed.result()["ipv4"] == ["192.168.0.23"]
    finally:
        bridge.shutdown()


def test_network_reset_uses_try_only_with_restore_points() -> None:
    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        plan = bridge.plan_network_reset()
        wait(bridge, [plan])
        assert engine.calls_named("network_reset")[-1] == ("skip", True)
        assert plan.result()["restore_point"] is None
        reset = bridge.network_reset()
        wait(bridge, [reset])
        assert engine.calls_named("network_reset")[-1] == ("try", False)
        assert reset.result()["restore_point"]["sequence"] == 9
    finally:
        bridge.shutdown()

    engine = FakeEngine(elevated=True)
    bridge = EngineBridge(module=engine, restore_points=False)  # type: ignore[arg-type]
    try:
        wait(bridge, [bridge.network_reset()])
        assert engine.calls_named("network_reset")[-1] == ("skip", False)
    finally:
        bridge.shutdown()


def test_dns_presets_is_synchronous_cached_and_does_not_sleep() -> None:
    engine = FakeEngine(delay=0.5)
    bridge = EngineBridge(module=engine)  # type: ignore[arg-type]
    try:
        started = time.perf_counter()
        presets = bridge.dns_presets()
        assert time.perf_counter() - started < 0.1, "the preset table was read with the call delay"
        assert not bridge.busy, "the preset table is not queued on the worker"
        assert [p["id"] for p in presets][:2] == ["automatic", "cloudflare"]
        assert all(set(p) == {"id", "title", "description", "ipv4", "ipv6"} for p in presets)
        assert bridge.dns_presets() is presets
        assert engine.calls_named("network_dns_presets") == [()]
    finally:
        bridge.shutdown()


def test_supports_reports_missing_functions() -> None:
    bridge = EngineBridge(module=FakeEngine(unsupported=["network_list"]))  # type: ignore[arg-type]
    try:
        assert not bridge.supports("network_list")
        assert bridge.supports("network_set_dns")
        assert not bridge.supports("no_such_function")
    finally:
        bridge.shutdown()


def test_fake_network_signatures_match_real_module() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "network_list", None)):
        pytest.skip("the deployed engine module has no network functions yet")
    assert_signatures_match(FakeEngine(elevated=True), module, NETWORK_NAMES)


def test_fake_dns_presets_match_real_module() -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "network_dns_presets", None)):
        pytest.skip("the deployed engine module has no network functions yet")
    # The preset table is pure data; reading it touches nothing on this PC.
    assert FakeEngine().network_dns_presets() == list(module.network_dns_presets())


def _unchanged_request(adapter: dict[str, Any]) -> tuple[str, list[str] | None, list[str] | None]:
    """A DNS request that leaves `adapter` as it is: its manual servers, else automatic."""
    manual = {
        family: [str(s) for s in adapter[f"dns_{family}"]["servers"]]
        for family in ("ipv4", "ipv6")
        if adapter[f"dns_{family}"]["mode"] == "manual"
    }
    if not manual:
        return "automatic", None, None
    return "custom", manual.get("ipv4"), manual.get("ipv6")


def test_real_dns_plan_opens_no_journal_session(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    try:
        module = load_engine_module()
    except EngineUnavailable as exc:
        pytest.skip(str(exc))
    if not callable(getattr(module, "network_set_dns", None)):
        pytest.skip("the deployed engine module has no network functions yet")
    # A private journal: nothing is added to the user's, and nothing the user does meanwhile
    # is counted.
    monkeypatch.setenv("OPTIMIZER_DATA_DIR", str(tmp_path))
    adapter = next((a for a in module.network_list()["adapters"] if a["can_change_dns"]), None)
    if adapter is None:
        pytest.skip("no adapter whose DNS servers can be changed")
    preset, ipv4, ipv6 = _unchanged_request(adapter)
    before = module.journal_summary()["sessions"]

    try:
        report = module.network_set_dns(adapter["id"], preset, ipv4, ipv6, restore_point="skip", dry_run=True)
    except ValueError as exc:
        pytest.skip(f"the adapter's current servers cannot be requested again: {exc}")
    assert report["dry_run"] is True and report["session_id"] is None
    assert {c["outcome"] for c in report["changes"]} <= {"already_set", "skipped"}
    assert module.journal_summary()["sessions"] == before == 0, "a dry run opened a journal session"
