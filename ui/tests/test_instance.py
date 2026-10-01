"""The single-instance lock and activation (`optimizer.instance`), with kernel objects of unique
test names in this session's namespace. No window; nothing outside this process changes."""

from __future__ import annotations

import os
import subprocess
import sys
import time
from collections.abc import Iterator
from uuid import uuid4

import pytest

from optimizer import instance
from optimizer.instance import InstanceLock, acquire_when_free, signal_running_instance, wait_for_exit


@pytest.fixture
def prefix() -> str:
    return f"Local\\CairnTest.{uuid4().hex}"


@pytest.fixture
def held(prefix: str) -> Iterator[InstanceLock]:
    lock = InstanceLock.acquire(prefix)
    assert lock is not None
    try:
        yield lock
    finally:
        lock.close()


def test_names_match_the_installer_and_the_launcher() -> None:
    assert instance.PREFIX + instance.MUTEX_SUFFIX == "Local\\Cairn.Instance"
    assert instance.PREFIX + instance.EVENT_SUFFIX == "Local\\Cairn.Activate"


def test_one_lock_per_session(prefix: str, held: InstanceLock) -> None:
    assert held.locked
    assert InstanceLock.acquire(prefix) is None
    assert InstanceLock.acquire(prefix) is None, "a refused attempt leaves the lock alone"


def test_a_second_start_rings_once(prefix: str, held: InstanceLock) -> None:
    assert not held.activation_requested()
    assert signal_running_instance(prefix)
    assert held.activation_requested()
    assert not held.activation_requested(), "the event resets itself"
    assert signal_running_instance(prefix)
    assert signal_running_instance(prefix)
    assert held.activation_requested()
    assert not held.activation_requested(), "two rings before a check count once"


def test_nobody_answers_without_a_window(prefix: str) -> None:
    assert not signal_running_instance(prefix)


def test_close_frees_the_lock(prefix: str) -> None:
    lock = InstanceLock.acquire(prefix)
    assert lock is not None
    lock.close()
    assert not lock.locked
    assert not lock.activation_requested()
    lock.close()
    again = InstanceLock.acquire(prefix)
    assert again is not None
    again.close()


def test_the_lock_is_a_context_manager(prefix: str) -> None:
    lock = InstanceLock.acquire(prefix)
    assert lock is not None
    with lock as entered:
        assert entered is lock
        assert InstanceLock.acquire(prefix) is None
    assert not lock.locked
    again = InstanceLock.acquire(prefix)
    assert again is not None
    again.close()


def test_a_closing_window_stops_answering_but_keeps_the_lock(prefix: str) -> None:
    lock = InstanceLock.acquire(prefix)
    assert lock is not None
    lock.stop_activation()
    assert not signal_running_instance(prefix), "a start during shutdown gets no answer"
    assert InstanceLock.acquire(prefix) is None, "and still cannot take the lock"
    assert lock.locked
    assert not lock.activation_requested()
    lock.close()
    again = InstanceLock.acquire(prefix)
    assert again is not None
    assert signal_running_instance(prefix)
    assert again.activation_requested()
    again.close()


def test_waiting_for_the_lock_of_a_closing_window(prefix: str) -> None:
    lock = InstanceLock.acquire(prefix)
    assert lock is not None
    lock.stop_activation()
    naps: list[float] = []
    now = [0.0]

    def sleep(seconds: float) -> None:
        naps.append(seconds)
        now[0] += seconds
        if len(naps) == 3:
            lock.close()

    got = acquire_when_free(prefix, timeout=30.0, interval=0.25, sleep=sleep, clock=lambda: now[0])
    assert got is not None
    assert naps == [0.25, 0.25, 0.25]
    got.close()


def test_waiting_gives_up_after_the_timeout(prefix: str, held: InstanceLock) -> None:
    naps: list[float] = []
    now = [0.0]

    def sleep(seconds: float) -> None:
        naps.append(seconds)
        now[0] += seconds

    assert acquire_when_free(prefix, timeout=1.0, interval=0.25, sleep=sleep, clock=lambda: now[0]) is None
    assert naps == [0.25] * 4
    assert held.locked


def test_waiting_for_a_process_that_ended() -> None:
    child = subprocess.Popen([sys.executable, "-c", "pass"])  # noqa: S603 - the test interpreter
    child.wait(timeout=10)
    started = time.monotonic()
    assert wait_for_exit(child.pid, 2.0)
    assert time.monotonic() - started < 2.0


def test_waiting_for_a_running_process_times_out() -> None:
    started = time.monotonic()
    assert not wait_for_exit(os.getpid(), 0.05)
    assert time.monotonic() - started < 1.0


def test_acquire_waits_for_the_process_it_replaces(prefix: str) -> None:
    child = subprocess.Popen([sys.executable, "-c", "pass"])  # noqa: S603 - the test interpreter
    child.wait(timeout=10)
    started = time.monotonic()
    lock = InstanceLock.acquire(prefix, wait_for_pid=child.pid, timeout=2.0)
    assert lock is not None and lock.locked
    assert time.monotonic() - started < 2.0
    lock.close()


def test_an_unlocked_lock_answers_nothing() -> None:
    lock = InstanceLock(None, None)
    assert not lock.locked
    assert not lock.activation_requested()
    lock.stop_activation()
    lock.close()
