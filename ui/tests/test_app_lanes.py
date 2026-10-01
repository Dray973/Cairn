"""Job lanes of the window: the order of the close dialogs and of shutdown, confirmed lanes, the
single-instance activation, a failing poll, the running-job title that network maintenance waits
for and the status bar texts.

The tools lane runs FakeEngine tool jobs; a scripted lane "extra" is added to one window through
`_job_lanes`. Administrator dialogs are never confirmed.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from optimizer.app import BUSY_CLOSE_TITLE, JOB_LANES, JobLane

from .app_support import (
    App,
    AppFactory,
    MessageDialog,
    dialog_text,
    dialogs,
    idle,
    next_dialog,
    pump,
    scanned,
    show_section,
    theme,
)

EXTRA = JobLane(
    "extra",
    "_poll_extra",
    "_extra_poll_failed",
    "_extra_allow_close",
    "_extra_before_shutdown",
    "_extra_running_title",
    False,
)
TOOL_DIALOG = "Check system files is still running"
EXTRA_DIALOG = "Extra job is still running"


class ExtraLane:
    """The methods of lane "extra", set on one window. While `title` is set a job runs, and
    closing asks about it in a dialog whose confirmation continues through `_continue_close`.
    `order` receives "extra" for each close check and "extra:shutdown" before shutdown."""

    def __init__(self, app: App, order: list[str], *, title: str | None = None) -> None:
        self.app = app
        self.order = order
        self.title = title
        self.polls = 0
        self.poll_failures = 0
        self.poll_error: Exception | None = None
        app._job_lanes = (*JOB_LANES, EXTRA)
        methods: dict[str, Callable[..., Any]] = {
            EXTRA.poll: self.poll,
            EXTRA.poll_failed: self.poll_failed,
            EXTRA.allow_close: self.allow_close,
            EXTRA.before_shutdown: self.before_shutdown,
            EXTRA.running_title: self.running_title,
        }
        for name, method in methods.items():
            setattr(app, name, method)

    def poll(self, now: float) -> None:
        self.polls += 1
        if self.poll_error is not None:
            raise self.poll_error

    def poll_failed(self) -> None:
        self.poll_failures += 1

    def allow_close(self) -> bool:
        self.order.append("extra")
        if self.title is None:
            return True
        MessageDialog(
            self.app,
            title=f"{self.title} is still running",
            message="Closing stops it.",
            confirm_text="Stop and close",
            cancel_text="Keep running",
            danger=True,
            on_confirm=lambda: self.app._continue_close("extra", busy_unchanged=True),
        )
        return False

    def before_shutdown(self) -> None:
        self.order.append("extra:shutdown")

    def running_title(self) -> str | None:
        return self.title


class InstanceRecorder:
    """Stands in for the single-instance lock `__main__` hands the window."""

    def __init__(self, *, fail: bool = False) -> None:
        self.stopped = 0
        self.fail = fail

    def stop_activation(self) -> None:
        self.stopped += 1
        if self.fail:
            raise OSError("the activation event is already closed")


def record(owner: Any, name: str, order: list[str], label: str) -> None:
    """Replaces `owner`'s method `name` with one that appends `label` to `order` first."""
    original = getattr(owner, name)

    def recorded(*args: Any, **kwargs: Any) -> Any:
        order.append(label)
        return original(*args, **kwargs)

    setattr(owner, name, recorded)


def record_shutdown(app: App, order: list[str]) -> None:
    """Records each lane's `before_shutdown` and the engine's shutdown in `order`."""
    for lane in JOB_LANES:
        record(app, lane.before_shutdown, order, f"{lane.name}:shutdown")
    record(app.engine, "shutdown", order, "engine")


def run_sfc_verify(app: App) -> None:
    """Shows Tools once the first scan is done and starts "Check system files" (no dialog asks
    for administrator rights, since the window is elevated)."""
    pump(app, 5.0, until=lambda: scanned(app))
    show_section(app, "Tools")
    pump(app, 5.0, until=lambda: app.tools_panel.loaded and idle(app))
    app.tools_panel.rows["sfc_verify"].run_button.invoke()
    next_dialog(app).confirm_button.invoke()
    pump(app, 5.0, until=lambda: app._tool_job is not None and idle(app))


