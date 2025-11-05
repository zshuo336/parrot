"""A stalled child cannot block owner-loss observation or hold source forever."""
import json
import os
from pathlib import Path
import socket
import signal
import subprocess
import sys
import time
import types

import pytest

from parrot_protocol.ray_artifacts import Snapshot
from parrot_protocol.ray_job import receive, send
from parrot_protocol import ray_job_guard as guard

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'apps/test_support'))
from owned_process import OwnedProcess, process_exited

BOOTSTRAP = '''import json,subprocess,sys
from pathlib import Path
from parrot_protocol.ray_job_guard import supervise
original=subprocess.Popen
def launch(command,**kwargs):
    if command[2]=='parrot_protocol.ray_job_guard':
        process=original(command,**kwargs)
        (Path(sys.argv[4])/'watchdog.json').write_text(json.dumps({'pid':process.pid}))
        return process
    assert command[1:3]==['-m','parrot_protocol.ray_job'] and command[-1]=='--driver'
    return original([sys.executable,sys.argv[2],command[3],sys.argv[3],sys.argv[4]],**kwargs)
subprocess.Popen=launch
supervise(int(sys.argv[1]))
'''
CHILD = '''import ctypes,json,os,socket,struct,sys
from pathlib import Path
from parrot_protocol.ray_job import receive,send,MAX_CONTROL_BYTES
connection=socket.socket(fileno=int(sys.argv[1]));mode=sys.argv[2];root=Path(sys.argv[3])
def ready():
    (root/'ready.tmp').write_text(json.dumps({'pid':os.getpid()}));(root/'ready.tmp').replace(root/'ready.json')
if mode=='before-config':
    ready();ctypes.PyDLL(None).sleep(60)
config=receive(connection)
from parrot_protocol.ray_snapshot_lease import hold_snapshot_lease
hold_snapshot_lease(config['runtime_env']['env_vars']['PARROT_SNAPSHOT_CONTROL'])
if mode=='partial':connection.sendall(struct.pack('!I',100)+b'{')
elif mode=='backpressure':send(connection,'x'*(MAX_CONTROL_BYTES-2))
elif mode=='oversize':connection.sendall(struct.pack('!I',MAX_CONTROL_BYTES+1))
elif mode=='duplicate':connection.sendall(b'\\x00\\x00\\x00\\x02{}'*2)
else:send(connection,{'job':'02000000','namespace':'test'})
ready()
if mode=='cooperative':
    if receive(connection) is not None:raise RuntimeError('expected EOF')
    (root/'cooperative-exit').touch()
else:ctypes.PyDLL(None).sleep(60)
'''


