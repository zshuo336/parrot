#!/usr/bin/env python3
"""Build and run every Cargo example in the parrot package."""
# SPDX-License-Identifier: AGPL-3.0-only
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--release', action='store_true')
    parser.add_argument('--legacy', action='store_true', help='use the legacy message envelope')
    parser.add_argument('--output', type=Path, help='new directory for logs and the result')
    args = parser.parse_args()
    output = args.output.resolve() if args.output else Path(tempfile.mkdtemp(prefix='parrot-examples-'))
    if args.output:
        output.mkdir(parents=True, exist_ok=False)
    print('Example reports:', output, flush=True)
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version=1'], cwd=ROOT))
    package, = [p for p in metadata['packages'] if p['name'] == 'parrot']
    targets = {t['name']: t['src_path'] for t in package['targets'] if 'example' in t['kind']}
    cmd = ['cargo', 'build', '--locked', '-p', 'parrot', '--examples', '--message-format=json-render-diagnostics']
    if args.release:
        cmd.append('--release')
    if args.legacy:
        cmd += ['--features', 'parrot_envelope_legacy']
    build = output / 'build.jsonl'
    with build.open('w') as log:
        subprocess.run(cmd, cwd=ROOT, stdout=log, check=True)
    programs = {}
    with build.open() as log:
        for line in log:
            row = json.loads(line)
            if row['reason'] == 'compiler-artifact' and row['package_id'] == package['id'] and 'example' in row['target']['kind']:
                name = row['target']['name']
                if (name in programs or row['target']['src_path'] != targets[name]
                        or row['profile']['test'] or not row['executable']):
                    raise ValueError(f'unexpected example artifact: {name}')
                programs[name] = Path(row['executable'])
    if not targets or targets.keys() != programs.keys():
        raise ValueError(f'Cargo examples differ from executable inventory: {targets.keys() ^ programs.keys()}')
    cases = {}
    for name, binary in sorted(programs.items()):
        log = output / (name + '.log')
        with log.open('w') as sink:
            run = subprocess.run([sys.executable, ROOT / 'scripts/with_timeout.py', '30', binary],
                                 cwd=ROOT, stdout=sink, stderr=subprocess.STDOUT)
        cases[name] = dict(exit_code=run.returncode, executable=str(binary), source=targets[name],
                           sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                           log_sha256=hashlib.sha256(log.read_bytes()).hexdigest())
        print(f'{name}: {run.returncode}', flush=True)
        if run.returncode:
            print(log.read_text(errors='replace')[-4000:], file=sys.stderr)
    passed = all(c['exit_code'] == 0 for c in cases.values())
    report = dict(passed=passed, build=cmd, cases=cases)
    (output / 'verification.json').write_text(json.dumps(report, indent=2) + '\n')
    return 0 if passed else 1


if __name__ == '__main__':
    raise SystemExit(main())
