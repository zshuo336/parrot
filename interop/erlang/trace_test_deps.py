"""Build fixed, checksum-verified OpenTelemetry dependencies for the explicit test gate."""
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request

PACKAGES = [
    ('opentelemetry_api', '1.5.0', 'f53ec8a1337ae4a487d43ac89da4bd3a3c99ddf576655d071deed8b56a2d5dda'),
    ('opentelemetry', '1.7.0', 'a9173b058c4549bf824cbc2f1d2fa2adc5cdedc22aa3f0f826951187bbd53131'),
]


def require(condition, message):
    if not condition:
        raise ValueError(message)


def main():
    root = Path(__file__).resolve().parent / 'target/trace-deps'
    root.mkdir(parents=True, exist_ok=True)
    manifests = {}
    for name, version, digest in PACKAGES:
        package = root / (name + '-' + version + '.tar')
        if not package.exists():
            with urllib.request.urlopen('https://repo.hex.pm/tarballs/' + package.name, timeout=30) as response:
                data = response.read(2 * 1024 * 1024 + 1)
            require(len(data) <= 2 * 1024 * 1024 and hashlib.sha256(data).hexdigest() == digest,
                    'dependency download checksum differs')
            with package.open('xb') as output:
                output.write(data)
        require(hashlib.sha256(package.read_bytes()).hexdigest() == digest, 'cached package checksum differs')
        destination = root / name
        with tempfile.TemporaryDirectory(prefix=name + '-', dir=root) as temp:
            staging = Path(temp)
            with tarfile.open(package) as outer:
                require(set(outer.getnames()) == {'VERSION', 'CHECKSUM', 'metadata.config', 'contents.tar.gz'},
                        'unexpected Hex package format')
                content = outer.extractfile('contents.tar.gz').read()
            seen = set()
            with tarfile.open(fileobj=io.BytesIO(content), mode='r:gz') as archive:
                for member in archive:
                    path = Path(member.name)
                    require(not path.is_absolute() and '..' not in path.parts and member.name not in seen,
                            'unsafe or duplicate dependency member')
                    seen.add(member.name)
                    if member.isdir():
                        continue
                    require(member.isfile() and member.size <= 1024 * 1024, 'nonregular or oversized dependency member')
                    target = staging / path
                    target.parent.mkdir(parents=True, exist_ok=True)
                    with target.open('xb') as output:
                        output.write(archive.extractfile(member).read())
            ebin = staging / 'ebin'
            ebin.mkdir(exist_ok=True)
            env = dict(os.environ, ERL_LIBS=str(root))
            sources = sorted((staging / 'src').glob('*.erl'))
            require(bool(sources), 'missing actual Erlang dependency sources')
            subprocess.run(['erlc', '-I', str(staging / 'include'), '-o', str(ebin),
                            *map(str, sources)], env=env, check=True, timeout=60)
            subprocess.run(['erl', '-noshell', '-eval',
                '{ok,[App]}=file:consult(hd(init:get_plain_arguments())), '
                'ok=file:write_file(lists:nth(2,init:get_plain_arguments()),io_lib:format("~tp.~n",[App])),halt().',
                '-extra', str(staging / 'src' / (name + '.app.src')), str(ebin / (name + '.app'))],
                env=env, check=True, timeout=30)
            if destination.exists():
                shutil.rmtree(destination)
            shutil.move(str(staging), str(destination))
        manifests[name] = dict(version=version, package_sha256=digest,
            beams={path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted((destination / 'ebin').glob('*.beam'))})
    (root / 'built.json').write_text(json.dumps(manifests, indent=2))
    print(root)


if __name__ == '__main__':
    main()
