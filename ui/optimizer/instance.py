"""One Cairn window per Windows session, and activation of the window that is open.

The window's process holds the mutex `Local\\Cairn.Instance` (the installer's AppMutex, so
setup asks to close Cairn) and waits on the auto-reset event `Local\\Cairn.Activate`: a
second start sets the event and exits, and the window comes to the front. Both objects give
the interactive users of this session only the rights to wait on and set them, and carry a
Medium integrity label with no-write-up, so an unelevated start can activate an elevated
window while low-integrity processes cannot.

Used by `__main__` only; the window reaches its lock through `App.instance`.
"""

from __future__ import annotations

import ctypes
import logging
import time
from collections.abc import Callable
from ctypes import wintypes
from types import SimpleNamespace

log = logging.getLogger(__name__)

PREFIX = "Local\\Cairn"
MUTEX_SUFFIX = ".Instance"
EVENT_SUFFIX = ".Activate"
OBJECT_SDDL = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x00100003;;;IU)S:(ML;;NW;;;ME)"

_SDDL_REVISION_1 = 1
_ERROR_ACCESS_DENIED = 5
_ERROR_ALREADY_EXISTS = 183
_WAIT_OBJECT_0 = 0
_SYNCHRONIZE = 0x00100000
_EVENT_MODIFY_STATE = 0x0002
_ASFW_ANY = 0xFFFFFFFF


class _SecurityAttributes(ctypes.Structure):
    _fields_ = [
        ("nLength", wintypes.DWORD),
        ("lpSecurityDescriptor", wintypes.LPVOID),
        ("bInheritHandle", wintypes.BOOL),
    ]


_API: SimpleNamespace | None = None


def _api() -> SimpleNamespace:
    """The kernel32, advapi32 and user32 functions used here, with their signatures."""
    global _API
    if _API is not None:
        return _API
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)
    user32 = ctypes.WinDLL("user32", use_last_error=True)
    lpsa = ctypes.POINTER(_SecurityAttributes)
    signatures: dict[str, tuple[object, list[object], object]] = {
        "CreateMutexW": (kernel32, [lpsa, wintypes.BOOL, wintypes.LPCWSTR], wintypes.HANDLE),
        "CreateEventW": (kernel32, [lpsa, wintypes.BOOL, wintypes.BOOL, wintypes.LPCWSTR], wintypes.HANDLE),
        "OpenEventW": (kernel32, [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR], wintypes.HANDLE),
        "OpenProcess": (kernel32, [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD], wintypes.HANDLE),
        "SetEvent": (kernel32, [wintypes.HANDLE], wintypes.BOOL),
        "WaitForSingleObject": (kernel32, [wintypes.HANDLE, wintypes.DWORD], wintypes.DWORD),
        "CloseHandle": (kernel32, [wintypes.HANDLE], wintypes.BOOL),
        "LocalFree": (kernel32, [wintypes.HLOCAL], wintypes.HLOCAL),
        "ConvertStringSecurityDescriptorToSecurityDescriptorW": (
            advapi32,
            [
                wintypes.LPCWSTR,
                wintypes.DWORD,
                ctypes.POINTER(wintypes.LPVOID),
                ctypes.POINTER(wintypes.ULONG),
            ],
            wintypes.BOOL,
        ),
        "AllowSetForegroundWindow": (user32, [wintypes.DWORD], wintypes.BOOL),
    }
    functions = {}
    for name, (dll, argtypes, restype) in signatures.items():
        fn = getattr(dll, name)
        fn.argtypes = argtypes
        fn.restype = restype
        functions[name] = fn
    _API = SimpleNamespace(**functions)
    return _API


def _security_descriptor(api: SimpleNamespace) -> int:
    """`OBJECT_SDDL` as a self-relative security descriptor; the caller frees it with LocalFree."""
    descriptor = wintypes.LPVOID()
    if not api.ConvertStringSecurityDescriptorToSecurityDescriptorW(
        OBJECT_SDDL, _SDDL_REVISION_1, ctypes.byref(descriptor), None
    ):
        raise ctypes.WinError(ctypes.get_last_error())
    return int(descriptor.value or 0)


