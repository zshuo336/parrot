"""Portable bundles, verified installation, literal SSH data and startup failures."""
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
DEPLOY = ROOT/'apps/websearch/deploy'


def module(name):
    spec = importlib.util.spec_from_file_location('distribution_'+name, DEPLOY/(name+'.py'))
    result = importlib.util.module_from_spec(spec); spec.loader.exec_module(result)
    return result


DIST, INSTALL = module('distribute'), module('install_bundle')


def checkout(root):
    def put(name, value='fixture', executable=False):
        path=root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text(value)
        if executable: path.chmod(0o700)
        return path
    for name in ['distribute.py','install_bundle.py','distribute.sh','start-remote.sh','ray_gateway.py']:
        path=root/'apps/websearch/deploy'/name;path.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(DEPLOY/name,path)
    put('apps/websearch/build.sh', '#!/bin/sh\nexit "${WS_TEST_BUILD_EXIT:-0}"\n', True)
    for name in ['jvm/target/websearch-jvm-1.0.0.jar','erlang/frontier.erl','python/tokenizer.py']:
        put('apps/websearch/'+name)
    put('interop/jvm/target/parrot-protocol-jvm-0.1.0.jar')
    # Same basename must not collapse distinct dependencies, including last line
    # without a newline (Maven commonly produces this format).
    a=put('cache/a/same.jar','first');b=put('cache/b/same.jar','second')
    put('interop/jvm/target/cp.txt',str(a)+':'+str(b))
    for name in ['parrot_gw','parrot_trace','parrot_component_runtime','parrot_beam_snapshot','parrot_beam_guest','parrot_beam_host']:
        put('interop/erlang/'+name+'.erl')
    put('interop/python/parrot_protocol/__init__.py')
    put('interop/python/parrot_protocol/nested/helper.py')
    put('interop/python/parrot_protocol/__pycache__/junk.pyc')


