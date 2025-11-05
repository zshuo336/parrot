#!/usr/bin/env python3
"""Run the actual C++ gate with ASan/UBSan and retain its build evidence."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import uuid

FLAGS = ['-O1', '-g', '-Werror', '-fsanitize=address,undefined',
         '-fno-sanitize-recover=all', '-fno-omit-frame-pointer']
BINARIES = {'test_vectors', 'test_interop', 'transport', 'resolver', 'completion',
            'pollset', 'pollset-forced-poll', 'address', 'decoder-limits', 'heartbeat', 'terminal-error'}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def save(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, indent=2)


def compile_forward():
    out = Path(tempfile.mkdtemp(prefix='compile-', dir=os.environ['PARROT_CPP_SANITIZER_BUILDS']))
    command = [os.environ['PARROT_CPP_SANITIZER_COMPILER'], *sys.argv[1:], *FLAGS]
    save(out / 'command.json', dict(command=command, cwd=os.getcwd()))
    with (out / 'compiler.log').open('xb') as stream:
        result = subprocess.run(command, stdout=stream, stderr=subprocess.STDOUT)
    with (out / 'compiler.log').open('rb') as stream:
        shutil.copyfileobj(stream, sys.stdout.buffer)
    artifact = None
    if result.returncode == 0:
        require(sys.argv.count('-o') == 1, 'expected one compiler output')
        binary = Path(sys.argv[sys.argv.index('-o') + 1]).resolve(strict=True)
        shutil.copy2(binary, out / 'executable')
        symbols = binary.with_name(binary.name + '.dSYM')
        if symbols.is_dir():
            shutil.copytree(symbols, out / symbols.name)
        artifact = dict(name=binary.name, sha256=sha(binary))
    save(out / 'result.json', dict(exit=result.returncode, artifact=artifact,
        log_sha256=sha(out / 'compiler.log')))
    return result.returncode


def validate_control(log, code, kind):
    marker = {'heap': 'AddressSanitizer: heap-use-after-free',
              'integer': 'runtime error: signed integer overflow'}.get(kind)
    require(kind in {'clean', 'heap', 'integer'}, 'unknown sanitizer control')
    if marker is None:
        require(code == 0 and 'AddressSanitizer' not in log and 'runtime error:' not in log,
                'clean sanitizer control failed')
    else:
        require(code == 1 and marker in log, 'designated sanitizer fault was not detected: ' + kind)


def suite_expectations(repo):
    expected = {}
    for name in ['transport', 'interop_cli']:
        path = repo / 'interop/cpp-lite' / ('test_' + name + '.py')
        spec = importlib.util.spec_from_file_location('sanitizer_discovery_' + name, path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        count = unittest.TestLoader().loadTestsFromModule(module).countTestCases()
        require(count > 0, 'empty C++ test discovery: ' + name)
        expected[name] = count
    vectors = json.loads((repo / 'docs/vectors/wire1.json').read_text())['vectors']
    require(isinstance(vectors, list) and len(vectors) > 0, 'empty vector corpus')
    expected['vectors'] = len(vectors)
    boundaries = (repo / 'docs/vectors/wire1-boundaries.tsv').read_text().splitlines()
    expected['boundaries'] = sum(bool(line) and not line.startswith('#') for line in boundaries)
    require(expected['boundaries'] > 0, 'empty boundary corpus')
    return expected


def validate_suite(log, artifacts, expected):
    receipts = [json.loads(line.removeprefix('PARROT_CPP_TEST_RECEIPT '))
                for line in log.splitlines() if line.startswith('PARROT_CPP_TEST_RECEIPT ')]
    require(len(receipts) == 1, 'expected one transport suite receipt')
    row = receipts[0]
    counters = ['selected', 'executed', 'failures', 'errors', 'skipped',
                'expected_failures', 'unexpected_successes']
    require(type(row['schema']) is int and row['schema'] == 1 and
            all(type(row[name]) is int for name in counters) and row['selected'] == expected['transport'] and
            row['executed'] == row['selected'] and row['successful'] is True and
            all(row[name] == 0 for name in ['failures', 'errors', 'skipped',
                'expected_failures', 'unexpected_successes']), 'transport suite incomplete')
    require(re.findall(r'^CPP_FIXTURE_RESULT tests=(\d+) failures=(\d+)$', log, re.M) == [('13', '0')] and
            re.findall(r'^CPP-LITE VECTORS PASS \((\d+) vectors\)$', log, re.M) == [str(expected['vectors'])] and
            re.findall(r'^CPP shared wire boundaries: (\d+) cases$', log, re.M) == [str(expected['boundaries'])] and
            len(re.findall(r'^CABI-SMOKE PASS$', log, re.M)) == 1 and
            re.findall(r'^Ran (\d+) tests? in ', log, re.M) ==
            [str(expected['interop_cli']), str(expected['transport'])],
            'vector or CLI suite did not complete')
    require(len(artifacts) == len(BINARIES) and {row['name'] for row in artifacts} == BINARIES,
            'C++ sanitizer build set incomplete')
    return row


def source_inputs(repo, fixtures):
    paths = [repo / 'Makefile']
    for directory in ['interop/cpp-lite', 'interop/tests', 'docs/vectors']:
        paths.extend(path for path in (repo / directory).rglob('*')
                     if path.is_file() and '__pycache__' not in path.parts)
    return dict(repository={path.relative_to(repo).as_posix(): sha(path) for path in sorted(paths)},
                controls={path.name: sha(path) for path in sorted(fixtures.glob('cpp_sanitizer_*.cpp'))})


def run_gate(repo, output, compiler):
    output.mkdir(parents=True, exist_ok=False)
    print('C++ sanitizer evidence: ' + str(output), flush=True)
    fixtures = Path(__file__).resolve().parent / 'fixtures'
    initial = source_inputs(repo, fixtures)
    require(len(initial['controls']) == 3, 'sanitizer control sources missing')
    save(output / 'source-inputs.json', initial)
    shutil.copy2(Path(__file__), output / 'check_cpp_sanitizers.py')
    save(output / 'compiler-input.json', dict(path=str(compiler), sha256=sha(compiler),
        gate_sha256=sha(Path(__file__)), flags=FLAGS))
    for name in initial['controls']:
        shutil.copy2(fixtures / name, output / name)
    bins = output / 'compiler'; bins.mkdir()
    builds = output / 'native-builds'; builds.mkdir()
    wrapper = bins / 'clang++'
    shutil.copy2(Path(__file__), wrapper); wrapper.chmod(0o755)
    env = dict(os.environ, PATH=str(bins) + os.pathsep + os.environ['PATH'], CXX=str(wrapper),
        PARROT_CPP_SANITIZER_COMPILER=str(compiler), PARROT_CPP_SANITIZER_BUILDS=str(builds),
        PARROT_REPO=str(repo), PYTHONDONTWRITEBYTECODE='1',
        ASAN_OPTIONS='detect_stack_use_after_return=1:abort_on_error=0:exitcode=1',
        UBSAN_OPTIONS='halt_on_error=1:exitcode=1')
    rows = []

    def run(label, command, timeout=120):
        started = time.monotonic()
        timed_out = False
        with (output / (label + '.log')).open('xb') as stream:
            child = subprocess.Popen(command, cwd=repo, env=env, stdout=stream,
                stderr=subprocess.STDOUT, start_new_session=True)
            try:
                code = child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                os.killpg(child.pid, signal.SIGKILL)
                code = child.wait()
            except BaseException:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
                raise
        row = dict(label=label, command=command, exit=code, timed_out=timed_out,
            seconds=time.monotonic() - started, log_sha256=sha(output / (label + '.log')))
        rows.append(row); save(output / (label + '.json'), row)
        require(not timed_out, 'sanitizer gate deadline: ' + label)
        return code, (output / (label + '.log')).read_text(errors='replace')

    try:
        for kind in ['clean', 'heap', 'integer']:
            source = output / ('cpp_sanitizer_' + kind + '.cpp')
            binary = output / ('control-' + kind)
            code, _ = run('compile-' + kind,
                [str(compiler), '-std=c++17', *FLAGS, str(source), '-o', str(binary)])
            require(code == 0, 'sanitizer control did not compile: ' + kind)
            code, log = run('control-' + kind, [str(binary)], timeout=30)
            validate_control(log, code, kind)
        expected = suite_expectations(repo)
        save(output / 'expected-suites.json', expected)
        code, log = run('cpp-suite', ['make', '--no-print-directory', '-j1',
            'CPP_BUILD_DIR=' + str(output / 'cpp-lite'), 'check-cpp'], timeout=900)
        require(code == 0, 'actual C++ sanitizer suite failed; inspect cpp-suite.log')
        artifacts = []
        for path in sorted(builds.glob('*/result.json')):
            compiled = json.loads(path.read_text())
            require(compiled['exit'] == 0 and compiled['artifact'] is not None, 'failed C++ build')
            require(sha(path.parent / 'executable') == compiled['artifact']['sha256'], 'saved binary differs')
            artifacts.append(compiled['artifact'])
        receipt = validate_suite(log, artifacts, expected)
        require(source_inputs(repo, fixtures) == initial, 'C++ gate changed source inputs')
    except BaseException as error:
        save(output / 'result.json', dict(status='failed', error=str(error), runs=rows))
        raise
    save(output / 'result.json', dict(status='passed', actual_runtime_faults_detected=2,
        transport=receipt, expected_suites=expected, artifacts=artifacts, runs=rows, source_unchanged=True))
    print('C++ ASan/UBSan gate passed; two designated runtime faults detected', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--output', type=Path, default=os.environ.get('PARROT_CPP_SANITIZER_OUTPUT'))
    parser.add_argument('--compiler', default='clang++')
    args = parser.parse_args()
    compiler = shutil.which(args.compiler)
    require(compiler is not None, 'C++ sanitizer compiler not found: ' + args.compiler)
    output = args.output or Path(tempfile.gettempdir()) / ('parrot-cpp-sanitizers-' + uuid.uuid4().hex)
    # The clang++ alias selects C++ linking even when it points to clang.
    run_gate(args.repo.resolve(strict=True), output.resolve(), Path(compiler).absolute())


if __name__ == '__main__':
    sys.exit(compile_forward() if Path(__file__).name == 'clang++' else main())
