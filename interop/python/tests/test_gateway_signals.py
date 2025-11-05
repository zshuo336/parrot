"""Signal disposition and CLI ownership; no Ray cluster is started here."""
import signal
import os
import sys
import threading
import types

import pytest

from parrot_protocol import ray_gw, ray_startup


@pytest.mark.parametrize("fails", [False, True])
@pytest.mark.parametrize("disabled", [False, True])
def test_driver_native_signal_option_is_scoped_without_changing_worker_environment(monkeypatch, fails, disabled):
    from ray._private import ray_constants
    monkeypatch.setattr(ray_constants, "RAY_DISABLE_FAILURE_SIGNAL_HANDLER", disabled, raising=False)
    monkeypatch.delenv("RAY_DISABLE_FAILURE_SIGNAL_HANDLER", raising=False)
    environment = dict(os.environ)
    prior = signal.getsignal(signal.SIGTERM)
    def initialize(**kwargs):
        assert ray_constants.RAY_DISABLE_FAILURE_SIGNAL_HANDLER is True
        assert dict(os.environ) == environment
        if fails:
            raise ValueError("startup failed")
        return "ready"
    with ray_startup.graceful_sigterm():
        fake = types.SimpleNamespace(init=initialize)
        if fails:
            with pytest.raises(ValueError, match="startup failed"):
                ray_startup.initialize(fake, {})
        else:
            assert ray_startup.initialize(fake, {}) == "ready"
        assert ray_constants.RAY_DISABLE_FAILURE_SIGNAL_HANDLER is disabled
    assert signal.getsignal(signal.SIGTERM) == prior


def test_legacy_ray_reports_native_limit_and_does_not_invent_unused_option(monkeypatch):
    from ray._private import ray_constants
    monkeypatch.delattr(ray_constants, "RAY_DISABLE_FAILURE_SIGNAL_HANDLER", raising=False)
    def initialize(**kwargs):
        assert not hasattr(ray_constants, "RAY_DISABLE_FAILURE_SIGNAL_HANDLER")
        return "ready"
    with ray_startup.graceful_sigterm():
        with pytest.warns(RuntimeWarning, match="cannot preserve SIGTERM"):
            assert ray_startup.initialize(types.SimpleNamespace(init=initialize), {}) == "ready"
    assert not hasattr(ray_constants, "RAY_DISABLE_FAILURE_SIGNAL_HANDLER")


@pytest.mark.parametrize("fails", [False, True])
def test_initialize_restores_callers_sigterm_even_on_failure(fails):
    prior = signal.getsignal(signal.SIGTERM)
    def caller(_signum, _frame):
        raise KeyboardInterrupt
    def replacement(_signum, _frame):
        raise SystemExit(15)
    def initialize(**kwargs):
        signal.signal(signal.SIGTERM, replacement)
        if fails:
            raise ValueError("Ray init failed")
        return "context"
    signal.signal(signal.SIGTERM, caller)
    try:
        ray = types.SimpleNamespace(init=initialize)
        if fails:
            with pytest.raises(ValueError, match="Ray init failed"):
                ray_startup.initialize(ray, {})
        else:
            assert ray_startup.initialize(ray, {}) == "context"
        assert signal.getsignal(signal.SIGTERM) is caller
    finally:
        signal.signal(signal.SIGTERM, prior)


def test_initialize_preserves_explicit_ignore():
    prior = signal.getsignal(signal.SIGTERM)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    try:
        ray = types.SimpleNamespace(init=lambda **kwargs: signal.signal(signal.SIGTERM, lambda *_: None))
        ray_startup.initialize(ray, {})
        assert signal.getsignal(signal.SIGTERM) == signal.SIG_IGN
    finally:
        signal.signal(signal.SIGTERM, prior)


