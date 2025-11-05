import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'apps/crawler-lab/deploy'))
from regression import compare, GOLDEN, RunFailure


class GoldenContracts(unittest.TestCase):
    def setUp(self):
        self.golden = GOLDEN.read_text()

    def test_elapsed_time_and_json_key_order_are_not_semantics(self):
        compare(self.golden, self.golden.replace('11.910917ms', '1s').replace(
            '{"terms":33,"postings":1746,"queries":4}', '{"queries":4, "postings":1746, "terms":33}'))

    def test_current_output_omits_redundant_historical_local_count(self):
        compare(self.golden, self.golden.replace('（本地对账 unique_terms=33）', ''))
        with self.assertRaises(RunFailure):
            compare(self.golden, self.golden.replace('unique_terms=33', 'unique_terms=32'))

    def test_count_prefix_is_not_equal_to_the_actual_count(self):
        with self.assertRaises(RunFailure):
            compare(self.golden, self.golden.replace('ray 索引: terms=33 postings=1746',
                                                   'ray 索引: terms=33 postings=17460'))

    def test_search_order_and_scores_must_match(self):
        with self.assertRaises(RunFailure):
            compare(self.golden, self.golden.replace('"doc":9,"score":7', '"doc":9,"score":8'))

    def test_duplicate_pass_or_stats_are_rejected(self):
        for line in ['crawler-lab PASS（四运行时全链集成）', '[lab] jvm healthz: {"terms":33,"postings":1746,"queries":4}']:
            with self.assertRaises(RunFailure):
                compare(self.golden, self.golden + '\n' + line + '\n')

    def test_missing_pass_and_numeric_type_drift_are_rejected(self):
        for changed in [self.golden.replace('crawler-lab PASS（四运行时全链集成）', ''),
                        self.golden.replace('"queries":4', '"queries":4.0')]:
            with self.assertRaises(RunFailure):
                compare(self.golden, changed)

    def test_invalid_cli_is_rejected_before_build_or_run(self):
        result = subprocess.run([sys.executable, str(ROOT / 'apps/crawler-lab/deploy/regression.py'), '--typo'],
                                text=True, capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn('LAB_RUN_DIR=', result.stdout)


if __name__ == '__main__':
    unittest.main()
