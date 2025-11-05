mod engine_stress_common;
use engine_stress_common::{BenchResult, Report, latency_stats};
use std::time::Duration;

fn scenario(name: &str, correctness: bool) -> BenchResult {
    BenchResult {
        unit: "messages",
        name: name.into(),
        engine: "fixture",
        total_messages: 4,
        elapsed: Duration::from_secs(1),
        latencies: latency_stats(vec![1, 2, 3, 4]),
        correctness,
        note: format!("{name} evidence"),
    }
}
#[test]
fn a_failed_scenario_cannot_be_hidden_by_successful_scenarios_or_report_rendering() {
    let report = Report::new();
    report.push(scenario("reply-lost", false));
    report.push(scenario("echo-ok", true));
    report.push(scenario("cleanup-failed", false));
    assert!(report.dump_markdown().contains("reply-lost"));
    let error = report.verify().unwrap_err().to_string();
    assert!(error.contains("reply-lost evidence"));
    assert!(error.contains("cleanup-failed evidence"));
    assert!(
        report.verify().is_err(),
        "rendering/checking must not consume failures"
    );
}
#[test]
fn an_empty_experiment_is_not_a_passing_stress_run() {
    assert!(Report::new().verify().is_err());
}
#[test]
fn successful_scenarios_produce_a_successful_verdict() {
    let report = Report::new();
    report.push(scenario("echo", true));
    report.push(scenario("cleanup", true));
    report.verify().unwrap();
}
#[test]
fn latency_percentiles_retain_tail_samples() {
    let mut samples = vec![1; 98];
    samples.extend([500, 1000]);
    let stats = latency_stats(samples);
    assert_eq!(stats.count, 100);
    assert_eq!(stats.p99_us, 500);
    assert_eq!(stats.max_us, 1000);
    assert_eq!(stats.p50_us, 1);
}

#[test]
fn chunking_preserves_the_fixed_work_result_including_a_short_final_chunk() {
    use engine_stress_common::{burn_cpu, burn_cpu_range};
    for salt in [0, 3, u64::MAX] {
        let expected = burn_cpu(107, salt);
        for chunk in [1, 8, 107, 200] {
            let mut state = salt | 1;
            let mut done = 0;
            while done < 107 {
                let take = chunk.min(107 - done);
                state = burn_cpu_range(done, take, state);
                done += take;
            }
            assert_eq!(state, expected);
        }
    }
}

#[test]
fn missing_latency_samples_and_zero_duration_are_not_reported_as_measurements() {
    let mut result = scenario("boundary-check", true);
    result.latencies = latency_stats(vec![]);
    result.elapsed = Duration::ZERO;
    assert!(result.msg_per_sec().is_none());
    let report = Report::new();
    report.push(result);
    let text = report.dump_markdown();
    assert!(text.contains("n/a"));
    assert!(!text.contains("inf"));
    report.verify().unwrap();
}

#[test]
fn stalled_probe_crossing_the_whole_cpu_interval_is_not_lost() {
    use engine_stress_common::{ProbeSample, probe_window};
    let origin = std::time::Instant::now();
    let t = |ms| origin + Duration::from_millis(ms);
    let samples = [
        ProbeSample {
            started: t(0),
            completed: t(5),
        },
        ProbeSample {
            started: t(9),
            completed: t(31),
        },
        ProbeSample {
            started: t(12),
            completed: t(14),
        },
        ProbeSample {
            started: t(29),
            completed: t(35),
        },
        ProbeSample {
            started: t(31),
            completed: t(32),
        },
    ];
    let window = probe_window(&samples, t(10), t(30)).unwrap();
    assert_eq!(window.baseline.count, 1);
    assert_eq!(window.overlapping.count, 3);
    assert_eq!(window.overlapping.max_us, 22_000);
    assert_eq!(window.completed, 1);
    let stalled = probe_window(&samples[1..2], t(10), t(30)).unwrap();
    assert_eq!(stalled.completed, 0);
    assert_eq!(stalled.overlapping.max_us, 22_000);
}
#[test]
fn sampling_rejects_reversed_time_and_does_not_invent_progress_from_empty_samples() {
    use engine_stress_common::{ProbeSample, probe_window};
    let start = std::time::Instant::now();
    let end = start + Duration::from_secs(1);
    let empty = probe_window(&[], start, end).unwrap();
    assert_eq!(empty.completed, 0);
    assert_eq!(empty.overlapping.count, 0);
    assert!(probe_window(&[], start, start).is_err());
    assert!(
        probe_window(
            &[ProbeSample {
                started: end,
                completed: start
            }],
            start,
            end
        )
        .is_err()
    );
}
#[test]
fn low_nonzero_throughput_is_not_rounded_to_zero() {
    let mut measured = scenario("slow", true);
    measured.total_messages = 1;
    measured.elapsed = Duration::from_secs(4);
    let report = Report::new();
    report.push(measured);
    assert!(report.dump_markdown().contains("0.25"));
}

