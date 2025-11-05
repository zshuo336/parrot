#!/usr/bin/env python3
"""Build a relocatable native websearch host and its owned gateway resources."""
from pathlib import Path
import argparse
import hashlib
import gzip
import io
import json
import os
import platform
import subprocess
import sys
import tarfile
import tempfile

from distribute import bundle, ROOT, APP
from install_bundle import receive, MAX_ARCHIVE, MAX_CONTENT, MAX_FILES


def sha(path):
    digest=hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda:source.read(1024*1024),b''):digest.update(chunk)
    return digest.hexdigest()


def bundle_host(repository, binary, destination, profile='release'):
    repository, binary, destination = Path(repository), Path(binary), Path(destination)
    if destination.exists() or destination.is_symlink():
        raise ValueError('release archive already exists; choose a new path')
    if not binary.is_file() or binary.is_symlink() or not os.access(binary, os.X_OK):
        raise ValueError('host binary is missing, linked or not executable')
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.ws-host-package-', dir=destination.parent) as temporary:
        work = Path(temporary)
        gateways = work/'gateways.tar.gz'
        digest, _ = bundle(repository, gateways)
        with gateways.open('rb') as source:
            receive(work/'payload', digest, source)
        payload = work/'payload'
        original = json.loads((payload/'MANIFEST.json').read_text())
        files = {name: payload/name for name in original}
        def add(path, name):
            if not path.is_file() or path.is_symlink() or name in files:
                raise ValueError('missing, linked or duplicate host release input: '+str(path))
            files[name] = path
        add(binary, 'bin/websearch')
        add(repository/'apps/websearch/websearch.app.toml', 'websearch.app.toml')
        for name in ['gateway_worker.py', 'ray_gateway.py']:
            add(repository/'tools/parrot-obs'/name, 'runtime/tools/parrot-obs/'+name)
        for name in original:
            if name.startswith('gw_erl/'):
                add(payload/name, 'runtime/interop/erlang/'+name[len('gw_erl/'):])
            elif name.startswith('gw_py/'):
                add(payload/name, 'runtime/interop/python/'+name[len('gw_py/'):])
            elif name.startswith('gw/'):
                add(payload/name, 'runtime/interop/jvm/target/'+name[len('gw/'):])
        metadata = work/'RELEASE.json'
        metadata.write_text(json.dumps(dict(format=1, application='websearch', profile=profile,
            built_on=dict(system=platform.system(), machine=platform.machine()),
            binary_sha256=sha(binary),
            entrypoint='bin/websearch', manifest='websearch.app.toml', runtime='runtime'), indent=2)+'\n')
        add(metadata, 'RELEASE.json')
        guide = work/'HOST_README.txt'
        guide.write_text('''Native host release. Run on a system compatible with bin/websearch.
Requires Python 3 with ray and jieba, a compatible Java runtime, and Erlang/erlc.
These system runtimes are prerequisites, not bundled or installed automatically.

Install using install_bundle.py and the archive SHA256. Keep the installed
release read-only; Erlang gateway compilation uses private temporary files.
Start with an absolute external data directory, for example:
  bin/websearch --insecure-development --manifest websearch.app.toml --data /srv/websearch-data <seed>
Use --serve-only with the same data directory for replay, or --keep to resume.
The default manifest data path is relative to the manifest; override it when
using a read-only release. Runtime discovery follows the installed executable;
--runtime-root can explicitly choose another complete local runtime tree.
Java dependencies are relative to runtime/interop/jvm/target. No build checkout
or Maven cache is needed at runtime. Remote gateway entrypoints are also kept.
Insecure development mode is explicit; this package is not a TLS deployment.
''')
        add(guide, 'HOST_README.txt')
        # Include the verified receiver so an archive can be installed without a
        # checkout. Obtain and verify its SHA via the trusted release channel.
        add(repository/'apps/websearch/deploy/install_bundle.py', 'install_bundle.py')
        if len(files)>MAX_FILES or sum(path.stat().st_size for path in files.values())>MAX_CONTENT:
            raise ValueError('host release exceeds receiver content budget')
        manifest = {name: sha(path) for name,path in sorted(files.items())}
        archive = work/'host.tar.gz'
        with archive.open('wb') as raw_output, gzip.GzipFile(fileobj=raw_output, mode='wb', filename='', mtime=0) as compressed, tarfile.open(fileobj=compressed, mode='w') as tar:
            raw = (json.dumps(manifest,sort_keys=True)+'\n').encode()
            if len(raw)>1024*1024:raise ValueError('host manifest exceeds receiver budget')
            info = tarfile.TarInfo('MANIFEST.json'); info.size=len(raw);info.mode=0o600
            tar.addfile(info,io.BytesIO(raw))
            for name,path in sorted(files.items()):
                info = tarfile.TarInfo(name);info.size=path.stat().st_size
                info.mode=0o700 if os.access(path,os.X_OK) else 0o600
                with path.open('rb') as source:tar.addfile(info,source)
        if archive.stat().st_size>MAX_ARCHIVE:raise ValueError('host archive exceeds receiver byte budget')
        with archive.open('rb') as source:os.fsync(source.fileno())
        # Exclusive atomic publication: a concurrent publisher cannot be replaced.
        os.link(archive,destination)
    return sha(destination),len(manifest)


def build_binary(repository, profile):
    env = dict(os.environ)
    env.setdefault('CARGO_BUILD_JOBS','2')
    command = ['cargo','build','--locked','-p','websearch','--profile',profile,'--message-format=json-render-diagnostics']
    process = subprocess.Popen(command,cwd=repository,env=env,stdout=subprocess.PIPE,text=True)
    artifacts = set()
    try:
        for line in process.stdout:
            message = json.loads(line)
            if message.get('reason') == 'compiler-message':
                rendered = message.get('message',{}).get('rendered')
                if rendered:print(rendered,end='',flush=True)
            if (message.get('reason') == 'compiler-artifact' and message.get('target',{}).get('name') == 'websearch'
                    and 'bin' in message['target'].get('kind',[]) and message.get('executable')):
                artifacts.add(message['executable'])
        code = process.wait()
        if code:raise subprocess.CalledProcessError(code,command)
    finally:
        process.stdout.close()
        if process.poll() is None:process.terminate();process.wait()
    if len(artifacts)!=1:raise ValueError('Cargo did not report exactly one websearch executable')
    return Path(artifacts.pop())


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--profile',choices=['dev','release'],default='release')
    args=parser.parse_args()
    try:
        if args.output.exists() or args.output.is_symlink():raise ValueError('release archive already exists')
        binary=build_binary(ROOT,args.profile)
        subprocess.run(['make','build-jvm'],cwd=ROOT,check=True)
        subprocess.run(['bash',str(APP/'jvm/build.sh')],cwd=ROOT,check=True)
        digest,count=bundle_host(ROOT,binary,args.output,args.profile)
        print(json.dumps(dict(archive=str(args.output.resolve()),sha256=digest,files=count,binary=str(binary))),flush=True)
        return 0
    except (OSError,ValueError,subprocess.SubprocessError) as error:
        print('websearch host package: '+str(error),file=sys.stderr)
        return 1


if __name__=='__main__':raise SystemExit(main())
