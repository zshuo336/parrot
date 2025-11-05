import concurrent.futures
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest

TOOL=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('gateway_manager',TOOL/'gateway_manager.py')
manager=importlib.util.module_from_spec(spec);spec.loader.exec_module(manager)
FIXTURE=Path(__file__).with_name('gateway_fixture.py')


def wait_for(predicate,timeout=10):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        value=predicate()
        if value:return value
        time.sleep(0.02)
    raise AssertionError('condition did not complete before deadline')


def port_free(port):
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        try:listener.bind(('127.0.0.1',port));return True
        except OSError:return False


class Ownership(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(prefix='pg-own-',dir='/tmp')
        self.directory=Path(self.temp.name)
        self.config={'directory':str(self.directory),'startup_timeout':3.0,'gateways':[
            dict(name=name,node=name+'-node',port=0,marker='READY=',cwd=str(TOOL),env={},
                 command=[sys.executable,str(FIXTURE),'--starts',str(self.directory/(name+'.starts'))])
            for name in ['erl','jvm','ray']]}

    def tearDown(self):
        with contextlib.suppress(Exception):manager.stop(self.directory)
        wait_for(lambda:not manager.owner_active(self.directory))
        self.temp.cleanup()

    def test_start_is_idempotent_and_down_preserves_unrelated_same_name_process(self):
        sentinel=subprocess.Popen([sys.executable,str(FIXTURE),'--tag','parrot_gw:main ParrotGatewayMain parrot_protocol.ray_gw'],stdout=subprocess.PIPE,text=True,start_new_session=True)
        try:
            self.assertTrue(sentinel.stdout.readline().startswith('READY='))
            first=manager.start(self.config);second=manager.start(self.config)
            self.assertEqual(first['ports'],second['ports'])
            for entry in self.config['gateways']:
                self.assertEqual(len((self.directory/(entry['name']+'.starts')).read_text().splitlines()),1)
            # A forged/stale PID file is irrelevant: down talks only to the owner.
            (self.directory/'pid').write_text(str(sentinel.pid))
            self.assertEqual(manager.stop(self.directory)['phase'],'stopped')
            self.assertIsNone(sentinel.poll())
            for port in first['ports'].values():self.assertTrue(port_free(port))
            self.assertEqual(manager.stop(self.directory)['phase'],'stopped')
        finally:
            if sentinel.poll() is None:sentinel.terminate()
            sentinel.wait(timeout=5);sentinel.stdout.close()

    def test_concurrent_launchers_join_one_owner_and_configuration_conflicts_fail(self):
        # flock coordinates independent opens, including separate callers here.
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results=list(pool.map(manager.start,[self.config,self.config]))
        self.assertEqual(results[0]['ports'],results[1]['ports'])
        changed=json.loads(json.dumps(self.config));changed['gateways'][0]['node']='different'
        with self.assertRaisesRegex(RuntimeError,'different gateway configuration'):manager.start(changed)
        self.assertEqual(manager.request(self.directory,'status')['ports'],results[0]['ports'])

    def test_partial_failure_and_false_readiness_reclaim_all_started_groups(self):
        for mode in ['exit','bad','never']:
            with self.subTest(mode=mode):
                config=json.loads(json.dumps(self.config));config['startup_timeout']=2.0
                config['gateways'][-1]['command']+=['--mode',mode]
                with self.assertRaises(RuntimeError):manager.start(config)
                self.assertFalse(manager.owner_active(self.directory))
                state=json.loads((self.directory/'state.json').read_text())
                self.assertEqual(state['phase'],'failed')
                for port in state['ports'].values():self.assertTrue(port_free(port))

    def test_occupied_port_is_not_mistaken_for_our_gateway_readiness(self):
        with socket.socket() as unrelated:
            unrelated.bind(('127.0.0.1',0));unrelated.listen(1);port=unrelated.getsockname()[1]
            self.config['gateways'][0]['port']=port
            self.config['gateways'][0]['command']+=['--port',str(port)]
            with self.assertRaises(RuntimeError):manager.start(self.config)
            self.assertEqual(unrelated.getsockname()[1],port)
            self.assertFalse(manager.owner_active(self.directory))

    def test_daemon_sigkill_retires_guardian_groups_and_their_term_ignoring_children(self):
        for entry in self.config['gateways']:entry['command']+=['--mode','child']
        manager.write_json(self.directory/'config.json',self.config)
        daemon=subprocess.Popen([sys.executable,str(TOOL/'gateway_manager.py'),'_serve',str(self.directory/'config.json')],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        try:
            value=wait_for(lambda:(v if (v:=manager.request(self.directory,'status')) and v['phase']=='running' else None))
            daemon.kill();daemon.wait(timeout=5)
            # The manager is gone, but the guardians still own its lease during
            # their grace period, so a new launcher cannot claim this session.
            self.assertTrue(manager.owner_active(self.directory))
            with self.assertRaisesRegex(RuntimeError,'owner is active'):
                manager.start(self.config)
            for port in value['ports'].values():wait_for(lambda:port_free(port),timeout=8)
        finally:
            if daemon.poll() is None:daemon.terminate();daemon.wait(timeout=8)

    def test_child_exit_after_readiness_stops_the_whole_owned_session(self):
        self.config['gateways'][0]['command']+=['--mode','crash']
        value=manager.start(self.config)
        wait_for(lambda:not manager.owner_active(self.directory))
        self.assertEqual(json.loads((self.directory/'state.json').read_text())['phase'],'failed')
        for port in value['ports'].values():self.assertTrue(port_free(port))

    def test_inline_runtime_retains_group_and_normal_stop_retires_descendants(self):
        for entry in self.config['gateways']:
            entry['python_entrypoint'] = True
            entry['command'] += ['--mode', 'inline-child']
        value = manager.start(self.config)
        self.assertEqual(manager.stop(self.directory)['phase'], 'stopped')
        for port in value['ports'].values():
            self.assertTrue(port_free(port))
        self.assertFalse(manager.owner_active(self.directory))

    def test_inline_runtime_abrupt_exit_is_not_masked_by_watchdog_control_socket(self):
        entry = self.config['gateways'][0]
        entry['python_entrypoint'] = True
        entry['command'] += ['--mode', 'inline-crash']
        value = manager.start(self.config)
        wait_for(lambda: not manager.owner_active(self.directory), timeout=8)
        state = json.loads((self.directory / 'state.json').read_text())
        self.assertEqual(state['phase'], 'failed')
        for port in value['ports'].values():
            self.assertTrue(port_free(port))

    def test_final_signal_error_does_not_skip_peer_cleanup_or_guardian_waits(self):
        for entry in self.config['gateways']:
            entry['command'] += ['--mode', 'child']
        manager.write_json(self.directory / 'config.json', self.config)
        driver = self.directory / 'signal_error.py'
        driver.write_text('''import importlib.util,json,signal,sys
from pathlib import Path
spec=importlib.util.spec_from_file_location('manager',sys.argv[1])
manager=importlib.util.module_from_spec(spec);spec.loader.exec_module(manager)
original=manager.os.killpg
injected=False
def fail_once(group,signum):
    global injected
    original(group,signum)
    if signum==signal.SIGKILL and not injected:
        injected=True
        raise PermissionError('injected acknowledgement failure after delivery')
manager.os.killpg=fail_once
state=manager.serve(json.loads(Path(sys.argv[2]).read_text()))
raise SystemExit(1 if state['phase']=='failed' else 0)
''')
        daemon = subprocess.Popen([sys.executable, str(driver), str(TOOL / 'gateway_manager.py'),
                                   str(self.directory / 'config.json')])
        try:
            state = wait_for(lambda: (value if (value := manager.request(self.directory, 'status'))
                                     and value['phase'] == 'running' else None))
            with self.assertRaisesRegex(RuntimeError, 'signal erl'):
                manager.stop(self.directory)
            self.assertEqual(daemon.wait(timeout=8), 1)
            saved = json.loads((self.directory / 'state.json').read_text())
            self.assertEqual(saved['phase'], 'failed')
            self.assertEqual(saved['guardian_exits'], dict(erl=-9, jvm=-9, ray=-9))
            self.assertFalse((self.directory / 'control.sock').exists())
            for port in state['ports'].values():
                self.assertTrue(port_free(port))
        finally:
            if daemon.poll() is None:
                daemon.kill()
            daemon.wait(timeout=5)

    def test_inline_watchdog_holds_lease_after_manager_and_runtime_death(self):
        for entry in self.config['gateways']:
            entry['python_entrypoint'] = True
            entry['command'] += ['--mode', 'inline-child']
        manager.write_json(self.directory / 'config.json', self.config)
        daemon = subprocess.Popen([sys.executable, str(TOOL / 'gateway_manager.py'), '_serve',
                                   str(self.directory / 'config.json')])
        try:
            state = wait_for(lambda: (value if (value := manager.request(self.directory, 'status'))
                                     and value['phase'] == 'running' else None))
            daemon.kill()
            daemon.wait(timeout=5)
            # Allow the fatal TERM handlers to run. Descendants ignore TERM;
            # the independent watchdog must still own the inherited flock.
            time.sleep(0.4)
            self.assertTrue(manager.owner_active(self.directory))
            self.assertTrue(any(not port_free(port) for port in state['ports'].values()))
            with self.assertRaisesRegex(RuntimeError, 'owner is active'):
                manager.start(self.config)
            for port in state['ports'].values():
                wait_for(lambda: port_free(port), timeout=8)
            wait_for(lambda: not manager.owner_active(self.directory), timeout=8)
        finally:
            if daemon.poll() is None:
                daemon.kill()
            daemon.wait(timeout=5)

    def test_invalid_control_clients_cannot_crash_the_owner(self):
        value=manager.start(self.config)
        for raw in [b'[]\n',b'null\n',b'not-json\n',b'x'*5000+b'\n']:
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(2);client.connect(str(self.directory/'control.sock'))
                with contextlib.suppress(OSError):client.sendall(raw);client.recv(1024)
        self.assertEqual(manager.request(self.directory,'status')['ports'],value['ports'])

    def test_private_directory_and_log_selector_boundaries(self):
        shared=self.directory/'shared';shared.mkdir(mode=0o755)
        with self.assertRaises(ValueError):manager.private_directory(shared)
        linked=self.directory/'linked';linked.symlink_to(self.directory,target_is_directory=True)
        with self.assertRaises(ValueError):manager.private_directory(linked)
        process=subprocess.run([sys.executable,str(TOOL/'gateway_manager.py'),'log','../../elsewhere'],capture_output=True)
        self.assertNotEqual(process.returncode,0)

    def test_state_write_failure_cannot_interrupt_retirement_or_claim_success(self):
        for entry in self.config['gateways']:entry['command']+=['--mode','child']
        value=manager.start(self.config)
        # A real filesystem failure at the first shutdown diagnostic write.
        (self.directory/'state.json.tmp').mkdir()
        with self.assertRaisesRegex(RuntimeError,'state persistence failed'):
            manager.stop(self.directory)
        self.assertFalse(manager.owner_active(self.directory))
        for port in value['ports'].values():self.assertTrue(port_free(port))

    def test_trickling_control_client_has_one_total_read_deadline(self):
        value=manager.start(self.config)
        with socket.socket(socket.AF_UNIX) as client:
            client.connect(str(self.directory/'control.sock'))
            # Admit this client before the ordinary status request; continuous
            # partial input must not extend the server's read deadline.
            client.sendall(b'{')
            sending=threading.Event()
            def trickle():
                sending.set()
                deadline=time.monotonic()+3
                while time.monotonic()<deadline:
                    try:client.sendall(b' ')
                    except OSError:return
                    time.sleep(0.1)
            thread=threading.Thread(target=trickle)
            thread.start()
            try:
                self.assertTrue(sending.wait(1))
                self.assertEqual(manager.request(self.directory,'status',timeout=1.5)['ports'],value['ports'])
            finally:thread.join(timeout=5)


if __name__=='__main__':unittest.main()
