import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / 'check_implementation_contracts.py'
spec = importlib.util.spec_from_file_location('implementation_contracts', SCRIPT)
contracts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contracts)


class ImplementationAccountingContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'docs').mkdir()
        (self.root / 'source.rs').write_text('pub fn entry() {}\n', encoding='utf-8')
        (self.root / 'test.rs').write_text('#[test] fn regression() {}\n', encoding='utf-8')
        self.catalog = {'schema': contracts.SCHEMA, 'documents': {}, 'contracts': {}}
        self.add_document('DEV_04', '```\nD1 sharding\nD2 singleton\n```\n## Exit\n',
                          ['D1', 'D2'])
        self.add_document('DEV_09', '| D1 | ABI |\n| A1 | Manifest |\n## Exit\n',
                          ['D1', 'A1'])

    def add_document(self, namespace, text, tasks):
        path = 'docs/' + namespace + '_contract.md'
        (self.root / path).write_text(text, encoding='utf-8')
        self.catalog['documents'][namespace] = {
            'path': path, 'sha256': hashlib.sha256(text.encode('utf-8')).hexdigest(),
            'supplemental': ['EXIT'],
        }
        for task in tasks + ['EXIT']:
            anchor = ('## Exit' if task == 'EXIT' else
                      '| ' + task + ' |' if namespace == 'DEV_09' else task + ' ')
            self.catalog['contracts'][namespace + '/' + task] = {
                'anchor': anchor, 'owners': ['K16'],
                'implementation': ['source.rs'], 'tests': ['test.rs'],
                'acceptance': ['Real process fault and independently observed effects.'],
                'remaining': ['No runtime acceptance in this fixture.'],
            }

    def document(self, namespace='DEV_04'):
        return self.root / self.catalog['documents'][namespace]['path']

    def rebind_fixture(self, namespace='DEV_04'):
        self.catalog['documents'][namespace]['sha256'] = hashlib.sha256(
            self.document(namespace).read_bytes()).hexdigest()

    def evaluate(self):
        return contracts.evaluate(self.catalog, self.root)

    def test_same_local_id_in_distinct_documents_is_preserved(self):
        result = self.evaluate()
        self.assertTrue(result['accounting_valid'])
        self.assertEqual(result['documents'], 2)
        self.assertEqual(result['contracts'], 6)
        self.assertIn('DEV_04/D1', result['by_owner']['K16'])
        self.assertIn('DEV_09/D1', result['by_owner']['K16'])
        self.assertNotIn('accepted', result)
        self.assertNotIn('complete', result)

    def test_omitted_task_is_rejected_even_when_another_namespace_has_same_id(self):
        del self.catalog['contracts']['DEV_04/D1']
        with self.assertRaisesRegex(ValueError, 'missing=.*DEV_04/D1'):
            self.evaluate()

    def test_new_document_is_not_silently_outside_the_denominator(self):
        (self.root / 'docs/DEV_07_new.md').write_text('New promises\n', encoding='utf-8')
        with self.assertRaisesRegex(ValueError, 'document selection mismatch'):
            self.evaluate()

    def test_removed_document_is_rejected(self):
        self.document().unlink()
        with self.assertRaisesRegex(ValueError, 'missing referenced file'):
            self.evaluate()

    def test_new_task_after_explicit_document_rebinding_still_needs_accounting(self):
        for namespace, declaration in [('DEV_04', 'D12 new task\n'),
                                       ('DEV_09', '| A12 | new task |\n')]:
            with self.subTest(namespace=namespace):
                path = self.document(namespace)
                original = path.read_text(encoding='utf-8')
                text = original.replace('D2 singleton', 'D2 singleton\n' + declaration.strip())
                if namespace == 'DEV_09':
                    text = original + declaration
                path.write_text(text, encoding='utf-8')
                self.rebind_fixture(namespace)
                with self.assertRaisesRegex(ValueError, 'contract set mismatch'):
                    self.evaluate()
                path.write_text(original, encoding='utf-8')
                self.rebind_fixture(namespace)

    def test_changed_requirement_without_rebinding_is_stale_even_if_anchor_survives(self):
        path = self.document()
        path.write_text(path.read_text(encoding='utf-8') + '\nNew deadline: 5 seconds.\n',
                        encoding='utf-8')
        with self.assertRaisesRegex(ValueError, 'review clauses before rebinding'):
            self.evaluate()

    def test_prose_mention_is_not_a_second_task_declaration(self):
        path = self.document()
        path.write_text(path.read_text(encoding='utf-8') + '\nD1 prose mention\n', encoding='utf-8')
        self.rebind_fixture()
        self.catalog['contracts']['DEV_04/D1']['anchor'] = 'D1 sharding'
        self.assertTrue(self.evaluate()['accounting_valid'])

    def test_empty_or_duplicate_task_declarations_are_rejected(self):
        path = self.document()
        for text in ['No declarations\n', '```\nD1 first\nD1 second\n```\n']:
            with self.subTest(text=text):
                path.write_text(text, encoding='utf-8')
                self.rebind_fixture()
                with self.assertRaisesRegex(ValueError, 'missing or duplicate task declarations'):
                    self.evaluate()

    def test_two_l2_rows_remain_one_layer(self):
        self.add_document('DEV_00', '| L2 integration |\n| L2 language vectors |\n## Exit\n', ['L2'])
        self.catalog['contracts']['DEV_00/L2']['anchor'] = '| L2 integration |'
        self.assertTrue(self.evaluate()['accounting_valid'])

    def test_missing_supplemental_gate_and_invented_contract_are_rejected(self):
        original = copy.deepcopy(self.catalog)
        for name, remove in [('DEV_04/EXIT', True), ('DEV_04/D99', False)]:
            with self.subTest(name=name):
                self.catalog = copy.deepcopy(original)
                if remove:
                    del self.catalog['contracts'][name]
                else:
                    self.catalog['contracts'][name] = copy.deepcopy(
                        self.catalog['contracts']['DEV_04/D1'])
                with self.assertRaisesRegex(ValueError, 'contract set mismatch'):
                    self.evaluate()

    def test_missing_or_ambiguous_source_anchor_is_rejected(self):
        task = self.catalog['contracts']['DEV_04/D1']
        for anchor in ['', 'not present', '```']:
            with self.subTest(anchor=anchor):
                task['anchor'] = anchor
                with self.assertRaisesRegex(ValueError, 'exactly one source location'):
                    self.evaluate()

    def test_source_path_cannot_be_bound_to_another_document_namespace(self):
        self.catalog['documents']['DEV_04']['path'] = 'docs/DEV_09_contract.md'
        with self.assertRaisesRegex(ValueError, 'namespace mismatch'):
            self.evaluate()

    def test_missing_implementation_or_test_reference_is_rejected(self):
        for name in ['source.rs', 'test.rs']:
            with self.subTest(name=name):
                path = self.root / name
                original = path.read_bytes()
                path.unlink()
                with self.assertRaisesRegex(ValueError, 'missing referenced file'):
                    self.evaluate()
                path.write_bytes(original)

    def test_reference_must_be_a_canonical_project_file(self):
        task = self.catalog['contracts']['DEV_04/D1']
        for name in ['../source.rs', '/source.rs', './source.rs', 'docs',
                     'docs/../source.rs', 'source\\file.rs', '.git/config']:
            with self.subTest(name=name):
                task['implementation'] = [name]
                with self.assertRaises(ValueError):
                    self.evaluate()

    def test_symlink_file_and_parent_do_not_count_as_project_references(self):
        task = self.catalog['contracts']['DEV_04/D1']
        (self.root / 'link.rs').symlink_to(self.root / 'source.rs')
        (self.root / 'linked-dir').symlink_to(self.root / 'docs', target_is_directory=True)
        for name in ['link.rs', 'linked-dir/DEV_04_contract.md']:
            with self.subTest(name=name):
                task['tests'] = [name]
                with self.assertRaisesRegex(ValueError, 'symlink reference'):
                    self.evaluate()

    def test_blank_missing_duplicate_and_nonstring_obligations_are_rejected(self):
        task = self.catalog['contracts']['DEV_04/D1']
        for field in ['acceptance', 'remaining', 'implementation', 'tests']:
            original = task[field]
            for value in [[], [' '], ['same', 'same'], [None], 'text', [True]]:
                with self.subTest(field=field, value=value):
                    task[field] = value
                    with self.assertRaisesRegex(ValueError, 'nonempty unique strings'):
                        self.evaluate()
            task[field] = original

    def test_unknown_optional_and_duplicate_owners_are_rejected(self):
        task = self.catalog['contracts']['DEV_04/D1']
        for owners in [[], ['K00'], ['K29'], ['X01'], ['K16', 'K16'], ['unassigned']]:
            with self.subTest(owners=owners):
                task['owners'] = owners
                with self.assertRaises(ValueError):
                    self.evaluate()

    def test_catalog_cannot_add_an_implementation_acceptance_flag(self):
        task = self.catalog['contracts']['DEV_04/D1']
        task['status'] = 'accepted'
        with self.assertRaisesRegex(ValueError, 'no acceptance status allowed'):
            self.evaluate()

    def test_duplicate_json_keys_are_rejected_before_data_can_be_overwritten(self):
        path = self.root / 'duplicate.json'
        for content in ['{"schema": 1, "schema": 2}',
                        '{"contracts": {"DEV_04/D1": 1, "DEV_04/D1": 2}}']:
            with self.subTest(content=content):
                path.write_text(content, encoding='utf-8')
                with self.assertRaisesRegex(ValueError, 'duplicate JSON key'):
                    contracts.load(path)

    def test_real_cli_returns_failure_without_a_success_document(self):
        path = self.root / 'catalog.json'
        command = [sys.executable] + (['-O'] if sys.flags.optimize else []) + [
            '-B', str(SCRIPT), '--root', str(self.root), '--catalog', str(path)]
        path.write_text(json.dumps(self.catalog), encoding='utf-8')
        result = subprocess.run(command, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)['accounting_valid'])
        del self.catalog['contracts']['DEV_04/D2']
        for content in [json.dumps(self.catalog), '{malformed', '{"schema":1,"schema":2}']:
            with self.subTest(content=content):
                path.write_text(content, encoding='utf-8')
                result = subprocess.run(command, capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(result.stdout, '')
                self.assertIn('accounting rejected', result.stderr)
                self.assertNotIn('Traceback', result.stderr)


if __name__ == '__main__':
    unittest.main()