def wait_for_exit(pid: int, timeout: float) -> bool:
    """Waits up to `timeout` seconds for process `pid` to end. True when it ended or no longer
    exists; False on timeout or when it cannot be waited on."""
    api = _api()
    handle = api.OpenProcess(_SYNCHRONIZE, False, int(pid))
    if not handle:
        return ctypes.get_last_error() != _ERROR_ACCESS_DENIED
    try:
        return api.WaitForSingleObject(handle, max(0, int(timeout * 1000))) == _WAIT_OBJECT_0
    finally:
        api.CloseHandle(handle)


class InstanceLock:
    """The window's hold on the single-instance mutex and its activation event.

    A lock whose objects could not be created for a reason other than another instance is
    still returned, unlocked (`locked` is False), so Cairn starts anyway.
    """

    def __init__(self, mutex: int | None, event: int | None) -> None:
        self._mutex = mutex
        self._event = event

    @classmethod
    def acquire(
        cls, prefix: str = PREFIX, *, wait_for_pid: int | None = None, timeout: float = 20.0
    ) -> InstanceLock | None:
        """Takes the lock, or returns None when another Cairn of this session holds it.

        With `wait_for_pid`, first waits up to `timeout` seconds for that process to end (a
        relaunch that replaces it)."""
        if wait_for_pid is not None:
            wait_for_exit(wait_for_pid, timeout)
        api = _api()
        try:
            descriptor = _security_descriptor(api)
        except OSError:
            log.warning("the single-instance lock could not be created", exc_info=True)
            return cls(None, None)
        try:
            attributes = _SecurityAttributes(ctypes.sizeof(_SecurityAttributes), descriptor, False)
            mutex = api.CreateMutexW(ctypes.byref(attributes), False, prefix + MUTEX_SUFFIX)
            error = ctypes.get_last_error()
            if not mutex:
                if error == _ERROR_ACCESS_DENIED:
                    # Held by an elevated Cairn, whose mutex this process may only wait on.
                    return None
                log.warning("the single-instance lock could not be created (error %d)", error)
                return cls(None, None)
            if error == _ERROR_ALREADY_EXISTS:
                api.CloseHandle(mutex)
                return None
            event = api.CreateEventW(ctypes.byref(attributes), False, False, prefix + EVENT_SUFFIX)
            if not event:
                log.warning("the activation event could not be created (error %d)", ctypes.get_last_error())
            return cls(mutex, event or None)
        finally:
            api.LocalFree(descriptor)

    @property
    def locked(self) -> bool:
        """Whether this lock holds the mutex."""
        return self._mutex is not None

    def activation_requested(self) -> bool:
        """Whether another start asked this window to come to the front since the last call."""
        if self._event is None:
            return False
        return _api().WaitForSingleObject(self._event, 0) == _WAIT_OBJECT_0

    def stop_activation(self) -> None:
        """Stops accepting activations and keeps the mutex: a start while this window shuts
        down finds no event and waits for the mutex instead."""
        event, self._event = self._event, None
        if event is not None:
            _api().CloseHandle(event)

    def close(self) -> None:
        """Releases the event and the mutex."""
        self.stop_activation()
        mutex, self._mutex = self._mutex, None
        if mutex is not None:
            _api().CloseHandle(mutex)

    def __enter__(self) -> InstanceLock:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()


def signal_running_instance(prefix: str = PREFIX) -> bool:
    """Asks the open window to come to the front and lets it take the foreground; False when
    no window accepts activation."""
    api = _api()
    event = api.OpenEventW(_EVENT_MODIFY_STATE, False, prefix + EVENT_SUFFIX)
    if not event:
        return False
    try:
        api.AllowSetForegroundWindow(_ASFW_ANY)
        return bool(api.SetEvent(event))
    finally:
        api.CloseHandle(event)


def acquire_when_free(
    prefix: str = PREFIX,
    *,
    timeout: float = 30.0,
    interval: float = 0.25,
    sleep: Callable[[float], None] = time.sleep,
    clock: Callable[[], float] = time.monotonic,
) -> InstanceLock | None:
    """Tries to take the lock every `interval` seconds for up to `timeout` seconds; for a start
    that found a window which no longer accepts activation because it is closing. None when
    the lock stayed taken."""
    deadline = clock() + timeout
    while True:
        lock = InstanceLock.acquire(prefix)
        if lock is not None:
            return lock
        if clock() >= deadline:
            return None
        sleep(interval)
