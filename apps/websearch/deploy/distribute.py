"""Build and distribute complete websearch gateway releases over SSH stdin."""
from pathlib import Path
import argparse
import hashlib
import io
import json
import os
import re
import shlex
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[3]
APP = ROOT/'apps/websearch'


def bundle(repository, destination):
    repository, destination = Path(repository), Path(destination)
    app = repository/'apps/websearch'
    files = {}
    def add(source, name):
        source = Path(source)
        if not source.is_file() or source.is_symlink():
            raise ValueError('missing or nonregular release input: '+str(source))
        if name in files:
            raise ValueError('duplicate release member: '+name)
        files[name] = source
    for relative in ['jvm/target/websearch-jvm-1.0.0.jar', 'erlang/frontier.erl', 'python/tokenizer.py']:
        add(app/relative, relative)
    for name in ['start-remote.sh']:
        add(app/'deploy'/name, name)
    add(app/'deploy/ray_gateway.py', 'remote-ray.py')
    add(repository/'interop/jvm/target/parrot-protocol-jvm-0.1.0.jar', 'gw/parrot-protocol-jvm-0.1.0.jar')
    for name in ['parrot_gw', 'parrot_trace', 'parrot_component_runtime', 'parrot_beam_snapshot', 'parrot_beam_guest', 'parrot_beam_host']:
        add(repository/'interop/erlang'/(name+'.erl'), 'gw_erl/'+name+'.erl')
    package = repository/'interop/python/parrot_protocol'
    if not (package/'__init__.py').is_file():
        raise ValueError('missing Python protocol package')
    for source in sorted(package.rglob('*.py')):
        add(source, 'gw_py/parrot_protocol/'+source.relative_to(package).as_posix())
    classpath = (repository/'interop/jvm/target/cp.txt').read_text().strip().split(os.pathsep)
    if not classpath or any(not p for p in classpath):
        raise ValueError('empty JVM dependency classpath')
    relative_classpath = []
    for index, path in enumerate(classpath):
        source = Path(path)
        if not source.is_absolute():
            source = repository/'interop/jvm'/source
        name = f'libs/{index:04}.jar'
        add(source, 'gw/'+name)
        relative_classpath.append(name)
    # Numeric names preserve classpath order and never overwrite duplicate jar
    # basenames from distinct dependencies.
    with tempfile.TemporaryDirectory(prefix='ws-bundle-meta-') as tmp:
        cp = Path(tmp)/'cp.txt'; cp.write_text(':'.join(relative_classpath)+'\n')
        add(cp, 'gw/cp.txt')
        manifest = {name:hashlib.sha256(source.read_bytes()).hexdigest() for name,source in sorted(files.items())}
        with tarfile.open(destination, 'w:gz') as tar:
            content = (json.dumps(manifest, sort_keys=True)+'\n').encode()
            info = tarfile.TarInfo('MANIFEST.json'); info.size=len(content); info.mode=0o600
            tar.addfile(info, io.BytesIO(content))
            for name, source in sorted(files.items()):
                tar.add(source, arcname=name, recursive=False)
    return hashlib.sha256(destination.read_bytes()).hexdigest(), len(manifest)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('hosts', help='Comma-separated SSH hosts (or SSH config aliases)')
    parser.add_argument('--root', default='/opt/parrot/websearch', help='New absolute release directory; existing directories are preserved')
    parser.add_argument('--profile', choices=['dev', 'release'], default='release')
    args = parser.parse_args(argv)
    hosts = args.hosts.split(',')
    if any(not re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_.@:-]*', host) or host.startswith('-') for host in hosts):
        parser.error('invalid SSH host or alias')
    root = Path(args.root)
    if not root.is_absolute() or root == Path('/') or '..' in root.parts or '\x00' in args.root:
        parser.error('--root must be an absolute non-root path without ..')
    try:
        timeout = int(os.environ.get('WS_DISTRIBUTE_TIMEOUT', '300'))
        if not 1 <= timeout <= 86400: raise ValueError('WS_DISTRIBUTE_TIMEOUT must be 1..86400 seconds')
        subprocess.run(['bash', str(APP/'build.sh'), '--profile', args.profile], cwd=ROOT, check=True)
        subprocess.run(['make', 'build-jvm'], cwd=ROOT, check=True)
        receiver = Path(__file__).with_name('install_bundle.py').read_text()
        with tempfile.TemporaryDirectory(prefix='ws-distribute-') as tmp:
            archive = Path(tmp)/'release.tar.gz'
            sha, count = bundle(ROOT, archive)
            print(f'Release: {count} files, SHA256={sha}', flush=True)
            for host in hosts:
                command = shlex.join(['python3', '-c', receiver, str(root), sha])
                with archive.open('rb') as stream:
                    subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', f'ConnectTimeout={min(timeout,30)}', '--', host, command],
                                   stdin=stream, check=True, timeout=timeout)
                print(f'Installed {host}:{root}', flush=True)
        return 0
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print('websearch distribution: '+str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
