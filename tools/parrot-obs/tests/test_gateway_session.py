import contextlib,importlib.util,json,os,signal,socket,subprocess,sys,tempfile,time,unittest
from pathlib import Path
from unittest import mock
ROOT=Path(__file__).resolve().parents[3];TOOL=ROOT/'tools/parrot-obs'
sys.path.insert(0,str(TOOL))
from process_owner import OwnedChild
import gateway_manager as manager
import process_session as session
FIXTURE=Path(__file__).with_name('gateway_session_fixture.py')
EVIDENCE=Path(os.environ['PARROT_SESSION_EVIDENCE']) if 'PARROT_SESSION_EVIDENCE' in os.environ else Path(tempfile.mkdtemp(prefix='pg-session-evidence-'))
EVIDENCE.mkdir(parents=True,exist_ok=True)
def port_free(port):
 with socket.socket() as s:
  try:s.bind(('127.0.0.1',port));return True
  except OSError:return False
def wait_for(predicate):
 deadline=time.monotonic()+10
 while not predicate():
  if time.monotonic()>=deadline:raise AssertionError('fixture did not retire independently')
  time.sleep(.01)
class SessionReceipt(unittest.TestCase):
 def test_owned_child_waits_for_other_group_in_its_session(self):
  with tempfile.TemporaryDirectory(prefix='pg492-',dir='/tmp') as d:
   directory=Path(d);record=directory/'worker.json'
   owner=OwnedChild('session',[sys.executable,str(FIXTURE),'gateway',str(record)],directory/'gateway.log',marker='READY=')
   try:
    wait_for(record.exists);value=json.loads(record.read_text());self.assertNotEqual(value['group'],value['session']);self.assertEqual(value['session'],owner.process.pid)
    owner.begin_stop();time.sleep(2.2);start=time.monotonic();owner.finish_stop()
    value.update(retired=owner.retired,port_retired=port_free(value['port']),seconds=time.monotonic()-start)
    (EVIDENCE/'owned-child.json').write_text(json.dumps(value))
    self.assertTrue(value['port_retired'],'OwnedChild returned retired while session worker still listened')
   finally:
    with contextlib.suppress(Exception):owner.finish_stop()
    if record.exists():wait_for(lambda:port_free(json.loads(record.read_text())['port']))
 def test_manager_stop_waits_for_other_group_in_its_session(self):
  with tempfile.TemporaryDirectory(prefix='pg492-',dir='/tmp') as d:
   directory=Path(d);record=directory/'worker.json'
   config=dict(directory=d,startup_timeout=3,gateways=[dict(name='jvm',node='node',port=0,marker='READY=',cwd=str(ROOT),env={},command=[sys.executable,str(FIXTURE),'gateway',str(record)])])
   try:
    ready=manager.start(config);value=json.loads(record.read_text());self.assertNotEqual(value['group'],value['session']);self.assertEqual(ready['ports']['jvm'],value['port'])
    reply=manager.stop(directory);value.update(reply=reply,port_retired=port_free(value['port']))
    (EVIDENCE/'manager.json').write_text(json.dumps(value))
    self.assertTrue(value['port_retired'],'manager returned stopped while session worker still listened')
   finally:
    with contextlib.suppress(Exception):manager.stop(directory)
    if record.exists():wait_for(lambda:port_free(json.loads(record.read_text())['port']))
 def test_external_reap_never_authorizes_signal_to_cached_group(self):
  with tempfile.TemporaryDirectory(prefix='pg492-',dir='/tmp') as d:
   owner=OwnedChild('exited',[sys.executable,'-c','pass'],Path(d)/'gateway.log')
   wait_for(lambda:(owner.poll() or owner.code is not None))
   owner.process.kill();owner.process.wait(timeout=5)
   try:
    with mock.patch('os.killpg') as send:
     error=None
     try:owner.finish_stop()
     except Exception as caught:error=str(caught)
     value=dict(signals=[list(c.args) for c in send.call_args_list],error=error,retired=owner.retired)
     (EVIDENCE/'external-reap.json').write_text(json.dumps(value))
     self.assertEqual(send.call_count,0,'lost ownership must not signal a cached process group')
     self.assertIsNotNone(error,'lost ownership must return an error')
     with self.assertRaisesRegex(RuntimeError,'identity no longer owned'):owner.finish_stop()
     self.assertEqual(send.call_count,0,'repeated failed retirement must not signal a stale ID')
   finally:owner.channel.close()
 def test_ignored_sigchld_rejects_spawn_before_creating_a_guardian(self):
  with mock.patch('signal.getsignal',return_value=signal.SIG_IGN),mock.patch('subprocess.Popen') as spawn:
   with self.assertRaisesRegex(RuntimeError,'SIGCHLD'):
    OwnedChild('ignored',[sys.executable,'-c','pass'],EVIDENCE/'ignored.log')
   spawn.assert_not_called()
 def test_timeout_preserves_identity_and_retry_waits_without_signalling_observed_pids(self):
  with tempfile.TemporaryDirectory(prefix='pg492-',dir='/tmp') as d:
   record=Path(d)/'worker.json'
   process=subprocess.Popen([sys.executable,str(FIXTURE),'gateway',str(record)],stdout=subprocess.DEVNULL,start_new_session=True)
   try:
    wait_for(record.exists);value=json.loads(record.read_text())
    with mock.patch('os.killpg',wraps=os.killpg) as send:
     session.final_signal(process)
     with self.assertRaises(TimeoutError):session.wait_retired(process,session.retirement_deadline(.05))
     self.assertFalse(port_free(value['port']))
     self.assertIsNotNone(session.still_owned(process))
     self.assertEqual(session.wait_retired(process,session.retirement_deadline(5)),-signal.SIGKILL)
     self.assertTrue(port_free(value['port']))
     self.assertEqual([call.args for call in send.call_args_list],[(process.pid,signal.SIGKILL)])
    (EVIDENCE/'timeout-retry.json').write_text(json.dumps(value))
   finally:
    if process.returncode is None:
     session.final_signal(process);session.wait_retired(process,session.retirement_deadline())
 def test_invalid_budget_is_rejected(self):
  for budget in [0,-1,float('nan'),float('inf')]:
   with self.subTest(budget=budget),self.assertRaises(ValueError):session.retirement_deadline(budget)
 @unittest.skipUnless(sys.platform=='linux','Linux /proc thread semantics')
 def test_zombie_leader_with_live_thread_is_not_retired(self):
  with tempfile.TemporaryDirectory(prefix='pg492-',dir='/tmp') as d:
   record=Path(d)/'thread.json'
   script = """import ctypes,json,os,socket,sys,threading,time
from pathlib import Path
ready=threading.Event()
def worker():
 s=socket.socket();s.bind(('127.0.0.1',0));s.listen()
 p=Path(sys.argv[1]);t=p.with_suffix('.tmp');t.write_text(json.dumps(dict(port=s.getsockname()[1])));t.replace(p)
 ready.set();time.sleep(120)
threading.Thread(target=worker).start();ready.wait()
libc=ctypes.CDLL(None);libc.pthread_exit.argtypes=[ctypes.c_void_p];libc.pthread_exit(None)
"""
   process=subprocess.Popen([sys.executable,'-c',script,str(record)],start_new_session=True)
   try:
    wait_for(record.exists)
    wait_for(lambda:Path('/proc/'+str(process.pid)+'/stat').read_bytes().rsplit(b')',1)[1].split()[0]==b'Z')
    value=json.loads(record.read_text());self.assertFalse(port_free(value['port']))
    self.assertTrue(session.session_live(process.pid,session.retirement_deadline()),'zombie main thread still has a live listening thread')
    (EVIDENCE/'zombie-live-thread.json').write_text(json.dumps(dict(pid=process.pid,**value)))
   finally:
    session.final_signal(process);session.wait_retired(process,session.retirement_deadline())
 @unittest.skipUnless(sys.platform=='linux','Linux /proc input semantics')
 def test_stat_parser_preserves_non_utf8_names_and_rejects_malformed_data(self):
  with tempfile.TemporaryDirectory(prefix='pg492-',dir='/tmp') as d:
   p=Path(d)/'stat';p.write_bytes(b'123 (name ) with \xff) S 1 123 123 0 0')
   self.assertEqual(session._stat(p),(b'S',123))
   p.write_bytes(b'123 no closing bracket')
   with self.assertRaises(RuntimeError):session._stat(p)
   p.write_bytes(b'x'*4097)
   with self.assertRaises(RuntimeError):session._stat(p)
if __name__=='__main__':unittest.main()
