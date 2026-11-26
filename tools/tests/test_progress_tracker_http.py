"""Live document allowlist changes must work without recreating the HTTP server."""
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import unittest
from urllib.error import HTTPError
from urllib.request import urlopen

SPEC = importlib.util.spec_from_file_location('progress_tracker', Path(__file__).resolve().parents[1] / 'progress_tracker.py')
tracker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(tracker)


class TaskDetails(unittest.TestCase):
    def test_expected_nonzero_controls_and_timeouts_have_distinct_results(self):
        stages = [
            dict(name="historical control", exit_code=1, expected_exit_code=1),
            dict(name="standard control", code=101, expected_code=101),
            dict(name="unexpected success", exit_code=0, expected_exit_code=1),
            dict(name="unexpected failure", exit_code=1),
            dict(name="timed out", exit_code=1, expected_exit_code=1, timed_out=True),
            dict(name="invalid result", passed=False),
            dict(name="checks failed after successful command", passed=False, exit_code=0),
            dict(name="zero tests in expected failure control", passed=0, code=101, expected_code=101),
        ]
        self.assertEqual(tracker.failed_stage_names(stages),
                         ["unexpected success", "unexpected failure", "timed out", "invalid result",
                          "checks failed after successful command"])

    def test_completed_additions_stay_with_their_task_in_source_order(self):
        body = ("状态：进行中\n剩余人日：1-2\n依赖：无\n已做：原始进展\n"
                "待做：复验\n验收：全部通过\n证据：无\n")
        text = ("### T01-01 首项\n" + body + "已做补充：第二批\n已做补充：第三批\n"
                + "### T01-02 次项\n" + body)
        first, second = tracker.parse_tasks(text)
        self.assertEqual(first['implemented'].splitlines(), ['原始进展', '第二批', '第三批'])
        self.assertEqual(second['implemented'], '原始进展')


class LiveDocumentsTest(unittest.TestCase):
    def test_added_changed_and_removed_document_uses_current_allowlist(self):
        with tempfile.TemporaryDirectory(prefix='parrot-tracker-http-') as directory:
            root = Path(directory)
            (root / 'docs/progress').mkdir(parents=True)
            task = '### T00-01 HTTP\n状态：进行中\n剩余人日：1-1\n依赖：无\n已做：启动服务\n待做：验证文档\n验收：实时白名单\n证据：无\n'
            (root / 'docs/TASKS.md').write_text(task, encoding='utf-8')
            (root / 'docs/EXECUTION-PLAN.md').write_text(task, encoding='utf-8')
            config = {
                'schema_version': 1, 'project_name': 'HTTP test', 'scope': 'documents',
                'port': 0, 'repositories': {'main': '.'}, 'phases': {'T00': 'HTTP'},
                'documents': {'tasks': {'name': 'tasks', 'repository': 'main', 'path': 'docs/TASKS.md'}},
            }
            config_path = root / 'docs/progress/config.json'
            def save_config():
                config_path.write_text(json.dumps(config), encoding='utf-8')
            save_config()
            (root / 'docs/progress/scope.json').write_text(json.dumps({
                'schema_version': 1, 'revision': 'http-1', 'active_task_ids': ['T00-01'], 'removed_tasks': [],
            }), encoding='utf-8')
            (root / 'docs/progress/current.json').write_text(json.dumps({
                'schema_version': 1, 'updated': '2026-11-21', 'scope_change': 'HTTP test',
                'headline': 'HTTP test', 'current_task_id': 'T00-01', 'current_detail': 'HTTP test',
                'boundary': 'HTTP only', 'queue': ['T00-01'], 'delivery_groups': [], 'batches': [], 'limits': [],
            }), encoding='utf-8')
            (root / 'docs/late.md').write_text('first document', encoding='utf-8')
            (root / 'docs/replacement.md').write_text('replacement document', encoding='utf-8')
            with tracker.make_server(root, port=0) as server:
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                url = 'http://127.0.0.1:' + str(server.server_port) + '/documents/late'
                try:
                    self.assert_not_found(url)
                    config['documents']['late'] = {'name': 'late', 'repository': 'main', 'path': 'docs/late.md'}
                    save_config()
                    with urlopen(url, timeout=5) as response:
                        self.assertEqual(response.read(), b'first document')
                    config['documents']['late']['path'] = 'docs/replacement.md'
                    save_config()
                    with urlopen(url, timeout=5) as response:
                        self.assertEqual(response.read(), b'replacement document')
                    del config['documents']['late']
                    save_config()
                    self.assert_not_found(url)
                finally:
                    server.shutdown()
                    thread.join(timeout=5)
                    self.assertFalse(thread.is_alive())

    def assert_not_found(self, url):
        with self.assertRaises(HTTPError) as caught:
            urlopen(url, timeout=5)
        self.assertEqual(caught.exception.code, 404)
        caught.exception.close()


if __name__ == '__main__':
    unittest.main()
