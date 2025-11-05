"""Physical sources and dependencies, exercised through the production executor."""
import os
import queue
import shutil
import sys
import types

import pytest

from parrot_protocol.ray_gw import ParrotDispatcher, RayAdminExecutor

SOURCE = '''from parrot_protocol.ray_gw import ParrotDispatcher
import helper
from pkg import part
import importlib

def parrot_entry(ctx):
    d = ParrotDispatcher()
    @d.handler('read')
    def read(key, payload):
        import lazy
        return key, '|'.join([helper.VALUE, part.VALUE, lazy.VALUE,
                             importlib.import_module('dynamic').VALUE]).encode()
    return d
'''


def artifact(root, value):
    root.mkdir(parents=True, exist_ok=True)
    (root/'entry.py').write_text(SOURCE)
    (root/'pkg').mkdir(exist_ok=True)
    (root/'pkg/__init__.py').write_text('from . import part\n')
    for relative in ['helper.py', 'pkg/part.py', 'lazy.py', 'dynamic.py']:
        (root/relative).write_text('VALUE = ' + repr(value) + '\n')
    return root


def component(name, root, module='entry'):
    return dict(name=name, version=name, artifact=dict(kind='pymodule', module=module,
                uri=root.as_uri() if hasattr(root, 'as_uri') else root), instances=dict(kind='singleton'))


@pytest.fixture
def executor():
    names = ['entry','helper','pkg','pkg.part','lazy','dynamic']
    saved = {n:sys.modules[n] for n in names if n in sys.modules}
    path = list(sys.path)
    for n in names: sys.modules.pop(n, None)
    ex = RayAdminExecutor(queue.Queue(), types.SimpleNamespace(), ParrotDispatcher())
    yield ex
    for name in list(ex._components): ex.stop('/user/' + name)
    for n in names: sys.modules.pop(n, None)
    sys.modules.update(saved)
    sys.path[:] = path


def read(ex, name):
    result = ex._runtime().ask('/user/' + name, 'read', b'')
    assert result['ok'], result
    return result['payload'].decode()


def test_distinct_roots_same_module_and_dependencies(executor, tmp_path):
    for value in ['A','B']:
        assert executor.deploy(component(value, artifact(tmp_path/value, value)))['kind'] == 'deployed'
    for name in ['A','B','A','B']:
        assert read(executor, name) == '|'.join([name]*4)


def test_redeploy_same_size_mtime_keeps_old_lazy_imports(executor, tmp_path):
    root = artifact(tmp_path/'source', 'A')
    assert executor.deploy(component('old', root))['kind'] == 'deployed'
    stats = {p:p.stat() for p in root.rglob('*.py')}
    artifact(root, 'B')
    for p, st in stats.items(): os.utime(p, ns=(st.st_atime_ns,st.st_mtime_ns))
    assert executor.deploy(component('new', root))['kind'] == 'deployed'
    assert read(executor, 'old') == 'A|A|A|A'
    assert read(executor, 'new') == 'B|B|B|B'
    executor.stop('/user/new')
    artifact(root, 'C')
    assert executor.deploy(component('new', root))['kind'] == 'deployed'
    assert read(executor, 'new') == 'C|C|C|C'
    assert read(executor, 'old') == 'A|A|A|A'


def test_original_directory_can_disappear_before_lazy_import(executor, tmp_path):
    root = artifact(tmp_path/'source', 'A')
    assert executor.deploy(component('old', root))['kind'] == 'deployed'
    shutil.rmtree(root)
    assert read(executor, 'old') == 'A|A|A|A'


def test_percent_encoded_file_uri(executor, tmp_path):
    root = artifact(tmp_path/'源 code # %', 'A')
    assert executor.deploy(component('unicode', root))['kind'] == 'deployed'
    assert read(executor, 'unicode') == 'A|A|A|A'


@pytest.mark.parametrize('uri', ['https://example.invalid/code', 'file://remote/tmp/code',
    'file:///tmp/code?query=1', 'file:///tmp/code#fragment', 'file:relative',
    'file:///tmp/%ZZ', 'file:///tmp/%ff'])
def test_invalid_uri_before_import_or_registry(executor, monkeypatch, uri):
    effects = []
    module = types.ModuleType('entry')
    module.parrot_entry = lambda ctx: (effects.append(ctx), ParrotDispatcher())[1]
    monkeypatch.setitem(sys.modules, 'entry', module)
    assert executor.deploy(component('invalid', uri))['kind'] == 'failed'
    assert effects == []
    assert executor._components == {}


def test_host_import_state_is_restored(executor, tmp_path, monkeypatch):
    sentinel = types.ModuleType('helper')
    sentinel.VALUE = 'HOST'
    monkeypatch.setitem(sys.modules, 'helper', sentinel)
    before = list(sys.path)
    assert executor.deploy(component('app', artifact(tmp_path/'source', 'A')))['kind'] == 'deployed'
    assert read(executor, 'app') == 'A|A|A|A'
    assert sys.modules['helper'] is sentinel
    assert sys.path == before
    assert 'entry' not in sys.modules and 'pkg' not in sys.modules and 'lazy' not in sys.modules


def test_snapshot_retained_until_last_path_then_removed(executor, tmp_path):
    from pathlib import Path
    comp = component('pool', artifact(tmp_path/'source','A'))
    comp['instances'] = dict(kind='pool',count=2)
    assert executor.deploy(comp)['kind'] == 'deployed'
    runtime = executor._runtime()
    root = Path(runtime._artifacts['pool']['snapshot'].root)
    assert root.exists()
    executor.stop('/user/pool-0')
    assert root.exists() and read(executor,'pool-1') == 'A|A|A|A'
    executor.stop('/user/pool-1')
    assert not root.exists() and runtime._artifacts == {}


