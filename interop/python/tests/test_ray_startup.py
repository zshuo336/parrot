"""Port-selection safety, independent of a running Ray cluster."""
import socket
import errno
import io
import subprocess
from unittest.mock import patch
import pytest
from parrot_protocol.ray_startup import ephemeral_port_range, guarded_allocator, guarded_ports


def test_collision_fallback_never_returns_excluded_port_and_closes_probes():
    sockets = []
    real = socket.socket
    def create(*args):
        probe = real(*args)
        sockets.append(probe)
        return probe
    with patch('parrot_protocol.ray_startup.socket.socket', side_effect=create):
        selected = guarded_allocator(lambda _, excluded: 65534)(None, {65534, 65535})
    assert selected not in {65534, 65535}
    assert sockets and all(s.fileno() == -1 for s in sockets)
    with real(socket.AF_INET, socket.SOCK_STREAM) as check:
        check.bind(('', selected))


def test_existing_valid_selection_and_errors_are_not_silently_retried():
    with patch('parrot_protocol.ray_startup.socket.socket', side_effect=AssertionError('unnecessary probe')):
        assert guarded_allocator(lambda _, excluded: 12345)(None, {42}) == 12345
    def broken(_, excluded):
        raise ValueError('configuration error')
    with pytest.raises(ValueError, match='configuration error'):
        guarded_allocator(broken)(None, set())


def test_exhaustion_is_bounded_and_every_probe_is_closed():
    class Probe:
        def __init__(self): self.closed = False
        def bind(self, address): raise OSError(errno.EADDRINUSE, 'occupied')
        def close(self): self.closed = True
    probes = []
    def create(*args):
        probe = Probe(); probes.append(probe); return probe
    with patch('parrot_protocol.ray_startup.socket.socket', side_effect=create):
        with pytest.raises(RuntimeError, match='distinct non-ephemeral internal port'):
            guarded_allocator(lambda _, excluded: 65534)(None, {65534})
    assert len(probes) == 128 and all(p.closed for p in probes)


def test_initialization_scope_restores_original_even_when_startup_fails():
    class Node:
        def _get_unused_port(self, allocated_ports=None): return 12345
        def start_raylet(self): pass
    original = Node._get_unused_port
    with pytest.raises(ValueError, match='init failed'):
        with guarded_ports(Node):
            assert Node._get_unused_port is not original
            assert Node()._get_unused_port({42}) == 12345
            raise ValueError('init failed')
    assert Node._get_unused_port is original


def test_auto_raylet_ports_are_distinct_from_pending_agents_before_launch():
    from ray._private.parameter import RayParams
    class Node:
        def __init__(self):
            self._ray_params = RayParams(runtime_env_agent_port=65516)
        def _get_unused_port(self, allocated_ports=None): return 65516
        def start_raylet(self, marker):
            params = self._ray_params
            assert params.object_manager_port not in (None, 0, 65516)
            assert params.node_manager_port not in (None, 0, 65516, params.object_manager_port)
            params.update_pre_selected_port()
            return marker
    node = Node()
    with guarded_ports(Node):
        assert node.start_raylet('launched') == 'launched'


def test_explicit_raylet_ports_and_start_failure_restore_both_methods():
    from ray._private.parameter import RayParams
    class Node:
        def __init__(self):
            self._ray_params = RayParams(object_manager_port=32101, node_manager_port=32102, runtime_env_agent_port=32103)
        def _get_unused_port(self, allocated_ports=None): raise AssertionError('explicit port changed')
        def start_raylet(self, *, marker):
            assert self._ray_params.object_manager_port == 32101
            assert self._ray_params.node_manager_port == 32102
            raise ValueError(marker)
    allocator, start = Node._get_unused_port, Node.start_raylet
    with pytest.raises(ValueError, match='startup failure'):
        with guarded_ports(Node):
            Node().start_raylet(marker='startup failure')
    assert Node._get_unused_port is allocator
    assert Node.start_raylet is start


def test_valid_but_pending_ephemeral_port_is_not_returned_as_a_service_port():
    # Ray considers this value valid: it is available and not in allocated_ports.
    # The OS can nevertheless give it to a later automatic outbound connection.
    with patch('parrot_protocol.ray_startup.ephemeral_port_range', return_value=(49152, 65535)):
        selected = guarded_allocator(lambda _, excluded: 60000)(None, {12345})
    assert 1024 <= selected < 49152 and selected != 12345
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', selected))
        listener.listen()
        with socket.create_connection(listener.getsockname(), timeout=2) as client:
            with listener.accept()[0] as peer:
                client.sendall(b'owned-port')
                assert peer.recv(32) == b'owned-port'


def test_custom_ephemeral_range_is_used_instead_of_assumed_defaults():
    with patch('parrot_protocol.ray_startup.ephemeral_port_range', return_value=(1024, 60000)), \
         patch('parrot_protocol.ray_startup.secrets.randbelow', return_value=0):
        selected = guarded_allocator(lambda _, excluded: 12345)(None, {65535})
    assert 60000 < selected < 65535


