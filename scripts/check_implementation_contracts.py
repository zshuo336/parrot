#!/usr/bin/env python3
"""Check implementation-contract accounting, never implementation acceptance.

Python 3.9+, standard library only. Document hashes require a human review when
the contract changes. References locate work; their existence proves no runtime
semantics, test execution, coverage, performance or project completion.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import sys


SCHEMA = 'parrot-implementation-contracts-v1'
CATALOG = Path(__file__).with_name('implementation_contracts.json')
TASK_FIELDS = {'anchor', 'owners', 'implementation', 'tests', 'acceptance', 'remaining'}


def reject_duplicates(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate JSON key: ' + key)
        result[key] = value
    return result


def load(path):
    return json.loads(Path(path).read_text(encoding='utf-8'),
                      object_pairs_hook=reject_duplicates)


def file_in(root, name):
    if not isinstance(name, str) or not name or '\\' in name:
        raise ValueError('invalid reference: ' + repr(name))
    path = PurePosixPath(name)
    if (path.is_absolute() or path.as_posix() != name
            or any(p in {'.', '..', '.git'} for p in path.parts)):
        raise ValueError('noncanonical reference: ' + name)
    current = root
    for part in path.parts:
        current = current / part
        if current.is_symlink():
            raise ValueError('symlink reference: ' + name)
    if not current.is_file():
        raise ValueError('missing referenced file: ' + name)
    return current


def strings(value, label):
    if (not isinstance(value, list) or not value
            or any(not isinstance(v, str) or not v.strip() for v in value)
            or len(set(value)) != len(value)):
        raise ValueError('expected nonempty unique strings: ' + label)
    return value


def declared_tasks(namespace, source):
    # This is a deliberately narrow check of these documents' task declaration
    # grammar, not a Markdown parser or a semantic requirement extractor.
    if namespace == 'DEV_00':
        pattern = r'^\| (L[0-9]+) '
    elif namespace == 'DEV_09':
        pattern = r'^\| ([A-G][0-9]+) \|'
    else:
        prefix = {'DEV_01': 'T', 'DEV_02': 'K', 'DEV_03': 'E',
                  'DEV_04': 'D', 'DEV_05': 'F', 'DEV_06': 'S'}[namespace]
        pattern = r'^(' + prefix + r'[0-9]+) '
        inside = False
        declarations = []
        for line in source.splitlines():
            if line.startswith('```'):
                inside = not inside
            elif inside:
                declarations.append(line)
        source = '\n'.join(declarations)
    tasks = re.findall(pattern, source, re.MULTILINE)
    # DEV_00 has two intentional L2 rows: integration and language vectors.
    if not tasks or (namespace != 'DEV_00' and len(tasks) != len(set(tasks))):
        raise ValueError('missing or duplicate task declarations: ' + namespace)
    return set(tasks)


def evaluate(catalog, root):
    root = Path(root).resolve()
    if (not isinstance(catalog, dict) or set(catalog) != {'schema', 'documents', 'contracts'}
            or catalog.get('schema') != SCHEMA):
        raise ValueError('invalid catalog schema')
    documents, contracts = catalog['documents'], catalog['contracts']
    if not isinstance(documents, dict) or not documents or not isinstance(contracts, dict):
        raise ValueError('missing documents or contracts')
    actual = {p.relative_to(root).as_posix()
              for p in (root / 'docs').glob('DEV_[0-9][0-9]_*.md')}
    registered = []
    sources = {}
    expected = set()
    for namespace, doc in documents.items():
        if (not re.fullmatch(r'DEV_(?:0[0-6]|09)', namespace)
                or not isinstance(doc, dict)
                or set(doc) != {'path', 'sha256', 'supplemental'}):
            raise ValueError('invalid document record: ' + namespace)
        name = doc['path']
        if not isinstance(name, str) or not name.startswith('docs/' + namespace + '_'):
            raise ValueError('document namespace mismatch: ' + namespace)
        raw = file_in(root, name).read_bytes()
        if hashlib.sha256(raw).hexdigest() != doc['sha256']:
            raise ValueError('document changed; review clauses before rebinding: ' + name)
        sources[namespace] = raw.decode('utf-8')
        registered.append(name)
        tasks = declared_tasks(namespace, sources[namespace])
        supplemental = set(strings(doc['supplemental'], namespace + ' supplemental'))
        if tasks & supplemental or any(not re.fullmatch(r'[A-Z][A-Z_]+', s) for s in supplemental):
            raise ValueError('invalid supplemental contract IDs: ' + namespace)
        expected.update(namespace + '/' + task for task in tasks | supplemental)
    if len(registered) != len(set(registered)) or set(registered) != actual:
        raise ValueError('document selection mismatch; missing=' + repr(sorted(actual - set(registered)))
                         + ', extra=' + repr(sorted(set(registered) - actual)))
    if set(contracts) != expected:
        raise ValueError('contract set mismatch; missing=' + repr(sorted(expected - set(contracts)))
                         + ', extra=' + repr(sorted(set(contracts) - expected)))
    by_owner = {}
    for name, task in contracts.items():
        namespace = name.split('/')[0]
        if not isinstance(task, dict) or set(task) != TASK_FIELDS:
            raise ValueError('invalid contract fields (no acceptance status allowed): ' + name)
        anchor = task['anchor']
        if (not isinstance(anchor, str) or not anchor.strip()
                or sources[namespace].count(anchor) != 1):
            raise ValueError('anchor must identify exactly one source location: ' + name)
        for owner in strings(task['owners'], name + ' owners'):
            if not re.fullmatch(r'K(?:0[1-9]|1[0-9]|2[0-8])', owner):
                raise ValueError('unknown required work package: ' + owner)
            by_owner.setdefault(owner, []).append(name)
        for kind in ('implementation', 'tests'):
            for reference in strings(task[kind], name + ' ' + kind):
                file_in(root, reference)
        for kind in ('acceptance', 'remaining'):
            strings(task[kind], name + ' ' + kind)
    return {
        'accounting_valid': True,
        'documents': len(documents), 'contracts': len(contracts),
        'by_owner': {k: sorted(v) for k, v in sorted(by_owner.items())},
        'scope': 'Declared DEV tasks plus explicitly registered cross-cutting clauses only. '
                 'No implementation acceptance or completeness of all public promises is certified.',
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--catalog', type=Path, default=CATALOG)
    args = parser.parse_args(argv)
    try:
        result = evaluate(load(args.catalog), args.root)
    except (OSError, UnicodeError, ValueError, TypeError) as error:
        print('implementation accounting rejected: ' + str(error), file=sys.stderr)
        return 2
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


if __name__ == '__main__':
    sys.exit(main())