@pytest.mark.parametrize('mode', ['cooperative','before-config','partial','backpressure','oversize','duplicate','unexpected-owner'])
def test_guard_reaps_child_even_when_config_receipt_or_shutdown_is_stalled(tmp_path, mode):
    source=tmp_path/'source';source.mkdir();(source/'entry.py').write_text('value=1\n')
    snapshot=Snapshot(source)
    snapshot_path=Path(snapshot.root)
    (tmp_path/'guard.py').write_text(BOOTSTRAP);(tmp_path/'child.py').write_text(CHILD)
    owner,remote=socket.socketpair();owner.settimeout(5)
    env=dict(os.environ,PYTHONPATH=str(Path(__file__).resolve().parents[1]))
    with (tmp_path/'guard.log').open('w') as log:
        process=subprocess.Popen([sys.executable,str(tmp_path/'guard.py'),str(remote.fileno()),str(tmp_path/'child.py'),mode,str(tmp_path)],
            pass_fds=(remote.fileno(),),env=env,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
        remote.close()
        try:
            send(owner,dict(address='existing:1234',namespace='test',node='0'*56,root=snapshot.root,
                runtime_env={'env_vars':{'PARROT_SNAPSHOT_CONTROL':str(snapshot._lease.path),'LARGE':'x'*500000}}))
            deadline=time.monotonic()+5
            while not (tmp_path/'ready.json').exists():
                if process.poll() is not None or time.monotonic()>deadline:
                    raise AssertionError((tmp_path/'guard.log').read_text())
                time.sleep(.01)
            if mode=='before-config':
                with pytest.raises(TimeoutError,match='leased'):snapshot._lease.close(timeout=0)
                assert Path(snapshot.root).is_dir(), 'guard must protect source before child reads configuration'
            if mode in ['cooperative','unexpected-owner']:
                assert receive(owner)=={'job':'02000000','namespace':'test'}
            if mode=='unexpected-owner':owner.sendall(b'!')
            if mode not in ['oversize','duplicate','unexpected-owner']:owner.close()
            status=process.wait(timeout=7)
            assert status==(1 if mode in ['oversize','duplicate','unexpected-owner'] else 0), (tmp_path/'guard.log').read_text()
            pid=json.loads((tmp_path/'ready.json').read_text())['pid']
            with pytest.raises(ProcessLookupError):os.kill(pid,0)
            if mode=='cooperative':assert (tmp_path/'cooperative-exit').exists()
            snapshot.close();assert not snapshot_path.exists()
        finally:
            owner.close()
            # Closing the control channel must let a correct guard reap its
            # child. A failed mutant fixture remains bounded by the 60s stall.
            if process.poll() is None:
                try:process.wait(timeout=5)
                except subprocess.TimeoutExpired:process.kill();process.wait(timeout=5)
            snapshot.close()


def test_eof_before_configuration_allocates_no_child(monkeypatch):
    owner,remote=socket.socketpair();owner.close()
    def forbidden(*args,**kwargs):raise AssertionError('allocated child without request')
    monkeypatch.setattr(guard.subprocess,'Popen',forbidden)
    guard.supervise(remote.detach())


def test_failed_reap_is_reported_after_only_owned_process_is_killed():
    calls=[]
    class Process:
        def wait(self,timeout):
            calls.append(('wait',timeout))
            raise subprocess.TimeoutExpired('own child',timeout)
        def kill(self):calls.append('kill')
    with pytest.raises(subprocess.TimeoutExpired):
        guard.retire(Process(),types.SimpleNamespace(close=lambda:calls.append('eof')))
    assert calls==['eof',('wait',guard.GRACE_SECONDS),'kill',('wait',guard.KILL_WAIT_SECONDS)]


def test_guard_refuses_shared_session_before_spawning(monkeypatch):
    owner,remote=socket.socketpair()
    send(owner,dict(address='existing:1',namespace='test',node='node',root='/payload',runtime_env={}))
    monkeypatch.setattr(guard.os,'getsid',lambda _: -1)
    def forbidden(*args,**kwargs):raise AssertionError('spawned into unrelated session')
    monkeypatch.setattr(guard.subprocess,'Popen',forbidden)
    try:
        with pytest.raises(RuntimeError,match='own session'):guard.supervise(remote.detach())
        assert owner.recv(1)==b''
    finally:owner.close();remote.close()


def test_failed_driver_reap_keeps_watchdog_armed(monkeypatch):
    owner,remote=socket.socketpair();owner.settimeout(1)
    send(owner,dict(address='existing:1',namespace='test',node='node',root='/payload',
                    runtime_env={'env_vars':{'PARROT_SNAPSHOT_CONTROL':'/control'}}))
    owner.shutdown(socket.SHUT_WR)
    lifetime,watcher=socket.socketpair();watcher.settimeout(1)
    monkeypatch.setattr(guard.os,'getpgrp',os.getpid)
    monkeypatch.setattr(guard.os,'getsid',lambda _:os.getpid())
    monkeypatch.setattr(guard,'hold_snapshot_lease',lambda _:None)
    monkeypatch.setattr(guard,'arm_watchdog',lambda _:(object(),lifetime))
    monkeypatch.setattr(guard.subprocess,'Popen',lambda *a,**k:types.SimpleNamespace(poll=lambda:None))
    def failed_reap(process,connection):
        connection.close()
        raise subprocess.TimeoutExpired('native driver',1)
    monkeypatch.setattr(guard,'retire',failed_reap)
    def forbidden(*args):raise AssertionError('disarmed before reaping driver')
    monkeypatch.setattr(guard,'disarm_watchdog',forbidden)
    try:
        with pytest.raises(subprocess.TimeoutExpired):guard.supervise(remote.detach())
        assert watcher.recv(1)==b'', 'watchdog must receive EOF, not the disarm command'
        assert owner.recv(1)==b''
    finally:owner.close();remote.close();lifetime.close();watcher.close()


@pytest.mark.parametrize('fault', ['spawn', 'readiness'])
def test_watchdog_allocation_failure_closes_fds_and_reaps_started_child(monkeypatch, fault):
    endpoints=[];calls=[];original=socket.socketpair
    def pair():
        endpoints.extend(original());return tuple(endpoints)
    def launch(*args,**kwargs):
        if fault=='spawn':raise OSError('spawn failed')
        endpoints[1].sendall(b'?')
        return types.SimpleNamespace(kill=lambda:calls.append('kill'),wait=lambda timeout:calls.append('wait'))
    monkeypatch.setattr(guard.socket,'socketpair',pair)
    monkeypatch.setattr(guard.subprocess,'Popen',launch)
    with pytest.raises((OSError,RuntimeError)):
        guard.arm_watchdog('/control')
    assert all(endpoint.fileno()==-1 for endpoint in endpoints)
    assert calls==([] if fault=='spawn' else ['kill','wait'])


@pytest.mark.parametrize('command,expected', [(b'D',0),(b'?',-signal.SIGKILL),(b'',-signal.SIGKILL)])
def test_watchdog_holds_lease_until_disarm_or_group_retirement(tmp_path, command, expected):
    source=tmp_path/'source';source.mkdir();(source/'entry.py').write_text('owned')
    snapshot=Snapshot(source);path=Path(snapshot.root)
    owner,remote=socket.socketpair();owner.settimeout(5)
    import owned_process
    original=owned_process.subprocess.Popen
    def launch(args,**kwargs):return original(args,pass_fds=(remote.fileno(),),**kwargs)
    from unittest.mock import patch
    env=dict(os.environ,PYTHONPATH=str(Path(__file__).resolve().parents[1]))
    with patch('owned_process.subprocess.Popen',side_effect=launch):
        process=OwnedProcess([sys.executable,'-m','parrot_protocol.ray_job_guard','--watch',
            str(remote.fileno()),str(snapshot._lease.path)],env=env)
    remote.close()
    try:
        assert owner.recv(1)==b'R'
        with pytest.raises(TimeoutError,match='leased'):snapshot._lease.close(timeout=0)
        if command:owner.sendall(command)
        else:owner.close()
        assert process.wait(5)==expected
        snapshot.close();assert not path.exists()
    finally:
        owner.close();process.close();snapshot.close()


@pytest.mark.parametrize('victim', ['guard', 'watchdog'])
@pytest.mark.parametrize('mode', ['before-config', 'stalled'])
def test_guard_death_retires_native_stall_and_releases_source(tmp_path, victim, mode):
    source=tmp_path/'source';source.mkdir();(source/'entry.py').write_text('value=1\n')
    snapshot=Snapshot(source)
    snapshot_path=Path(snapshot.root)
    owner,remote=socket.socketpair();owner.settimeout(5)
    (tmp_path/'guard.py').write_text(BOOTSTRAP);(tmp_path/'child.py').write_text(CHILD)
    env=dict(os.environ,PYTHONPATH=str(Path(__file__).resolve().parents[1]))
    # Transfer only this fixture's socket. OwnedProcess retains the actual
    # guard's waitable identity until the last fallback group signal.
    import owned_process
    original=owned_process.subprocess.Popen
    def launch(command, **kwargs):
        return original(command,pass_fds=(remote.fileno(),),**kwargs)
    from unittest.mock import patch
    with (tmp_path/'guard.log').open('w') as log:
        with patch('owned_process.subprocess.Popen',side_effect=launch):
            process=OwnedProcess([sys.executable,str(tmp_path/'guard.py'),str(remote.fileno()),
                str(tmp_path/'child.py'),mode,str(tmp_path)],env=env,stdout=log,stderr=subprocess.STDOUT)
        remote.close()
        try:
            send(owner,dict(address='existing:1234',namespace='test',node='0'*56,root=snapshot.root,
                runtime_env={'env_vars':{'PARROT_SNAPSHOT_CONTROL':str(snapshot._lease.path)}}))
            if mode != 'before-config':
                assert receive(owner)=={'job':'02000000','namespace':'test'}
            deadline=time.monotonic()+5
            while not (tmp_path/'ready.json').exists():
                assert process.poll() is None and time.monotonic()<deadline
                time.sleep(.01)
            with pytest.raises(TimeoutError,match='leased'):snapshot._lease.close(timeout=0)
            if victim=='guard':
                process.send_signal(signal.SIGKILL)
            else:
                os.kill(json.loads((tmp_path/'watchdog.json').read_text())['pid'],signal.SIGKILL)
            assert process.wait(5)==(-signal.SIGKILL if victim=='guard' else 1)
            # Keep the coordinator's endpoint open: guard loss alone must be
            # enough. A GIL-holding driver's socket read cannot do this job.
            snapshot.close()
            assert not snapshot_path.exists()
            assert (source/'entry.py').read_text()=='value=1\n'
            assert process_exited(json.loads((tmp_path/'ready.json').read_text())['pid'])
            assert process_exited(json.loads((tmp_path/'watchdog.json').read_text())['pid'])
        finally:
            owner.close()
            process.close()
            snapshot.close()
