#!/usr/bin/env python3
"""Validate frozen core regression logs, fresh build identity and terminal state.

Counts certify this core matrix only, not workspace or complete project coverage.
"""
import argparse
import csv
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import tempfile

STAGES = (
    'manifest', 'input-before', 'rustc', 'execution-input-before', 'build-target',
    'debug-default', 'debug-legacy', 'release-default', 'release-legacy',
    'api-library', 'doctest', 'clippy', 'fmt', 'execution-input-after', 'input-after',
)
CONFIGURATIONS = ('debug-default', 'debug-legacy', 'release-default', 'release-legacy')
BINARIES = {
    'library': 1523,
    'test_actix_root_creation_capacity': 2,
    'test_thread_actor_configuration': 23,
    'test_native_instance_inputs': 22,
    'test_thread_initialization_trace': 3,
    'test_root_cohort': 37,
    'test_root_cohort_host_exit': 38,
    'test_runtime_root_cohort': 20,
    'test_retirement_receipts': 7,
    'test_shutdown_deadline': 5,
    'test_individual_stop_deadline': 14,
    'test_thread_supervision_host_exit': 32,
    'test_factory_retirement_contract': 12,
    'test_reply_helper_ownership': 34,
    'test_factory_admission_contract': 3,
    'test_child_factory_contract': 19,
    'test_child_factory_inputs': 7,
    'test_pending_factory_handoff': 62,
    'test_typed_child_factory': 64,
    'test_typed_factory': 19,
    'test_typed_factory_creation_identity': 12,
    'test_native_factory_inputs': 6,
    'test_actix_root_factory_contract': 10,
    'test_actix_child_factory_contract': 26,
    'test_actix_typed_factory': 19,
    'test_actix_factory_creation_identity': 8,
    'test_local_factory': 23,
    'test_local_factory_children': 12,
    'test_actix_local_typed': 24,
    'test_actix_typed_children': 26,
    'test_actix_typed_lifecycle': 19,
    'test_m4_typed_actix': 5,
    'test_derived_local_runtime': 6,
    'test_neutral_local': 14,
    'test_neutral_local_children': 13,
    'test_neutral_local_creation_identity': 10,
    'test_local_children': 14,
    'test_local_stream_init_identity': 16,
    'test_local_stream_retirement': 4,
}
RUNNING = re.compile(r'^\s+Running (unittests src/lib\.rs|tests/([^ ]+)\.rs) \(([^)]+)\)$')
RESULT = re.compile(r'^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;')
FULL_RESULT = re.compile(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;')
ANNOUNCED = re.compile(r'^running (\d+) tests?$')


def binary_results(path, target, profile):
    results = {}
    current = None
    announced = None
    for line in path.read_text().splitlines():
        running = RUNNING.match(line)
        if line.lstrip().startswith('Running ') and running is None:
            raise ValueError('invalid Cargo test binary announcement: ' + line)
        if running:
            if current is not None:
                raise ValueError('missing result for test binary: ' + current)
            current = running[2] or 'library'
            executable = PurePosixPath(running[3])
            if executable.parent != PurePosixPath(target) / profile / 'deps':
                raise ValueError('test binary is outside the fresh build target: ' + str(executable))
            if current in results:
                raise ValueError('duplicate test binary: ' + current)
            announced = None
        announcement = ANNOUNCED.match(line)
        if current is not None and announced is None and announcement:
            announced = int(announcement[1])
        if line.startswith('test result: '):
            if current is None:
                raise ValueError('unexpected or unsuccessful test result: ' + line)
            result = FULL_RESULT.match(line)
            if result is None:
                # Isolated children can interleave stdout. Only a complete,
                # unfiltered parent result proves the whole binary succeeded.
                continue
            passed, failed, ignored, measured, filtered = map(int, result.groups()[1:])
            if filtered:
                if announced is None or passed + failed + ignored + measured + filtered != announced:
                    raise ValueError('filtered child result does not match active binary: ' + current)
                continue
            if result[1] != 'ok' or measured or announced != passed + failed + ignored:
                raise ValueError('unexpected or unsuccessful test result: ' + line)
            results[current] = {'passed': passed, 'failed': failed, 'ignored': ignored}
            current = None
    if current is not None:
        raise ValueError('missing result for test binary: ' + current)
    return results


def summarize(root, manifest_path):
    manifest_bytes = manifest_path.read_bytes()
    input_count = len(json.loads(manifest_bytes)['files'])
    digest = hashlib.sha256(manifest_bytes).hexdigest()
    errors = []
    manifest_log = root / 'manifest.log'
    if not manifest_log.exists() or manifest_log.read_text().split() != [digest, '/workspace/manifest.json']:
        errors.append('execution manifest identity does not match the candidate')
    expected_target = '/workspace/build-targets/' + digest
    receipt = {}
    target = expected_target
    try:
        receipt = json.loads((root / 'build-target.log').read_text())
        expected = {'status': 'ok', 'files': input_count, 'action': 'build-target',
                    'target_dir': expected_target, 'manifest_sha256': digest, 'fresh': True}
        if receipt != expected or receipt.get('fresh') is not True:
            errors.append('fresh build target receipt does not match the candidate')
        # Validate binary paths against the actual receipt, even when the
        # caller supplies another manifest with the same number of inputs.
        if isinstance(receipt.get('target_dir'), str):
            target = receipt['target_dir']
    except (OSError, ValueError, AttributeError):
        errors.append('missing or invalid fresh build target receipt')
    stages = []
    try:
        with (root / 'stages.tsv').open(newline='') as source:
            for name, code in csv.reader(source, delimiter='\t'):
                stages.append({'name': name, 'exit_code': int(code)})
    except (OSError, ValueError):
        errors.append('missing or invalid stage ledger')
    if [stage['name'] for stage in stages] != list(STAGES):
        errors.append('missing, duplicated or unexpected stage sequence')
    if any(stage['exit_code'] != 0 for stage in stages):
        errors.append('one or more stages failed')
    try:
        state = json.loads((root / 'container-state.json').read_text())
    except (OSError, ValueError):
        state = {}
    if (not isinstance(state, dict) or state.get('Status') != 'exited'
            or state.get('ExitCode') != 0 or state.get('OOMKilled') is not False):
        errors.append('container did not terminate successfully')
    configurations = {}
    for label in CONFIGURATIONS:
        try:
            results = binary_results(root / (label + '.log'), target, label.split('-')[0])
        except (OSError, ValueError) as error:
            errors.append(label + ': ' + str(error))
            results = {}
        configurations[label] = results
        if set(results) != set(BINARIES):
            errors.append(label + ': missing or unexpected test binaries')
        if any(result['failed'] or result['ignored'] or result['passed'] == 0
               for result in results.values()):
            errors.append(label + ': failed, ignored or empty test binary')
        for name, count in BINARIES.items():
            if results.get(name, {}).get('passed') != count:
                errors.append(label + ': unexpected count for ' + name)
    for name in ('input-before', 'execution-input-before', 'execution-input-after', 'input-after'):
        try:
            result = json.loads((root / (name + '.log')).read_text())
            if result != {'status': 'ok', 'files': input_count, 'action': 'verify'}:
                errors.append(name + ': unexpected input verification result')
        except (OSError, ValueError):
            errors.append(name + ': missing or invalid verification result')
    try:
        api = binary_results(root / 'api-library.log', target, 'debug')
    except (OSError, ValueError) as error:
        errors.append('api-library: ' + str(error))
        api = {}
    if api != {'library': {'passed': 178, 'failed': 0, 'ignored': 0}}:
        errors.append('missing or unexpected API library result')
    doctest_path = root / 'doctest.log'
    doctests = [tuple(map(int, match.groups())) if (match := RESULT.match(line)) else None
                for line in doctest_path.read_text().splitlines()
                if line.startswith('test result: ')] if doctest_path.exists() else []
    if doctests != [(24, 0, 0)]:
        errors.append('missing or unexpected doctest result')
    return {
        'passed': not errors, 'input_count': input_count, 'manifest_sha256': digest,
        'build_target': receipt, 'stages': stages, 'container_state': state,
        'configurations': configurations, 'errors': errors,
        'artifact_sha256': {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                            for path in sorted(root.iterdir()) if path.is_file()},
    }


def check_identity(root, manifest_path):
    original = summarize(root, manifest_path)
    if not original['passed']:
        raise ValueError('positive result must pass before testing candidate substitution')
    manifest = json.loads(manifest_path.read_bytes())
    name = 'crates/parrot/src/thread/typed.rs'
    old = manifest['files'][name]['sha256']
    manifest['files'][name]['sha256'] = ('1' if old[0] == '0' else '0') + old[1:]
    candidate = (json.dumps(manifest, indent=2, sort_keys=True) + '\n').encode()
    with tempfile.TemporaryDirectory(prefix='parrot-summary-identity-') as folder:
        changed = Path(folder) / 'manifest.json'
        changed.write_bytes(candidate)
        rejected = summarize(root, changed)
    expected = ['execution manifest identity does not match the candidate',
                'fresh build target receipt does not match the candidate']
    if rejected['passed'] or rejected['errors'] != expected or rejected['input_count'] != original['input_count']:
        raise ValueError('same-count candidate substitution was not rejected specifically: ' + repr(rejected['errors']))
    return {
        'passed': True, 'check': 'reject-same-count-different-source-manifest',
        'input_count': original['input_count'],
        'positive_manifest_sha256': original['manifest_sha256'],
        'substituted_manifest_sha256': hashlib.sha256(candidate).hexdigest(),
        'modified_source': name, 'rejected_errors': rejected['errors'],
    }


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('results', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('manifest', type=Path)
    parser.add_argument('--check-identity', action='store_true')
    args = parser.parse_args()
    result = (check_identity if args.check_identity else summarize)(args.results, args.manifest)
    with args.output.open('x') as destination:
        json.dump(result, destination, indent=2)
        destination.write('\n')
    print(json.dumps({'passed': result['passed'], 'errors': result.get('errors', [])}))
    raise SystemExit(0 if result['passed'] else 1)
