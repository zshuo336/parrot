"""Exercise actual Make recipes and failure propagation on the selected GNU Make.

PARROT_MAKE selects another make executable; PARROT_MAKEFILE selects a frozen
baseline. Tool shims inject process failures, without running gateways or builds.
"""
import os
import shlex
import sys
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
MAKEFILE = Path(os.environ.get("PARROT_MAKEFILE", ROOT / "Makefile"))
MAKE = shutil.which(os.environ.get("PARROT_MAKE", "make"))


class MakeContracts(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(MAKE, "GNU Make is required; missing tools are not skips")
        self.temporary = tempfile.TemporaryDirectory(prefix="parrot-make-contract-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        # Recursive $(MAKE) reads this same, unmodified production Makefile.
        shutil.copyfile(MAKEFILE, self.directory / "Makefile")
        self.events = self.directory / "events"
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        self.environment = dict(os.environ)
        for key in ["MAKEFLAGS", "MFLAGS", "GNUMAKEFLAGS", "MAKELEVEL", "MAKEOVERRIDES",
                    "PARROT_UNDEFINED_MAKE_CONTRACT"]:
            self.environment.pop(key, None)
        self.environment.update(
            PATH=str(self.bin) + os.pathsep + os.environ.get("PATH", ""),
            PARROT_MAKE_TEST_EVENTS=str(self.events),
            PARROT_MAKE_FAIL_TOOL="", PARROT_MAKE_FAIL_ARG="",
        )
        for directory in ['interop/erlang', 'interop/jvm', 'apps/crawler-lab/erlang', 'apps/crawler-lab/jvm']:
            (self.directory / directory).mkdir(parents=True)
        for name in ["cargo", "erl", "erlc", "mvn", "java", "node", "python3", "launcher"]:
            path = self.bin / name
            path.write_text(
                '#!/bin/sh\n'
                'printf "%s\\n" "${0##*/}:$*" >> "$PARROT_MAKE_TEST_EVENTS"\n'
                'if [ "${0##*/}" = "$PARROT_MAKE_FAIL_TOOL" ] && '
                '[ "$1" = "$PARROT_MAKE_FAIL_ARG" ]; then exit 23; fi\n'
                'exit 0\n'
            )
            path.chmod(0o755)
        shutil.copyfile(self.bin / "launcher", self.directory / "launcher")
        (self.directory / "launcher").chmod(0o755)
        (self.directory / "probes.mk").write_text(
            ".PHONY: contract-compound contract-pipeline contract-undefined contract-handled\n"
            "contract-compound:\n\t@false; echo FALSE_SUCCESS\n"
            "contract-pipeline:\n\t@false | cat; echo FALSE_SUCCESS\n"
            "contract-undefined:\n\t@echo \"$$PARROT_UNDEFINED_MAKE_CONTRACT\"; echo FALSE_SUCCESS\n"
            "contract-handled:\n\t@if false; then exit 1; fi; false || true; printf ok | cat\n"
        )

    def run_make(self, target, *args, fail_tool="", fail_arg=""):
        environment = dict(self.environment, PARROT_MAKE_FAIL_TOOL=fail_tool,
                           PARROT_MAKE_FAIL_ARG=fail_arg)
        result = subprocess.run(
            [MAKE, "--no-print-directory", "-f", "Makefile", "-f", "probes.mk",
             target, "CARGO=" + str(self.bin / "cargo"), "LAB_SCRIPT=launcher", *args],
            cwd=self.directory, env=environment, text=True, capture_output=True, timeout=15,
        )
        events = self.events.read_text().splitlines() if self.events.exists() else []
        return result, events

    def failed(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("FALSE_SUCCESS", result.stdout)
        self.assertNotIn("五运行时全链场景通过", result.stdout)
        self.assertNotIn("crawler 场景通过", result.stdout)
        self.assertNotIn("快速模式完成", result.stdout)

    def test_compound_recipe_stops_after_failed_command(self):
        result, _ = self.run_make("contract-compound")
        self.failed(result)

    def test_pipeline_propagates_failure_from_an_earlier_process(self):
        result, _ = self.run_make("contract-pipeline")
        self.failed(result)

    def test_undefined_recipe_variable_is_an_error(self):
        result, _ = self.run_make("contract-undefined")
        self.failed(result)
        self.assertIn("PARROT_UNDEFINED_MAKE_CONTRACT", result.stderr)

    def test_handled_errors_and_successful_pipelines_remain_valid(self):
        result, _ = self.run_make("contract-handled")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "ok")

    def test_lab_build_failure_does_not_launch_stale_binary_or_print_success(self):
        result, events = self.run_make("test-lab", fail_tool="cargo", fail_arg="build")
        self.failed(result)
        self.assertTrue(any(line.startswith("cargo:build") for line in events), events)
        self.assertFalse(any(line.startswith("launcher:") for line in events), events)

    def test_lab_scenario_failure_is_not_success(self):
        result, events = self.run_make("test-lab", fail_tool="launcher", fail_arg="--pages")
        self.failed(result)
        self.assertTrue(any(line.startswith("launcher:--pages 200") for line in events), events)

    def test_lab_success_requires_both_build_and_scenario(self):
        result, events = self.run_make("test-lab")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        steps = [line.split(":", 1)[0] for line in events if line.startswith(("cargo:", "launcher:"))]
        self.assertEqual(steps, ["cargo", "launcher"])
        self.assertIn("crawler 场景通过", result.stdout)

    def test_lab_gateway_build_failure_does_not_launch(self):
        result, events = self.run_make("test-lab", fail_tool="erlc", fail_arg="parrot_gw.erl")
        self.failed(result)
        self.assertTrue(any(line.startswith("erlc:parrot_gw.erl") for line in events), events)
        self.assertFalse(any(line.startswith("launcher:") for line in events), events)

    def test_lab_component_build_failure_does_not_launch(self):
        result, events = self.run_make("test-lab", fail_tool="erlc", fail_arg="frontier.erl")
        self.failed(result)
        self.assertTrue(any(line.startswith("erlc:frontier.erl") for line in events), events)
        self.assertFalse(any(line.startswith("launcher:") for line in events), events)

    def test_recursive_test_target_preserves_build_failure(self):
        result, events = self.run_make("test", "MODE=fast", fail_tool="cargo", fail_arg="build")
        self.failed(result)
        self.assertTrue(any(line.startswith("cargo:build") for line in events), events)
        self.assertFalse(any(line.startswith("cargo:test") for line in events), events)

    def test_recursive_test_target_preserves_test_failure(self):
        result, events = self.run_make("test", "MODE=fast", fail_tool="cargo", fail_arg="test")
        self.failed(result)
        self.assertEqual([line.split(":", 1)[0] for line in events], ["cargo", "python3", "cargo"])
        self.assertTrue(events[-1].startswith("cargo:test"), events)


    def test_erlang_compile_failure_prevents_runtime_tests_and_cleans_temporary_beams(self):
        temporary_beams = self.directory / "temporary-beams"
        temporary_beams.mkdir()
        self.environment["TMPDIR"] = str(temporary_beams)
        for target in ["erl", "check-erl"]:
            with self.subTest(target=target):
                if self.events.exists():
                    self.events.unlink()
                result, events = self.run_make(target, fail_tool="erlc", fail_arg="-Werror")
                self.failed(result)
                self.assertTrue(any(line.startswith("erlc:-Werror") for line in events), events)
                self.assertFalse(any(line.startswith(("erl:", "python3:")) for line in events), events)
                self.assertEqual(list(temporary_beams.iterdir()), [])

    def test_component_preparation_failure_prevents_cargo_tests(self):
        for target in ['test-fast', 'test-stress']:
            with self.subTest(target=target):
                self.events.unlink(missing_ok=True)
                result, events = self.run_make(target, fail_tool='python3',
                                               fail_arg='scripts/crawler_business_artifacts.py')
                self.failed(result)
                self.assertTrue(any(line.startswith('python3:scripts/crawler_business_artifacts.py build') for line in events), events)
                self.assertFalse(any(line.startswith('cargo:test') for line in events), events)

    def test_erlang_targets_execute_lease_activation_once(self):
        expected = 'erl:lease-activation-contract'
        for target in ['erl', 'check-erl']:
            with self.subTest(target=target):
                self.events.unlink(missing_ok=True)
                result, events = self.run_make(target, 'ERL_LEASE_RUN=erl lease-activation-contract')
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(events.count(expected), 1, events)

    def test_erlang_targets_execute_shared_fixture_contracts_once(self):
        for target in ['erl', 'check-erl']:
            with self.subTest(target=target):
                self.events.unlink(missing_ok=True)
                result, events = self.run_make(target, 'ERL_VECTOR_RUN=python3 shared-fixture-contract')
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(events.count('python3:shared-fixture-contract'), 1, events)

    def test_erlang_shared_fixture_failure_stops_runtime_tests(self):
        for target in ['erl', 'check-erl']:
            with self.subTest(target=target):
                self.events.unlink(missing_ok=True)
                result, events = self.run_make(target, 'ERL_VECTOR_RUN=python3 shared-fixture-contract',
                                               fail_tool='python3', fail_arg='shared-fixture-contract')
                self.failed(result)
                self.assertEqual(events.count('python3:shared-fixture-contract'), 1, events)
                self.assertFalse(any(line.startswith('erl:') for line in events), events)
                self.assertEqual(events[-1], 'python3:shared-fixture-contract', events)

    def test_erlang_lease_activation_failure_stops_later_tests(self):
        expected = 'erl:lease-activation-contract'
        for target in ['erl', 'check-erl']:
            with self.subTest(target=target):
                self.events.unlink(missing_ok=True)
                result, events = self.run_make(target, 'ERL_LEASE_RUN=erl lease-activation-contract',
                                               fail_tool='erl', fail_arg='lease-activation-contract')
                self.failed(result)
                self.assertEqual(events.count(expected), 1, events)
                self.assertEqual(events[-1], expected, events)

    def test_erlang_targets_execute_boundary_suites_once(self):
        for target in ['erl', 'check-erl']:
            with self.subTest(target=target):
                self.events.unlink(missing_ok=True)
                result, events = self.run_make(target)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                for script in ['test_negotiated_limits.py', 'test_heartbeat_envelopes.py']:
                    self.assertEqual(events.count('python3:interop/erlang/' + script), 1, events)

    def test_erlang_boundary_suite_failure_stops_later_tests(self):
        for target in ['erl', 'check-erl']:
            for script in ['test_negotiated_limits.py', 'test_heartbeat_envelopes.py']:
                with self.subTest(target=target, script=script):
                    self.events.unlink(missing_ok=True)
                    path = 'interop/erlang/' + script
                    expected = 'python3:' + path
                    result, events = self.run_make(target, fail_tool='python3', fail_arg=path)
                    self.failed(result)
                    self.assertEqual(events.count(expected), 1, events)
                    self.assertEqual(events[-1], expected, events)

    def test_unrelated_crate_scope_does_not_require_business_components(self):
        result, events = self.run_make('test-fast', 'CRATE=parrot', fail_tool='python3',
                                       fail_arg='scripts/crawler_business_artifacts.py')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(line.startswith('python3:') for line in events), events)
        self.assertTrue(any(line.startswith('cargo:test -p parrot') for line in events), events)

    def test_crawler_scope_prepares_receipt_before_tests(self):
        result, events = self.run_make('test-fast', 'CRATE=crawler-lab')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([line.split(':', 1)[0] for line in events], ['cargo', 'python3', 'cargo'])
        self.assertTrue(events[-1].startswith('cargo:test -p crawler-lab'), events)


    def performance_failure(self, name):
        # Model only test selection, not performance: the chosen contract fails
        # deterministically if the actual recipe asks Cargo to execute it.
        self.environment['PARROT_PERF_FAILURE'] = name
        self.environment['PARROT_PERF_NO_TESTS'] = ''
        (self.bin / 'cargo').write_text(
            '#!' + sys.executable + '\n' + r'''
import os, sys
from pathlib import Path
args = sys.argv[1:]
with Path(os.environ['PARROT_MAKE_TEST_EVENTS']).open('a') as out:
    out.write('cargo:' + ' '.join(args) + '\n')
if not args or args[0] != 'test' or '--release' not in args:
    sys.exit(0)
current = 'a5_herd_spawn_throughput_thread'
stretch = current + '_stretch'
a1 = 'a1_dedicated_thread_isolation'
m4 = 'm4_static_track_matches_pure_actix'
names = {current, stretch, a1, m4}
if '--test' in args and args[args.index('--test') + 1] == 'test_thread_advantages':
    names.remove(m4)
if '--exact' in args:
    names.intersection_update(args)
if '--include-ignored' not in args and '--ignored' not in args:
    names.clear()
for index, arg in enumerate(args[:-1]):
    if arg == '--skip':
        names = {name for name in names if args[index + 1] not in name}
if os.environ.get('PARROT_PERF_NO_TESTS'):
    names.clear()
if '--list' in args:
    for name in sorted(names):
        print(name + ': test')
    sys.exit(0)
sys.exit(23 if os.environ['PARROT_PERF_FAILURE'] in names else 0)
''')
        (self.bin / 'cargo').chmod(0o755)

    def assert_current_performance_selection(self, events):
        selected = [e for e in events if e.startswith('cargo:test ') and '--release' in e]
        self.assertTrue(selected, events)
        for line in selected:
            args = shlex.split(line[len('cargo:'):])
            self.assertIn('--include-ignored', args)
            skips = [args[i + 1] for i, a in enumerate(args[:-1]) if a == '--skip']
            self.assertEqual(skips, ['a5_herd_spawn_throughput_thread_stretch'], line)
            self.assertEqual([a for a in args if a.startswith('--test-threads=')], ['--test-threads=1'])

    def test_full_acceptance_excludes_only_a5_stretch(self):
        self.performance_failure('a5_herd_spawn_throughput_thread_stretch')
        result, events = self.run_make('test-full')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_current_performance_selection(events)

    def test_stress_acceptance_excludes_only_a5_stretch(self):
        self.performance_failure('a5_herd_spawn_throughput_thread_stretch')
        result, events = self.run_make('test-stress')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_current_performance_selection(events)

    def test_ci_release_acceptance_excludes_only_a5_stretch(self):
        workflow = Path(os.environ.get('PARROT_CI_WORKFLOW', ROOT / '.github/workflows/ci.yml'))
        lines = workflow.read_text().splitlines()
        marker = '      - name: Release-only threshold tests'
        self.assertEqual(lines.count(marker), 1)
        command_line = lines[lines.index(marker) + 1].strip()
        self.assertTrue(command_line.startswith('run: '))
        command = shlex.split(command_line[len('run: '):])
        if command[0] == 'make':
            command[0] = MAKE
        self.performance_failure('a5_herd_spawn_throughput_thread_stretch')
        result = subprocess.run(command, cwd=self.directory, env=self.environment,
                                capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_current_performance_selection(self.events.read_text().splitlines())

    def test_explicit_a5_stretch_target_preserves_failure(self):
        name = 'a5_herd_spawn_throughput_thread_stretch'
        self.performance_failure(name)
        result, events = self.run_make('check-a5-stretch')
        self.failed(result)
        selected = [e for e in events if e.startswith('cargo:test ') and '--list' not in e]
        self.assertEqual(len(selected), 1, events)
        args = shlex.split(selected[0][len('cargo:'):])
        self.assertIn(name, args)
        self.assertIn('--exact', args)
        self.assertIn('--ignored', args)
        self.assertNotIn('--skip', args)

    def test_current_a5_target_preserves_failure(self):
        name = 'a5_herd_spawn_throughput_thread'
        self.performance_failure(name)
        result, events = self.run_make('check-a5-perf')
        self.failed(result)
        selected = [e for e in events if e.startswith('cargo:test ') and '--list' not in e]
        self.assertEqual(len(selected), 1, events)
        args = shlex.split(selected[0][len('cargo:'):])
        self.assertIn(name, args)
        self.assertIn('--exact', args)
        self.assertIn('--ignored', args)

    def test_full_keeps_a1_as_a_failing_requirement(self):
        self.performance_failure('a1_dedicated_thread_isolation')
        result, events = self.run_make('test-full')
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn('✓ 全模式完成', result.stdout)
        self.assertTrue(any('--release' in e for e in events), events)

    def test_stress_keeps_m4_as_a_failing_requirement(self):
        self.performance_failure('m4_static_track_matches_pure_actix')
        result, events = self.run_make('test-stress')
        self.failed(result)
        self.assertTrue(any('--release' in e for e in events), events)

    def test_full_stress_preserves_requested_crate_and_feature_scope(self):
        self.performance_failure('no-performance-failure')
        result, events = self.run_make('test-full', 'CRATE=parrot', 'FEATURE=parrot_envelope_legacy')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        stress = [shlex.split(e[len('cargo:'):]) for e in events
                  if e.startswith('cargo:test ') and '--release' in e and '--tests' in e]
        self.assertEqual(len(stress), 1, events)
        args = stress[0]
        self.assertEqual(args[args.index('-p') + 1], 'parrot')
        self.assertIn('--features', args)
        self.assertEqual(args[args.index('--features') + 1], 'parrot_envelope_legacy')

    def test_named_a5_gates_reject_an_empty_test_selection(self):
        for target in ['check-a5-perf', 'check-a5-stretch', 'check-thread-perf']:
            with self.subTest(target=target):
                self.events.unlink(missing_ok=True)
                self.performance_failure('no-performance-failure')
                self.environment['PARROT_PERF_NO_TESTS'] = '1'
                result, events = self.run_make(target)
                self.failed(result)
                self.assertIn('ERROR: expected exactly one', result.stdout)
                cargo = [e for e in events if e.startswith('cargo:test ')]
                self.assertEqual(len(cargo), 1, events)
                self.assertIn('--list', cargo[0])


if __name__ == "__main__":
    unittest.main()
