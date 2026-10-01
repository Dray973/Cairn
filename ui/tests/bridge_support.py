"""Helpers for EngineBridge tests: waiting for queued calls and comparing the FakeEngine's
function signatures with the real engine module's."""

from __future__ import annotations

import inspect
import time
from collections.abc import Callable, Iterable
from concurrent.futures import Future
from typing import Any

from optimizer.bridge.engine import EngineBridge


def wait(bridge: EngineBridge, futures: list[Future[Any]], timeout: float = 5.0) -> None:
    """Waits until every future is done, then runs the finished calls' callbacks."""
    deadline = time.monotonic() + timeout
    while not all(f.done() for f in futures):
        assert time.monotonic() < deadline, "engine call did not finish"
        time.sleep(0.005)
    bridge.dispatch_completed()


def _parameters(fn: Callable[..., Any]) -> list[tuple[str, Any]]:
    return [(p.name, p.default) for p in inspect.signature(fn).parameters.values()]


def assert_signatures_match(fake: object, module: object, names: Iterable[str]) -> None:
    """The fake's functions take the same parameters, in order and with the same defaults, as
    the real module's. Names the real module lacks (an older build) are skipped."""
    for name in names:
        real = getattr(module, name, None)
        if real is None:
            continue
        mirror = getattr(fake, name, None)
        assert mirror is not None, f"FakeEngine has no {name}"
        assert _parameters(mirror) == _parameters(real), (
            f"{name}: FakeEngine takes {inspect.signature(mirror)}, the engine {inspect.signature(real)}"
        )
