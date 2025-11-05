"""Actual SDK, TCP, serial worker and isolated BEAM causal-context contracts."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'apps/test_support'))
from owned_process import OwnedProcess


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--rust', action='store_true', help='also run the production Rust sender in both profiles')
    args = parser.parse_args()
    erlang = ROOT / 'interop/erlang'
    target = erlang / 'target'
    target.mkdir(exist_ok=True)
    explicit = os.environ.get('PARROT_ERLANG_CAUSAL_OUTPUT')
    if explicit:
        out = Path(explicit).resolve()
        out.mkdir(parents=True)
    else:
        out = Path(tempfile.mkdtemp(prefix='causal-run-', dir=target))
    rows = []
    print('CAUSAL_EVIDENCE=' + str(out), flush=True)

    def stage(name, command, env=None, marker=None, timeout=180):
        log = out / (name + '.log')
        started = time.monotonic()
        stage_env = dict(os.environ if env is None else env, ERL_CRASH_DUMP=str(out / (name + '-crash.dump')))
        with log.open('xb') as stream:
            with OwnedProcess(command, env=stage_env, cwd=ROOT, stdout=stream, stderr=subprocess.STDOUT) as process:
                code = process.wait(timeout=timeout)
            if not process.retired:
                raise ValueError('owned command not retired')
        row = dict(name=name, command=command, exit=code, elapsed=time.monotonic()-started,
                   log_sha256=sha(log), owned_group_retired=True)
        rows.append(row)
        (out / (name + '.json')).write_text(json.dumps(row, indent=2))
        print(json.dumps(row), flush=True)
        if code != 0 or (marker and marker not in log.read_text()):
            raise ValueError('causal contract stage failed: ' + name)

    files = sorted(erlang.glob('parrot_*.erl')) + sorted((erlang / 'tests/causal').glob('*.erl'))
    source = out / 'source'
    source.mkdir()
    for path in files:
        shutil.copy2(path, source / path.name)
    controllers = [Path(__file__).resolve(), erlang / 'trace_test_deps.py', ROOT / 'apps/test_support/owned_process.py']
    if args.rust:
        controllers += [ROOT / 'crates/parrot-remote/tests/erlang_causal_context.rs', ROOT / 'Cargo.lock']
    for path in controllers:
        shutil.copy2(path, source / path.name)
    sources = {str(path.relative_to(ROOT)): sha(path) for path in [*files, *controllers]}
    (out / 'source-inputs.json').write_text(json.dumps(sources, indent=2))
    stage('sdk-dependencies', [sys.executable, '-B', str(erlang / 'trace_test_deps.py')])
    sdk = target / 'trace-deps'
    shutil.copy2(sdk / 'built.json', out / 'sdk-built.json')
    env = dict(os.environ, ERL_LIBS=str(sdk))
    classes = out / 'classes'
    classes.mkdir()
    stage('strict-compile', ['erlc', '-Werror', '-o', str(classes), *map(str, files)], env)
    artifact = out / 'artifact'
    artifact.mkdir()
    for name in ['causal_fixture.beam', 'causal_rust_fixture.beam']:
        shutil.copy2(classes / name, artifact / name)
    for package in ['opentelemetry_api', 'opentelemetry']:
        shutil.copytree(sdk / package / 'ebin', artifact / package / 'ebin')
    disabled = dict(os.environ, ERL_LIBS='')
    stage('without-sdk', ['erl', '-noshell', '-pa', str(classes), '-eval',
          'ok=test_causal:run_disabled(),halt().'], disabled, 'CAUSAL-DISABLED-PASS')
    env['PARROT_CAUSAL_ARTIFACT'] = str(artifact)
    stage('actual-sdk-and-guests', ['erl', '-noshell', '-pa', str(classes), '-eval',
          'ok=test_causal:run(),halt().'], env, 'CAUSAL-ALL-PASS tests=10')
    if args.rust:
        env.update(PARROT_ERLANG_CAUSAL_CLASSES=str(classes), PARROT_ERLANG_CAUSAL_EVIDENCE=str(out / 'rust-evidence'),
                   CARGO_BUILD_JOBS='2')
        for profile in ['debug', 'release']:
            command = ['cargo', 'test', '--locked', '-p', 'parrot-remote', '--test', 'erlang_causal_context']
            if profile == 'release':
                command.append('--release')
            stage('rust-erlang-' + profile, command + ['--', '--ignored', '--exact',
                  'rust_sender_observes_actual_erlang_handlers_and_guest_retirement', '--nocapture'], env, timeout=900)
        stage('rust-clippy', ['cargo', 'clippy', '--locked', '-p', 'parrot-remote', '--test',
              'erlang_causal_context', '--', '-D', 'warnings'], env, timeout=900)
    if any(sha(ROOT / name) != digest for name, digest in sources.items()):
        raise ValueError('source changed during execution')
    record = dict(status='passed', stages=rows, source_inputs=sources,
                  classfiles={path.name: sha(path) for path in classes.glob('*.beam')},
                  tests=10, rust_profiles=['debug', 'release'] if args.rust else [],
                  disabled_sdk_gate=True, installed=False, formal_acceptance=False)
    with (out / 'verified.json').open('x') as output:
        json.dump(record, output, indent=2)
    print('Erlang causal execution contracts passed', flush=True)


if __name__ == '__main__':
    main()
