#!/usr/bin/env python3
"""Actual embedded/native/Wasm business, historical oracle and owned runtimes.

Requires make build-crawler-business build-crawler-components build-ts.
No skipped form, mock gateway or changed historical golden counts as passing.
"""
import argparse
import json
from pathlib import Path
import sys
import tempfile
import time

from run_launcher_integration import ROOT, Run, require
sys.path.insert(0, str(ROOT / 'scripts'))
from crawler_business_artifacts import verify as verify_business_artifacts
sys.path.insert(0, str(ROOT / 'apps/crawler-lab/deploy'))
from regression import compare, GOLDEN


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    root = args.output or Path(tempfile.mkdtemp(prefix='crawler-business-', dir='/tmp'))
    if args.output:
        root.mkdir(parents=True, exist_ok=False)
    print('CRAWLER_BUSINESS_ARTIFACTS=' + str(root), flush=True)
    artifacts = verify_business_artifacts()
    (root / 'artifacts.json').write_text(json.dumps(artifacts, indent=2) + '\n')
    results, failures = {}, []
    for mode, form in [('direct', 'embedded'), ('direct', 'dylib'), ('direct', 'wasm'),
                       ('registry', 'dylib'), ('registry', 'wasm')]:
        name = mode + '-' + form
        options = [] if form == 'embedded' else ['--hub-' + form, artifacts[form]['path'],
                    '--trust-native-sha256' if form == 'dylib' else '--hub-sha256', artifacts[form]['sha256']]
        run = None
        try:
            run = Run(root / name, mode=mode, pages=60, hub_options=options, application=artifacts['application']['path'])
            while not run.sample():
                time.sleep(.025)
            record = run.finish()
            text = run.stdout.read_text()
            compare(GOLDEN.read_text(), text)
            require('[lab] hub business: form=' + form + ' ' in text, 'requested form did not execute')
            terminal = json.loads((run.directory / 'result.json').read_text())
            require(all(child['retired'] for child in terminal['children']), 'missing guardian retirement')
            require(terminal['ts'] == ('passed' if mode == 'direct' else 'unavailable-in-registry'), 'wrong TS execution status')
            results[name] = record
            print('CRAWLER_COMPONENT_BUSINESS_PASS ' + name, flush=True)
        except Exception as error:
            failures.append(dict(name=name, error=str(error)))
            print('CRAWLER_COMPONENT_BUSINESS_FAIL ' + name + ': ' + str(error), flush=True)
        finally:
            if run is not None:
                if run.process.poll() is None:
                    run.process.kill()
                    run.process.wait(timeout=15)
                    # EOF guardians retire descendants even after harness failure.
                    try:
                        run.finish()
                    except Exception as error:
                        failures.append(dict(name=name + '-retirement', error=str(error)))
                run.stream.close()
    (root / 'result.json').write_text(json.dumps(dict(passed=results, failed=failures), indent=2) + '\n')
    require(not failures and len(results) == 5, 'component business matrix failed: ' + str(root))


if __name__ == '__main__':
    main()