def test_background_initialization_does_not_install_process_handlers():
    from ray._private import ray_constants
    prior = signal.getsignal(signal.SIGTERM)
    native_option = getattr(ray_constants, "RAY_DISABLE_FAILURE_SIGNAL_HANDLER", None)
    results = []
    def initialize(**kwargs):
        assert getattr(ray_constants, "RAY_DISABLE_FAILURE_SIGNAL_HANDLER", None) == native_option
        return "ready"
    def work():
        try:
            results.append(ray_startup.initialize(types.SimpleNamespace(init=initialize), {}))
        except BaseException as error:
            results.append(error)
    thread = threading.Thread(target=work)
    thread.start()
    thread.join(5)
    assert not thread.is_alive()
    assert results == ["ready"]
    assert signal.getsignal(signal.SIGTERM) == prior


def test_repeated_cli_term_does_not_interrupt_cleanup_and_restores_handler():
    prior = signal.getsignal(signal.SIGTERM)
    with ray_startup.graceful_sigterm():
        handler = signal.getsignal(signal.SIGTERM)
        with pytest.raises(KeyboardInterrupt):
            handler(signal.SIGTERM, None)
        handler(signal.SIGTERM, None)
        handler(signal.SIGTERM, None)
    assert signal.getsignal(signal.SIGTERM) == prior


def test_background_cli_scope_does_not_change_main_thread_handler():
    prior = signal.getsignal(signal.SIGTERM)
    results = []
    def work():
        try:
            with ray_startup.graceful_sigterm():
                results.append(signal.getsignal(signal.SIGTERM))
        except BaseException as error:
            results.append(error)
    thread = threading.Thread(target=work)
    thread.start()
    thread.join(5)
    assert not thread.is_alive()
    assert results == [prior]


def test_unconfigured_embedding_keeps_rays_default_signal_policy():
    from ray._private import ray_constants
    prior = signal.getsignal(signal.SIGTERM)
    native_option = getattr(ray_constants, "RAY_DISABLE_FAILURE_SIGNAL_HANDLER", None)
    replacement = lambda *_: None
    def initialize(**kwargs):
        assert getattr(ray_constants, "RAY_DISABLE_FAILURE_SIGNAL_HANDLER", None) == native_option
        signal.signal(signal.SIGTERM, replacement)
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    try:
        ray = types.SimpleNamespace(init=initialize)
        ray_startup.initialize(ray, {})
        assert signal.getsignal(signal.SIGTERM) is replacement
    finally:
        signal.signal(signal.SIGTERM, prior)


@pytest.mark.parametrize("already_initialized", [False, True])
def test_cli_interrupt_retires_only_runtime_it_owns(monkeypatch, already_initialized):
    shutdowns = []
    fake = types.SimpleNamespace(is_initialized=lambda: already_initialized,
                                 shutdown=lambda: shutdowns.append("closed"))
    monkeypatch.setitem(sys.modules, "ray", fake)
    def stop(*args, **kwargs):
        raise KeyboardInterrupt
    monkeypatch.setattr(ray_gw, "serve", stop)
    prior = signal.getsignal(signal.SIGTERM)
    try:
        ray_gw.main(["ray_gw", "0"])
    except KeyboardInterrupt:
        pytest.fail("CLI leaked its graceful termination instead of returning")
    assert shutdowns == ([] if already_initialized else ["closed"])
    assert signal.getsignal(signal.SIGTERM) == prior


@pytest.mark.parametrize("failure", [ValueError("invalid config"), SystemExit(15)])
def test_cli_startup_failures_are_not_normalized_to_success(monkeypatch, failure):
    shutdowns = []
    monkeypatch.setitem(sys.modules, "ray", types.SimpleNamespace(
        is_initialized=lambda: False, shutdown=lambda: shutdowns.append("closed")))
    def fail(*args, **kwargs):
        raise failure
    monkeypatch.setattr(ray_gw, "serve", fail)
    prior = signal.getsignal(signal.SIGTERM)
    with pytest.raises(type(failure)) as error:
        ray_gw.main(["ray_gw", "0"])
    assert error.value is failure
    assert shutdowns == ["closed"]
    assert signal.getsignal(signal.SIGTERM) == prior