def test_close_asks_busy_then_tools_then_each_lane_and_shuts_down_in_lane_order(
    make_app: AppFactory,
) -> None:
    app, engine = make_app(elevated=True, delay=0.2)
    run_sfc_verify(app)
    order: list[str] = []
    ExtraLane(app, order, title="Extra job")
    record(app, "_tools_allow_close", order, "tools")
    record_shutdown(app, order)
    instance = InstanceRecorder()
    app.instance = instance
    app.start_scan()
    assert app._busy

    app._request_close()
    dialog = next_dialog(app)
    assert dialog.title_text == BUSY_CLOSE_TITLE
    assert order == [], "the busy dialog comes before any lane"
    dialog.confirm_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == TOOL_DIALOG
    assert order == ["tools"]
    dialog.confirm_button.invoke()
    dialog = next_dialog(app)
    assert dialog.title_text == EXTRA_DIALOG, "the lanes are asked in lane order"
    assert order == ["tools", "extra"]
    assert app._running and instance.stopped == 0

    dialog.confirm_button.invoke()
    assert not app._running
    assert instance.stopped == 1
    assert order == [
        "tools",
        "extra",
        "tools:shutdown",
        "updates:shutdown",
        "storage:shutdown",
        "maintenance:shutdown",
        "security:shutdown",
        "extra:shutdown",
        "engine",
    ]
    assert engine.calls_named("tools_shutdown") == [()]
    assert app.errors == []