def test_empty_safe_range_fails_without_probing_ephemeral_or_privileged_ports():
    with patch('parrot_protocol.ray_startup.ephemeral_port_range', return_value=(1024, 65535)), \
         patch('parrot_protocol.ray_startup.socket.socket', side_effect=AssertionError('unsafe probe')):
        with pytest.raises(RuntimeError, match='non-ephemeral'):
            guarded_allocator(lambda _, excluded: 60000)(None)


@pytest.mark.parametrize('candidate', [True, 80, 0, -1, 65536, None])
def test_invalid_or_privileged_auto_selection_is_not_published(candidate):
    first, last = ephemeral_port_range()
    selected = guarded_allocator(lambda _, excluded: candidate)(None)
    assert type(selected) is int and 1024 <= selected <= 65535
    assert not first <= selected <= last


def test_bind_errors_other_than_occupied_are_not_hidden_by_retry():
    class Probe:
        closed = False
        attempts = 0
        def bind(self, address):
            self.attempts += 1
            raise OSError(errno.EACCES, 'policy denied binding')
        def close(self): self.closed = True
    probe = Probe()
    with patch('parrot_protocol.ray_startup.socket.socket', return_value=probe):
        with pytest.raises(OSError) as error:
            guarded_allocator(lambda _, excluded: 0)(None)
    assert error.value.errno == errno.EACCES and probe.closed and probe.attempts == 1


@pytest.mark.parametrize('raw', ['49152 65535\n', '1024\t60999\n', '1 65535'])
def test_linux_range_reads_the_current_kernel_setting(raw):
    with patch('parrot_protocol.ray_startup.sys.platform', 'linux'), \
         patch('parrot_protocol.ray_startup.Path.open', return_value=io.StringIO(raw)):
        assert ephemeral_port_range() == tuple(map(int, raw.split()))


@pytest.mark.parametrize('raw', ['', '65535 49152', '0 65535', '1 65536',
                                 '49152 65535 10', '-1 65535', '四 65535', '1 ' + '9' * 129])
def test_malformed_kernel_range_is_rejected(raw):
    with patch('parrot_protocol.ray_startup.sys.platform', 'linux'), \
         patch('parrot_protocol.ray_startup.Path.open', return_value=io.StringIO(raw)):
        with pytest.raises(RuntimeError, match='cannot determine'):
            ephemeral_port_range()


def test_macos_range_reads_both_values_and_propagates_sysctl_failure():
    with patch('parrot_protocol.ray_startup.sys.platform', 'darwin'), \
         patch('parrot_protocol.ray_startup.subprocess.run', return_value=subprocess.CompletedProcess([], 0, '50000\n62000\n')):
        assert ephemeral_port_range() == (50000, 62000)
    with patch('parrot_protocol.ray_startup.sys.platform', 'darwin'), \
         patch('parrot_protocol.ray_startup.subprocess.run', side_effect=subprocess.TimeoutExpired('sysctl', 5)):
        with pytest.raises(RuntimeError, match='cannot determine'):
            ephemeral_port_range()


def test_range_discovery_failure_leaves_original_methods_installed():
    class Node:
        def _get_unused_port(self, allocated_ports=None): return 12345
        def start_raylet(self): pass
    allocator, start = Node._get_unused_port, Node.start_raylet
    with patch('parrot_protocol.ray_startup.ephemeral_port_range', side_effect=RuntimeError('range unavailable')):
        with pytest.raises(RuntimeError, match='range unavailable'):
            with guarded_ports(Node):
                raise AssertionError('must not start with an unknown range')
    assert Node._get_unused_port is allocator and Node.start_raylet is start


def test_safe_but_reserved_original_port_is_rejected():
    with patch('parrot_protocol.ray_startup.ephemeral_port_range', return_value=(49152, 65535)):
        selected = guarded_allocator(lambda _, excluded: 12345)(None, {12345})
    assert 1024 <= selected < 49152 and selected != 12345


def test_small_safe_range_exhausts_each_candidate_once_and_closes_all_probes():
    attempts, closed = [], []
    class Probe:
        def bind(self, address):
            attempts.append(address)
            raise OSError(errno.EADDRINUSE, 'occupied')
        def close(self): closed.append(self)
    with patch('parrot_protocol.ray_startup.ephemeral_port_range', return_value=(1024, 65531)), \
         patch('parrot_protocol.ray_startup.secrets.randbelow', return_value=0), \
         patch('parrot_protocol.ray_startup.socket.socket', side_effect=lambda *args: Probe()):
        with pytest.raises(RuntimeError, match='non-ephemeral'):
            guarded_allocator(lambda _, excluded: 60000)(None, {65534})
    assert len(attempts) == len(closed) == 3
    assert set(attempts) == {('', 65532), ('', 65533), ('', 65535)}


def test_missing_kernel_range_and_unsupported_platform_fail_explicitly():
    with patch('parrot_protocol.ray_startup.sys.platform', 'linux'), \
         patch('parrot_protocol.ray_startup.Path.open', side_effect=OSError('unavailable')):
        with pytest.raises(RuntimeError, match='cannot determine'):
            ephemeral_port_range()
    with patch('parrot_protocol.ray_startup.sys.platform', 'unsupported'):
        with pytest.raises(RuntimeError, match='requires Linux or macOS'):
            ephemeral_port_range()
