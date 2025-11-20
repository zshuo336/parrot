#!/usr/bin/env python3
"""Rebuild and compare one owned fixed workload to the historical golden output."""
import argparse
import json
import os
from pathlib import Path
import re
import sys

from launcher import Launcher, ROOT, RunFailure, arguments

GOLDEN = ROOT / 'apps/crawler-lab/golden/pre_migration_output.txt'


def strict_json(text):
    def fields(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError('duplicate JSON field: ' + key)
            result[key] = value
        return result

    def constant(value):
        raise ValueError('non-JSON numeric constant: ' + value)

    try:
        return json.loads(text, object_pairs_hook=fields, parse_constant=constant)
    except ValueError as error:
        raise RunFailure('invalid regression JSON: ' + str(error)) from error


def built_application(output):
    candidates = []
    source = ROOT / 'apps/crawler-lab/src/main.rs'
    for line in output.splitlines():
        # Cargo diagnostics and build-script output may share the guardian log.
        if not line.startswith('{'):
            continue
        event = strict_json(line)
        if not isinstance(event, dict) or event.get('reason') != 'compiler-artifact':
            continue
        target = event.get('target', {})
        if (target.get('name') == 'crawler-lab' and target.get('kind') == ['bin']
                and target.get('src_path') == str(source)
                and event.get('profile', {}).get('test') is False):
            executable = event.get('executable')
            if isinstance(executable, str) and Path(executable).is_absolute():
                candidates.append(Path(executable))
    if len(candidates) != 1 or not candidates[0].is_file() or not os.access(candidates[0], os.X_OK):
        raise RunFailure('Cargo did not identify exactly one runnable crawler-lab application')
    return candidates[0]


def observations(text):
    patterns = {
        'crawl': r'\[lab\] 爬取完成：fetched=(\d+)（去重后）pushed_total=(\d+) 耗时 .+',
        'ray': r'\[lab\] ray 索引: terms=(\d+) postings=(\d+)(?:（本地对账 unique_terms=(\d+)）)?',
        'index': r'\[lab\] jvm 倒排: (.+)',
        'health': r'\[lab\] jvm healthz: (.+)',
        'done': r'\[lab\s+\d+s\] done\s+pushed=\s*(\d+) fetched=\s*(\d+) ray_pages=\s*(\d+) jvm_terms=\s*(\d+)',
        'pass': r'crawler-lab PASS（四运行时全链集成）',
    }
    values = {}

    def put(key, value):
        if key in values:
            raise RunFailure('duplicate regression observation: ' + key)
        values[key] = value

    for line in text.splitlines():
        for key, pattern in patterns.items():
            match = re.fullmatch(pattern, line)
            if match:
                # Preserve JSON token types and reject malformed JSON. The
                # historical key order is not a semantic part of the contract.
                value = strict_json(match[1]) if key in ('index', 'health') else match.groups()
                if key == 'ray':
                    if match[3] is not None and match[3] != match[1]:
                        raise RunFailure('inconsistent historical local term count')
                    value = match[1], match[2]
                put(key, value)
        match = re.fullmatch(r'\[lab\] search (\[.+\]) → (\[.*\])', line)
        if match:
            query = json.dumps(strict_json(match[1]), ensure_ascii=False)
            put('search ' + query, strict_json(match[2]))
    if set(values) != set(patterns) | {
        'search ["parrot"]', 'search ["actor", "cluster"]',
        'search ["crawler", "index", "search"]', 'search ["raft", "swim"]',
    }:
        raise RunFailure('incomplete or unexpected regression observations: ' + ', '.join(values))
    return values


def compare(expected, actual):
    before, after = observations(expected), observations(actual)
    # JSON's bool/int equality in Python must not hide a numeric type drift.
    if json.dumps(before, sort_keys=True) != json.dumps(after, sort_keys=True):
        changed = [key for key in before
                   if json.dumps(before[key], sort_keys=True) != json.dumps(after[key], sort_keys=True)]
        raise RunFailure('regression observations changed: ' + ', '.join(changed))


class Regression(Launcher):
    def execute(self):
        build_components = self.spawn('build-components', ['make', 'build-crawler-components'])
        self.completed(build_components)
        build = self.spawn('build-rust', [os.environ.get('CARGO', 'cargo'), 'build', '--locked',
                                        '-p', 'crawler-lab', '--bin', 'crawler-lab', '--message-format=json'])
        self.completed(build)
        os.environ['PARROT_LAB_BIN'] = str(built_application(build.text()))
        super().execute()
        app = next(child for child in self.children if child.name == 'app')
        compare(GOLDEN.read_text(), app.text())


def main():
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument('--keep-output', action='store_true', help='compatibility option; all runs retain private logs')
    parser.parse_args()
    if os.environ.get('LAB_MODE', 'direct') != 'direct':
        parser.error('fixed golden regression requires LAB_MODE=direct')
    os.environ['LAB_MODE'] = 'direct'
    try:
        runner = Regression(arguments(['--pages', '60', '--depth', '2', '--fanout', '3', '--batch', '32', '--skip-ts']))
    except RunFailure as error:
        print(str(error), file=sys.stderr)
        return error.code
    import signal
    signal.signal(signal.SIGTERM, runner.signal)
    signal.signal(signal.SIGINT, runner.signal)
    code = runner.run()
    if code == 0:
        print('crawler-lab 回归 PASS（固定工作负载计数与查询结果匹配；不证明 manifest 迁移或 cid 轨迹）', flush=True)
    return code


if __name__ == '__main__':
    raise SystemExit(main())