class DistributionContracts(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory(prefix='ws-distribution-');self.addCleanup(self.tmp.cleanup)
        self.root=Path(self.tmp.name); self.repo=self.root/'repo';checkout(self.repo)

    def packed(self):
        path=self.root/'bundle.tar.gz';sha,count=DIST.bundle(self.repo,path)
        return path,sha,count

    def test_bundle_is_portable_and_preserves_dependency_order_and_collisions(self):
        archive,sha,count=self.packed()
        shutil.rmtree(self.repo/'cache')
        with archive.open('rb') as stream: result=INSTALL.receive(self.root/'installed',sha,stream)
        release=Path(result['root']);entries=(release/'gw/cp.txt').read_text().strip().split(':')
        self.assertEqual([(release/'gw'/entry).read_text() for entry in entries],['first','second'])
        self.assertEqual(result['files'],count)
        self.assertTrue((release/'gw_py/parrot_protocol/nested/helper.py').is_file())
        self.assertFalse((release/'gw_py/parrot_protocol/__pycache__').exists())
        self.assertFalse((release/'erlang/frontier.beam').exists())
        self.assertTrue(os.access(release/'start-remote.sh',os.X_OK))

    def test_missing_any_dependency_fails_before_bundle_is_published(self):
        (self.repo/'cache/b/same.jar').unlink()
        with self.assertRaisesRegex(ValueError,'release input'):self.packed()
        self.assertFalse((self.root/'bundle.tar.gz').exists())

    def test_integrity_failure_preserves_destination_and_releases_own_lock(self):
        archive,sha,_=self.packed();target=self.root/'release'
        with archive.open('rb') as stream:
            with self.assertRaisesRegex(ValueError,'digest'):INSTALL.receive(target,'0'*64,stream)
        self.assertFalse(target.exists());self.assertFalse(target.with_name('release.install-lock').exists())
        target.mkdir();(target/'user-data').write_text('preserve')
        with archive.open('rb') as stream:
            with self.assertRaisesRegex(ValueError,'already exists'):INSTALL.receive(target,sha,stream)
        self.assertEqual((target/'user-data').read_text(),'preserve')

    def test_foreign_lock_and_symlink_destination_are_preserved(self):
        archive,sha,_=self.packed();target=self.root/'release';lock=self.root/'release.install-lock'
        lock.write_text('other installer')
        with archive.open('rb') as stream:
            with self.assertRaises(FileExistsError): INSTALL.receive(target,sha,stream)
        self.assertEqual(lock.read_text(),'other installer');lock.unlink()
        target.symlink_to(self.root/'absent')
        with archive.open('rb') as stream:
            with self.assertRaisesRegex(ValueError,'already exists'):INSTALL.receive(target,sha,stream)
        self.assertTrue(target.is_symlink())

    def test_archive_missing_duplicate_link_traversal_and_wrong_hash_are_rejected(self):
        for case in ['missing','duplicate','symlink','traversal','hash','extra']:
            with self.subTest(case=case):
                data=io.BytesIO();name='../escape' if case=='traversal' else 'safe'
                manifest={name:hashlib.sha256(b'body').hexdigest()}
                if case=='hash':manifest[name]='0'*64
                with tarfile.open(fileobj=data,mode='w:gz') as tar:
                    body=json.dumps(manifest).encode();info=tarfile.TarInfo('MANIFEST.json');info.size=len(body);tar.addfile(info,io.BytesIO(body))
                    if case!='missing':
                        info=tarfile.TarInfo('extra' if case=='extra' else name);info.size=4
                        if case=='symlink':info.type=tarfile.SYMTYPE;info.linkname='../escape';info.size=0
                        tar.addfile(info,io.BytesIO(b'body'))
                        if case=='duplicate':tar.addfile(info,io.BytesIO(b'body'))
                raw=data.getvalue();target=self.root/case
                with self.assertRaises(ValueError):INSTALL.receive(target,hashlib.sha256(raw).hexdigest(),io.BytesIO(raw))
                self.assertFalse(target.exists())
        self.assertFalse((self.root/'escape').exists())

    def ssh_environment(self):
        bin=self.root/'bin';bin.mkdir(exist_ok=True)
        (bin/'make').write_text('#!/bin/sh\nexit 0\n')
        # The real remote receiver executes through a local shell, with the exact
        # SSH command string. No network connection or scp rewriting is involved.
        (bin/'ssh').write_text('#!'+sys.executable+'\nimport os,subprocess,sys\nraise SystemExit(subprocess.call(["sh","-c",sys.argv[-1]],cwd=os.environ["WS_TEST_SSH_CWD"]))\n')
        for p in bin.iterdir():p.chmod(0o700)
        return dict(os.environ,PATH=str(bin)+':'+os.environ['PATH'],WS_TEST_SSH_CWD=str(self.root))

    def command(self,target):
        return ['bash',str(self.repo/'apps/websearch/deploy/distribute.sh'),'fixture-host','--root',str(target)]

    def test_actual_distribution_keeps_shell_metacharacters_literal(self):
        target=self.root/"release ' ; touch SENTINEL # 中文"
        result=subprocess.run(self.command(target),env=self.ssh_environment(),capture_output=True,text=True,timeout=15)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertTrue((target/'MANIFEST.json').exists());self.assertFalse((self.root/'SENTINEL').exists())
        self.assertFalse(list(self.root.glob('*.install-lock')))

    def test_failed_build_never_installs_and_concurrent_install_never_overwrites(self):
        env=self.ssh_environment();target=self.root/'release'
        result=subprocess.run(self.command(target),env=dict(env,WS_TEST_BUILD_EXIT='17'),capture_output=True,text=True,timeout=15)
        self.assertNotEqual(result.returncode,0);self.assertFalse(target.exists())
        a=subprocess.Popen(self.command(target),env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
        b=subprocess.Popen(self.command(target),env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
        try:
            ao=a.communicate(timeout=15)[0];bo=b.communicate(timeout=15)[0]
            self.assertEqual(sorted([a.returncode,b.returncode]),[0,1],ao+bo)
            self.assertTrue((target/'MANIFEST.json').exists())
        finally:
            for process in [a,b]:
                if process.poll() is None:process.kill()
                process.wait(timeout=5)

    def test_remote_erl_compile_failure_cannot_start_stale_gateway(self):
        bin=self.root/'bin';bin.mkdir()
        (bin/'erlc').write_text('#!/bin/sh\ncase "$*" in *frontier.erl*) exit 17;; *) exit 0;; esac\n')
        (bin/'erl').write_text('#!/bin/sh\ntouch "$WS_TEST_STARTED"\n')
        for p in bin.iterdir():p.chmod(0o700)
        marker=self.root/'started'
        result=subprocess.run(['bash',str(DEPLOY/'start-remote.sh'),'erl','127.0.0.1:1','--root',str(self.root)],env=dict(os.environ,PATH=str(bin)+':'+os.environ['PATH'],WS_TEST_STARTED=str(marker)),capture_output=True,text=True,timeout=10)
        self.assertEqual(result.returncode,17,result.stderr);self.assertFalse(marker.exists())

    def test_remote_argument_errors_precede_storage_and_commands(self):
        for args in [['jvm','host:0'],['jvm','http://host:10'],['jvm','host:1','--data'],['jvm','host:1','--root','relative']]:
            result=subprocess.run(['bash',str(DEPLOY/'start-remote.sh'),*args],capture_output=True,text=True,timeout=10)
            self.assertNotEqual(result.returncode,0)

    def test_registration_target_is_never_interpolated_into_erlang_code(self):
        bin=self.root/'bin';bin.mkdir()
        (bin/'erlc').write_text('#!/bin/sh\nexit 0\n')
        (bin/'erl').write_text('#!'+sys.executable+'\nimport json,os,sys\nopen(os.environ["WS_TEST_CAPTURE"],"w").write(json.dumps([sys.argv,os.environ["WS_APP_TARGET"]]))\n')
        for p in bin.iterdir():p.chmod(0o700)
        capture=self.root/'capture.json';target='host",halt():1234'
        result=subprocess.run(['bash',str(DEPLOY/'start-remote.sh'),'erl',target,'--root',str(self.root)],
                              env=dict(os.environ,PATH=str(bin)+':'+os.environ['PATH'],WS_TEST_CAPTURE=str(capture)),capture_output=True,text=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        argv,value=json.loads(capture.read_text())
        self.assertEqual(value,target)
        expression=argv[argv.index('-eval')+1]
        self.assertNotIn(target,expression)
        self.assertEqual(expression,'parrot_gw:main([0, "parrot=" ++ os:getenv("WS_APP_TARGET")]).')

    def test_archive_and_expansion_limits_fail_without_publishing(self):
        from unittest.mock import patch
        archive,sha,_=self.packed()
        for field in ['MAX_ARCHIVE','MAX_CONTENT']:
            target=self.root/field
            with patch.object(INSTALL,field,1),archive.open('rb') as stream:
                with self.assertRaisesRegex(ValueError,'size limit'):INSTALL.receive(target,sha,stream)
            self.assertFalse(target.exists())

    def test_invalid_jvm_classpath_does_not_create_data(self):
        gw=self.root/'gw';gw.mkdir();(gw/'cp.txt').write_text('/absent/build/cache.jar')
        data=self.root/'data'
        result=subprocess.run(['bash',str(DEPLOY/'start-remote.sh'),'jvm','127.0.0.1:1','--root',str(self.root),'--data',str(data)],capture_output=True,text=True,timeout=10)
        self.assertNotEqual(result.returncode,0);self.assertFalse(data.exists())


if __name__=='__main__':unittest.main()
