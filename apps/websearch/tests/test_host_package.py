"""Native package layout, actual Cargo receipts and exclusive publication."""
from pathlib import Path
import hashlib, json, os, shutil, subprocess, sys, tempfile, unittest
from unittest.mock import patch
from test_distribution import checkout, INSTALL, DEPLOY
sys.path.insert(0,str(DEPLOY))
import package_host as HOST


class HostPackageContracts(unittest.TestCase):
    def setUp(self):
        self.directory=tempfile.TemporaryDirectory(prefix='ws-host-package-tests-');self.addCleanup(self.directory.cleanup)
        self.root=Path(self.directory.name);self.repo=self.root/'repository';checkout(self.repo)
        for name in ['gateway_worker.py','ray_gateway.py']:
            path=self.repo/'tools/parrot-obs'/name;path.parent.mkdir(parents=True,exist_ok=True)
            path.write_text('gateway fixture\n')
        (self.repo/'apps/websearch/websearch.app.toml').write_text('manifest fixture\n')
        self.binary=self.root/'custom target/native app';self.binary.parent.mkdir()
        self.binary.write_text('#!/bin/sh\nexit 0\n');self.binary.chmod(0o700)

    def package(self,name='host.tar.gz'):
        archive=self.root/name
        digest,count=HOST.bundle_host(self.repo,self.binary,archive,'dev')
        return archive,digest,count

    def test_complete_package_is_reproducible_relocatable_and_keeps_native_receipt(self):
        first,digest,count=self.package();second,again,_=self.package('second.tar.gz')
        self.assertEqual(digest,again)
        with first.open('rb') as source:INSTALL.receive(self.root/'installed',digest,source)
        shutil.rmtree(self.repo);self.binary.unlink()
        (self.root/'installed').rename(self.root/'moved release')
        release=self.root/'moved release'
        manifest=json.loads((release/'MANIFEST.json').read_text())
        self.assertEqual(len(manifest),count)
        for name,sha in manifest.items():self.assertEqual(hashlib.sha256((release/name).read_bytes()).hexdigest(),sha)
        self.assertTrue(os.access(release/'bin/websearch',os.X_OK))
        info=json.loads((release/'RELEASE.json').read_text())
        self.assertEqual(info['binary_sha256'],manifest['bin/websearch'])
        self.assertEqual(info['manifest'],'websearch.app.toml')
        target=release/'runtime/interop/jvm/target'
        self.assertEqual([(target/p).read_text() for p in (target/'cp.txt').read_text().strip().split(':')],['first','second'])
        for name in ['runtime/tools/parrot-obs/gateway_worker.py','runtime/tools/parrot-obs/ray_gateway.py',
                     'runtime/interop/erlang/parrot_gw.erl','runtime/interop/erlang/parrot_trace.erl',
                     'runtime/interop/python/parrot_protocol/__init__.py',
                     'websearch.app.toml','install_bundle.py','HOST_README.txt']:
            self.assertIn(name,manifest)
        self.assertFalse(any(name.endswith('.beam') or '__pycache__' in name for name in manifest))

    def test_existing_archive_and_failed_input_are_never_published_over(self):
        path=self.root/'host.tar.gz';path.write_bytes(b'preserve')
        with self.assertRaisesRegex(ValueError,'already exists'):self.package()
        self.assertEqual(path.read_bytes(),b'preserve');path.unlink()
        (self.repo/'tools/parrot-obs/gateway_worker.py').unlink()
        with self.assertRaisesRegex(ValueError,'release input'):self.package()
        self.assertFalse(path.exists());self.assertFalse(list(self.root.glob('.ws-host-package-*')))

    def test_receiver_budget_failure_leaves_no_successful_archive(self):
        with patch.object(HOST,'MAX_CONTENT',1):
            with self.assertRaisesRegex(ValueError,'receiver content budget'):self.package()
        self.assertFalse((self.root/'host.tar.gz').exists())

    def test_cargo_receipt_respects_custom_target_and_failure_does_not_reuse_output(self):
        shim=self.root/'tools';shim.mkdir();cargo=shim/'cargo'
        message=dict(reason='compiler-artifact',target=dict(name='websearch',kind=['bin']),executable=str(self.binary))
        cargo.write_text('#!'+sys.executable+'\nimport os\nprint('+repr(json.dumps(message))+')\nraise SystemExit(int(os.getenv("WS_CARGO_TEST_EXIT","0")))\n');cargo.chmod(0o700)
        with patch.dict(os.environ,dict(PATH=str(shim)+':'+os.environ['PATH'])):
            self.assertEqual(HOST.build_binary(self.repo,'dev'),self.binary)
            with patch.dict(os.environ,dict(WS_CARGO_TEST_EXIT='17')):
                with self.assertRaises(subprocess.CalledProcessError):HOST.build_binary(self.repo,'dev')
        self.assertFalse((self.root/'host.tar.gz').exists())


if __name__=='__main__':unittest.main()