@pytest.mark.parametrize('kind', ['bytes','files','depth','symlink','missing-entry','reserved'])
def test_bad_sources_leave_no_snapshot_or_factory_effects(executor, tmp_path, monkeypatch, kind):
    from pathlib import Path
    from parrot_protocol import ray_artifacts as artifacts
    root = artifact(tmp_path/'source','A')
    temp = tmp_path/'snapshots'; temp.mkdir()
    monkeypatch.setattr(artifacts.tempfile,'tempdir',str(temp))
    if kind == 'bytes': monkeypatch.setattr(artifacts,'MAX_BYTES',4)
    elif kind == 'files': monkeypatch.setattr(artifacts,'MAX_FILES',2)
    elif kind == 'depth': monkeypatch.setattr(artifacts,'MAX_DEPTH',0)
    elif kind == 'symlink': (root/'escape').symlink_to(tmp_path/'outside')
    elif kind == 'missing-entry': (root/'entry.py').unlink()
    else: (root/'ray.py').write_text('raise AssertionError("must not import")')
    assert executor.deploy(component('bad',root))['kind'] == 'failed'
    assert executor._components == {} and executor._runtime()._artifacts == {}
    assert list(Path(temp).iterdir()) == []


def test_cleanup_failure_retains_owner_and_retry_releases(executor, tmp_path, monkeypatch):
    from pathlib import Path
    from parrot_protocol import ray_snapshot_lease
    assert executor.deploy(component('app',artifact(tmp_path/'source','A')))['kind'] == 'deployed'
    root = Path(executor._runtime()._artifacts['app']['snapshot'].root)
    original = ray_snapshot_lease.remove_snapshot_payload
    def fail_once(directory, manifest):
        raise OSError('injected cleanup failure')
    monkeypatch.setattr(ray_snapshot_lease,'remove_snapshot_payload',fail_once)
    with pytest.raises(RuntimeError,match='incomplete'): executor.stop('/user/app')
    assert root.exists() and executor.status('/user/app')['states'][0]['state'] == 'stop_failed'
    monkeypatch.setattr(ray_snapshot_lease,'remove_snapshot_payload',original)
    executor.stop('/user/app')
    assert not root.exists()


def test_bytecode_is_not_an_artifact_version(executor, tmp_path):
    import py_compile
    root = artifact(tmp_path/'source','A')
    stats = {p:p.stat() for p in root.rglob('*.py')}
    for p in stats: py_compile.compile(str(p),doraise=True)
    artifact(root,'B')
    for p, st in stats.items(): os.utime(p,ns=(st.st_atime_ns,st.st_mtime_ns))
    assert executor.deploy(component('fresh',root))['kind'] == 'deployed'
    assert read(executor,'fresh') == 'B|B|B|B'


def test_missing_uri_resolves_package_root_without_importing_parent(executor, tmp_path, monkeypatch):
    root = artifact(tmp_path/'source','P')
    (root/'pkg/app.py').write_text(SOURCE)
    monkeypatch.syspath_prepend(str(root))
    comp = component('app',root,module='pkg.app')
    del comp['artifact']['uri']
    assert 'pkg' not in sys.modules
    assert executor.deploy(comp)['kind'] == 'deployed'
    assert read(executor,'app') == 'P|P|P|P' and 'pkg' not in sys.modules


def test_drain_imports_use_their_snapshot(executor, tmp_path):
    root = artifact(tmp_path/'source','A')
    marker = tmp_path/'drained'
    (root/'entry.py').write_text(SOURCE.replace('    return d',
        "    def drain():\n        import lazy\n        from pathlib import Path\n        Path(" + repr(str(marker)) + ").write_text(lazy.VALUE)\n    d.parrot_drain = drain\n    return d"))
    assert executor.deploy(component('old',root))['kind'] == 'deployed'
    artifact(root,'B')
    assert executor.drain('/user/old',5000) == (1,0)
    assert marker.read_text() == 'A'


def test_snapshot_root_replacement_cannot_redirect_copy(tmp_path, monkeypatch):
    from pathlib import Path
    from parrot_protocol import ray_artifacts
    source = tmp_path/'source'; source.mkdir(); (source/'data').write_text('original')
    outside = tmp_path/'outside'; outside.mkdir(); (outside/'data').write_text('outside')
    scan = ray_artifacts.os.scandir
    replaced = []
    def replace_at_scan(path):
        if not replaced:
            replaced.append(True)
            source.rename(tmp_path/'original')
            source.symlink_to(outside,target_is_directory=True)
        return scan(path)
    monkeypatch.setattr(ray_artifacts.os,'scandir',replace_at_scan)
    snapshot = ray_artifacts.Snapshot(source)
    try: assert (Path(snapshot.root)/'data').read_text() == 'original'
    finally: snapshot.close()


def test_regular_file_replaced_by_fifo_does_not_block_snapshot(tmp_path):
    import subprocess
    script = '''from pathlib import Path
import os,sys
from parrot_protocol.ray_artifacts import Snapshot
source=Path(sys.argv[1]);source.mkdir();(source/'data').write_text('data')
original=os.open
swapped=[]
def replace(path, flags, *args, **kwargs):
    if Path(path).name == 'data' and not swapped:
        swapped.append(True)
        (source/'data').unlink();os.mkfifo(source/'data')
    return original(path,flags,*args,**kwargs)
os.open=replace
try:
    snapshot=Snapshot(source)
except ValueError:
    assert swapped
else:
    snapshot.close()
    raise AssertionError('FIFO must be rejected')
'''
    result = subprocess.run([sys.executable,'-c',script,str(tmp_path/'source')],
                            capture_output=True,text=True,timeout=3)
    assert result.returncode == 0, result.stderr
