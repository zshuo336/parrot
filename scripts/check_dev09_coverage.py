#!/usr/bin/env python3
"""Check DEV_09's unchanged per-crate 100% line / 95% branch thresholds.

This checks report completeness and arithmetic, not the authenticity of a run.
Archive the source manifest, toolchain, command, tests and raw profiles alongside
any report. Reports from failed tests or invalid profiles are not acceptance data.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sys

CRATES = ('parrot-app', 'parrot-abi', 'parrot-wasm')


def counts(summary, metric):
    value = summary.get(metric)
    if not isinstance(value, dict):
        raise ValueError('missing ' + metric + ' counters')
    count, covered = value.get('count'), value.get('covered')
    if (type(count) is not int or type(covered) is not int
            or not 0 <= covered <= count):
        raise ValueError('invalid ' + metric + ' counters')
    # Calculate from integer counters. Rounded or supplied percentages cannot
    # turn 94.999% into 95%, or a zero denominator into a measured success.
    return count, covered


def evaluate(report, root, scope=None):
    root = Path(root).resolve()
    if not isinstance(report, dict) or report.get('type') != 'llvm.coverage.json.export':
        raise ValueError('expected an LLVM coverage JSON export')
    data = report.get('data')
    if not isinstance(data, list) or not data:
        raise ValueError('empty LLVM coverage data')
    # These are source classifications, not uncovered-code waivers. Exact
    # content bindings prevent a later executable body or removed cfg(test)
    # from silently inheriting the exclusion. Unknown files remain required.
    if scope is None:
        scope = json.loads(Path(__file__).with_name('dev09_coverage_scope.json').read_text())
    excluded = {}
    for name, classification in scope.items():
        if (not isinstance(classification, dict)
                or not isinstance(classification.get('inputs'), dict)
                or name not in classification['inputs']):
            raise ValueError('source classification must bind its own file: ' + name)
        if (root / name).is_file() and all(
            (root / source).is_file()
            and hashlib.sha256((root / source).read_bytes()).hexdigest() == digest
            for source, digest in classification['inputs'].items()
        ):
            excluded[name] = classification['reason']
    required = {}
    for crate in CRATES:
        source = root / 'crates' / crate / 'src'
        files = {p.relative_to(root).as_posix() for p in source.rglob('*.rs') if p.is_file()}
        if not files:
            raise ValueError('no source files for ' + crate)
        required[crate] = files - excluded.keys()
    expected = set().union(*required.values())
    observed = {}
    for item in data:
        if not isinstance(item, dict) or not isinstance(item.get('files'), list):
            raise ValueError('missing LLVM file records')
        for record in item['files']:
            if not isinstance(record, dict) or not isinstance(record.get('filename'), str):
                raise ValueError('invalid LLVM file record')
            path = Path(record['filename'])
            if not path.is_absolute():
                path = root / path
            try:
                relative = path.resolve().relative_to(root).as_posix()
            except ValueError:
                continue  # Dependencies outside the supplied source root.
            if relative not in expected:
                continue
            if relative in observed:
                raise ValueError('duplicate source record: ' + relative)
            summary = record.get('summary')
            if not isinstance(summary, dict):
                raise ValueError('missing summary: ' + relative)
            observed[relative] = {metric: counts(summary, metric) for metric in ('lines', 'branches')}
    results = {}
    for crate, files in required.items():
        metrics = {}
        for metric in ('lines', 'branches'):
            count = sum(observed[p][metric][0] for p in files & observed.keys())
            covered = sum(observed[p][metric][1] for p in files & observed.keys())
            threshold = 100 if metric == 'lines' else 95
            metrics[metric] = {
                'count': count, 'covered': covered,
                'percent': 100 * covered / count if count else None,
                'required_percent': threshold,
                'met': count > 0 and 100 * covered >= threshold * count,
            }
        missing = sorted(files - observed.keys())
        results[crate] = {
            'source_files': len(files), 'reported_files': len(files & observed.keys()),
            'missing_files': missing, **metrics,
            'met': not missing and all(m['met'] for m in metrics.values()),
        }
    return {
        'crates': results, 'met': all(r['met'] for r in results.values()),
        'classified_nonproduction_or_declarative': excluded,
        'scope': 'Reported Rust source in the three DEV_09 crates; no waivers applied. '
                 'Missing files and zero denominators are not passes. A single platform/feature '
                 'report may be incomplete. Run provenance and test success must be verified separately.',
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args(argv)
    try:
        result = evaluate(json.loads(args.report.read_text()), args.root)
    except (OSError, ValueError, TypeError) as error:
        print('coverage report rejected: ' + str(error), file=sys.stderr)
        return 2
    print(json.dumps(result, indent=2, ensure_ascii=False))
    return 0 if result['met'] else 1


if __name__ == '__main__':
    sys.exit(main())