#[test]
fn probe_window_boundaries_are_explicit_and_do_not_include_late_completions() {
    use engine_stress_common::{ProbeSample, probe_window};
    let origin = std::time::Instant::now();
    let t = |ms| origin + Duration::from_millis(ms);
    let samples = [
        ProbeSample {
            started: t(0),
            completed: t(10),
        },
        ProbeSample {
            started: t(10),
            completed: t(20),
        },
        ProbeSample {
            started: t(20),
            completed: t(21),
        },
    ];
    let measured = probe_window(&samples, t(10), t(20)).unwrap();
    assert_eq!(measured.baseline.count, 1);
    assert_eq!(measured.completed, 1);
    assert_eq!(measured.overlapping.count, 1);
    assert_eq!(measured.overlapping.max_us, 10_000);
    assert!(probe_window(&samples, t(20), t(10)).is_err());
}
#[test]
fn timed_cpu_keeps_the_actual_computation_result_and_interval() {
    use engine_stress_common::{burn_cpu, timed_cpu};
    let before = std::time::Instant::now();
    let result = timed_cpu(10_000);
    let after = std::time::Instant::now();
    assert_eq!(result.value, burn_cpu(10_000, 7));
    assert!(
        before <= result.started && result.started <= result.completed && result.completed <= after
    );
}

#[test]
fn saved_report_preserves_the_measurement_boundary_and_scenario_notes() {
    let report = Report::new();
    report.push(scenario("probe", true));
    assert!(
        report.dump_markdown().contains("probe evidence"),
        "a saved result must carry its interpretation, not only unlabelled numbers"
    );
}

#[test]
fn positive_but_wrong_numeric_replies_and_wrong_types_are_rejected() {
    use engine_stress_common::contracts::verify_u64;
    assert!(verify_u64(Box::new(1u64), 2).is_err());
    assert!(verify_u64(Box::new("2"), 2).is_err());
    verify_u64(Box::new(0u64), 0).unwrap();
}

#[test]
fn cpu_reference_has_independent_literal_vectors_including_the_old_early_exit() {
    use engine_stress_common::{burn_cpu, oracle};
    for (n, salt, expected) in [
        (107, 3, 4046730678074085073),
        (2, 3639755004476521255, 10481999410520546992),
        (0, 0, 1),
    ] {
        assert_eq!(oracle::reference(n, salt), expected);
        assert_eq!(burn_cpu(n, salt), expected);
    }
}

#[test]
fn reports_preserve_units_and_propagate_output_errors() {
    let path = std::env::temp_dir().join(format!("parrot-stress-report-{}", uuid::Uuid::new_v4()));
    let report = Report::new();
    let mut row = scenario("stops", true);
    row.unit = "lifecycle operations";
    report.push(row);
    report.save(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("lifecycle operations"));
    assert!(text.contains("schema: 2"));
    assert!(text.contains("run_id:"));
    assert!(text.contains("runtime:"));
    assert!(text.contains("stops evidence"));
    let result = report.save(path.join("cannot-write-under-a-file"));
    std::fs::remove_file(path).unwrap();
    assert!(result.is_err());
}
