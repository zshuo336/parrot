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
                value = json.loads(match[1]) if key in ('index', 'health') else match.groups()
                if key == 'ray':
                    if match[3] is not None and match[3] != match[1]:
                        raise RunFailure('inconsistent historical local term count')
                    value = match[1], match[2]
                put(key, value)
        match = re.fullmatch(r'\[lab\] search (\[.+\]) → (\[.*\])', line)
        if match:
            query = json.dumps(json.loads(match[1]), ensure_ascii=False)
            put('search ' + query, json.loads(match[2]))
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
        build = self.spawn('build-rust', [os.environ.get('CARGO', 'cargo'), 'build', '--locked', '-p', 'crawler-lab'])
        self.completed(build)
        os.environ['PARROT_LAB_BIN'] = str(ROOT / 'target/debug/crawler-lab')
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