def test_a_confirmed_lane_is_not_asked_again_after_the_busy_dialog(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    run_sfc_verify(app)
    asked: list[str] = []
    record(app, "_tools_allow_close", asked, "tools")
    instance = InstanceRecorder()
    app.instance = instance

    app._request_close()
    tools_dialog = next_dialog(app)
    assert tools_dialog.title_text == TOOL_DIALOG
    # A recorded change starts while the tools dialog is open.
    app._set_busy(True, "journaled", action="Privacy Mode")
    tools_dialog.confirm_button.invoke()
    busy_dialog = next_dialog(app)
    assert busy_dialog.title_text == BUSY_CLOSE_TITLE
    assert "Privacy Mode is still running." in dialog_text(busy_dialog)
    assert app._running and instance.stopped == 0

    busy_dialog.confirm_button.invoke()
    assert asked == ["tools"], "the tools dialog was confirmed once and is not shown again"
    assert not app._running
    assert instance.stopped == 1
    assert engine.calls_named("tools_shutdown") == [()]
    assert app.errors == []


def test_a_cancelled_lane_dialog_keeps_the_window_and_the_next_close_asks_again(
    make_app: AppFactory,
) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    order: list[str] = []
    ExtraLane(app, order, title="Extra job")
    instance = InstanceRecorder()
    app.instance = instance

    app._request_close()
    dialog = next_dialog(app)
    assert dialog.title_text == EXTRA_DIALOG
    dialog._cancel()
    pump(app, 0.2)
    assert app._running
    assert instance.stopped == 0, "a cancelled close leaves the window reachable for a second start"
    assert app._close_confirmed == set()

    app._request_close()
    dialog = next_dialog(app)
    assert dialog.title_text == EXTRA_DIALOG, "a new attempt asks again"
    dialog.confirm_button.invoke()
    assert not app._running
    assert instance.stopped == 1
    assert order == ["extra", "extra", "extra:shutdown"]
    assert app.errors == []


def test_force_skips_every_dialog_but_not_the_shutdown_steps(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    run_sfc_verify(app)
    order: list[str] = []
    ExtraLane(app, order, title="Extra job")
    record(app, "_tools_allow_close", order, "tools")
    # A lock whose activation event cannot be stopped does not keep the window open.
    instance = InstanceRecorder(fail=True)
    app.instance = instance
    app._request_close(force=True)
    assert not app._running
    assert instance.stopped == 1
    assert order == ["extra:shutdown"], "no lane is asked; each still prepares for shutdown"
    assert engine.calls_named("tools_shutdown") == [()]
    assert app.errors == []


def test_a_failing_before_shutdown_does_not_stop_the_others(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    order: list[str] = []
    ExtraLane(app, order)
    record_shutdown(app, order)

    def broken() -> None:
        order.append("storage:broken")
        raise RuntimeError("storage shutdown broke")

    app._storage_before_shutdown = broken  # type: ignore[method-assign]
    app._request_close()
    assert not app._running
    assert order == [
        "extra",
        "tools:shutdown",
        "updates:shutdown",
        "storage:broken",
        "maintenance:shutdown",
        "security:shutdown",
        "extra:shutdown",
        "engine",
    ]
    assert app.errors == []


def test_a_failing_poll_latches_only_its_lane(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    order: list[str] = []
    extra = ExtraLane(app, order)
    tool_polls: list[str] = []
    record(app, "_poll_tools", tool_polls, "tools")
    pump(app, 5.0, until=lambda: extra.polls > 0)

    extra.poll_error = RuntimeError("extra lane broke")
    pump(app, 5.0, until=lambda: "extra" in app._lane_failed)
    polls = extra.polls
    before = len(tool_polls)
    pump(app, 0.3)
    assert extra.polls == polls, "a failed lane is not polled again"
    assert extra.poll_failures == 1
    assert len(tool_polls) > before, "the other lanes keep being polled"
    assert app._lane_failed == {"extra"}
    assert not app._tool_poll_failed
    assert len(app.errors) == 1 and "extra lane broke" in app.errors[0]
    app.errors.clear()

    # The tools flag of the tools feature is the tools lane's latch.
    app._tool_poll_failed = True
    assert app._lane_failed == {"extra", "tools"}
    before = len(tool_polls)
    pump(app, 0.2)
    assert len(tool_polls) == before
    app._tool_poll_failed = False
    assert app._lane_failed == {"extra"}
    assert app.errors == []


def test_running_job_title_follows_lane_order_and_network_only(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    order: list[str] = []
    extra = ExtraLane(app, order)
    assert app._running_job_title() is None
    assert app._running_job_title(network_only=True) is None

    extra.title = "Extra job"
    assert app._running_job_title() == "Extra job"
    assert app._running_job_title(network_only=True) is None, "lane extra does not block the network"

    titles: dict[str, str | None] = {"updates": None, "storage": "Speed test on C:"}
    app._updates_running_title = lambda: titles["updates"]  # type: ignore[method-assign]
    app._running_storage_title = lambda: titles["storage"]  # type: ignore[method-assign]
    assert app._running_job_title() == "Speed test on C:"
    assert app._running_job_title(network_only=True) is None
    titles["updates"] = "Updating 2 apps"
    assert app._running_job_title() == "Updating 2 apps"
    assert app._running_job_title(network_only=True) == "Updating 2 apps"
    app._running_tool_title = lambda: "Check system files"  # type: ignore[method-assign]
    assert app._running_job_title() == "Check system files", "tools is the first lane"

    def broken() -> str | None:
        raise RuntimeError("tools title broke")

    app._running_tool_title = broken  # type: ignore[method-assign]
    assert app._running_job_title() == "Updating 2 apps", "a broken lane is reported and skipped"
    assert len(app.errors) == 1 and "tools title broke" in app.errors[0]
    app.errors.clear()
    del app._running_tool_title
    assert app._running_job_title() == "Updating 2 apps"
    assert app.errors == []


def test_network_maintenance_waits_only_for_lanes_that_block_it(make_app: AppFactory) -> None:
    app, engine = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))
    adapter = {"id": "{aaaaaaaa-0000-0000-0000-000000000001}", "name": "Wi-Fi"}

    app._running_storage_title = lambda: "Speed test on C:"  # type: ignore[method-assign]
    app._on_renew(adapter)
    dialog = next_dialog(app)
    assert dialog.title_text == "Renew the IP lease of Wi-Fi?", "a speed test does not block the network"
    dialog._cancel()
    pump(app, 0.2)

    app._updates_running_title = lambda: "Updating 2 apps"  # type: ignore[method-assign]
    app._on_renew(adapter)
    assert (app.status_message.cget("text"), app.status_message.cget("text_color")) == (
        "Wait for Updating 2 apps to finish before renewing the lease.",
        theme.WARNING,
    )
    app._on_network_reset()
    assert app.status_message.cget("text") == (
        "Wait for Updating 2 apps to finish before resetting the network stack."
    )
    pump(app, 0.2)
    assert dialogs(app) == []
    assert engine.calls_named("network_renew_dhcp") == []
    assert engine.calls_named("network_reset") == []
    assert app.errors == []


def test_job_status_texts_are_joined_in_lane_order(make_app: AppFactory) -> None:
    app, _ = make_app(elevated=True)
    pump(app, 5.0, until=lambda: scanned(app))

    def shown() -> str:
        return str(app.status_tool.cget("text"))

    assert shown() == ""
    app.set_job_status("security", "◐ Checking Windows Update…")
    app.set_job_status("updates", "◐ Updating 2 apps")
    assert shown() == "◐ Updating 2 apps  ·  ◐ Checking Windows Update…"
    app.set_job_status("tools", "◐ Check system files  ·  40%")
    assert shown() == "◐ Check system files  ·  40%  ·  ◐ Updating 2 apps  ·  ◐ Checking Windows Update…"
    app.set_job_status("updates", "")
    assert shown() == "◐ Check system files  ·  40%  ·  ◐ Checking Windows Update…"

    # A lane the window does not know yet is shown last; a known one keeps its place.
    app.set_job_status("extra", "◐ Extra job")
    assert shown().endswith("◐ Checking Windows Update…  ·  ◐ Extra job")
    order: list[str] = []
    ExtraLane(app, order)
    app.set_job_status("storage", "◐ Speed test on C:")
    assert shown() == (
        "◐ Check system files  ·  40%  ·  ◐ Speed test on C:  ·  ◐ Checking Windows Update…  ·  ◐ Extra job"
    )
    for lane in ("tools", "storage", "security", "extra"):
        app.set_job_status(lane, "")
    assert shown() == ""
    assert app.errors == []
