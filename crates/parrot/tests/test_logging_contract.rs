use parrot::logging::{self, LogConfig};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
use tracing::Level;

fn config() -> LogConfig {
    LogConfig {
        show_file_line: false,
        show_thread_info: false,
        show_time: false,
        ..Default::default()
    }
}
fn child_case(name: &str, action: impl FnOnce(&Path), verify: impl FnOnce(&str, &Path)) {
    const KEY: &str = "PARROT_LOGGING_CONTRACT_CASE";
    if std::env::var(KEY).as_deref() == Ok(name) {
        action(Path::new(
            &std::env::var("PARROT_LOGGING_CONTRACT_DIR").unwrap(),
        ));
        return;
    }
    let dir = std::env::temp_dir().join(format!("parrot-logging-{}-{name}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture"])
        .env(KEY, name)
        .env("PARROT_LOGGING_CONTRACT_DIR", &dir)
        .env_remove("RUST_LOG")
        .current_dir(&dir)
        .stdout(std::fs::File::create(dir.join("stdout.txt")).unwrap())
        .stderr(std::fs::File::create(dir.join("stderr.txt")).unwrap());
    if name == "environment_level_overrides_default" {
        command.env("RUST_LOG", "debug");
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("logging child deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let stdout = std::fs::read_to_string(dir.join("stdout.txt")).unwrap();
    let stderr = std::fs::read_to_string(dir.join("stderr.txt")).unwrap();
    assert!(
        output.status.success(),
        "{name}: {}\n{stdout}\n{}",
        output.status,
        stderr
    );
    verify(&stdout, &dir);
}
fn log_path(dir: &Path) -> std::path::PathBuf {
    dir.join("events.log")
}
fn file(dir: &Path) -> String {
    std::fs::read_to_string(log_path(dir)).unwrap()
}
fn event(text: &str, marker: &str) -> String {
    let lines: Vec<_> = text.lines().filter(|line| line.contains(marker)).collect();
    assert_eq!(lines.len(), 1, "missing or duplicated event: {text}");
    lines[0].to_owned()
}
fn json_event(text: &str, marker: &str) -> serde_json::Value {
    serde_json::from_str(&event(text, marker)).expect("requested JSON event")
}

#[test]
fn failed_file_open_is_reported_and_retryable() {
    child_case(
        "failed_file_open_is_reported_and_retryable",
        |dir| {
            let invalid = dir.join("missing/events.log");
            let failure = logging::init_with_file(config(), invalid.to_str().unwrap());
            assert!(
                failure.is_err(),
                "opening a missing parent must return an IO error"
            );
            logging::init_with_file(config(), log_path(dir).to_str().unwrap()).unwrap();
            tracing::warn!("RETRY_AFTER_OPEN_FAILURE");
        },
        |stdout, dir| {
            event(stdout, "RETRY_AFTER_OPEN_FAILURE");
            event(&file(dir), "RETRY_AFTER_OPEN_FAILURE");
        },
    );
}
#[test]
fn foreign_subscriber_is_reported() {
    child_case(
        "foreign_subscriber_is_reported",
        |dir| {
            tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
            let result = logging::init_with_file(config(), log_path(dir).to_str().unwrap());
            assert_eq!(
                result.unwrap_err().kind(),
                std::io::ErrorKind::AlreadyExists
            );
        },
        |_, _| {},
    );
}
#[test]
fn plain_hides_timestamp_and_location() {
    child_case(
        "plain_hides_timestamp_and_location",
        |_| {
            logging::init(config());
            tracing::info!("PLAIN_NO_TIME");
        },
        |stdout, _| {
            let line = event(stdout, "PLAIN_NO_TIME");
            assert!(
                line.trim_start().starts_with("INFO"),
                "timestamp leaked: {line}"
            );
            assert!(!line.contains(".rs:") && !line.contains("ThreadId"));
        },
    );
}
#[test]
fn json_hides_timestamp() {
    child_case(
        "json_hides_timestamp",
        |_| {
            logging::init(LogConfig {
                json_format: true,
                ..config()
            });
            tracing::info!("JSON_NO_TIME");
        },
        |stdout, _| {
            let value = json_event(stdout, "JSON_NO_TIME");
            for key in [
                "timestamp",
                "filename",
                "line_number",
                "threadName",
                "threadId",
            ] {
                assert!(value.get(key).is_none(), "unexpected {key}: {value}");
            }
        },
    );
}
#[test]
fn json_shows_requested_location_and_thread() {
    child_case(
        "json_shows_requested_location_and_thread",
        |_| {
            logging::init(LogConfig {
                json_format: true,
                show_file_line: true,
                show_thread_info: true,
                show_time: true,
                ..config()
            });
            tracing::info!("JSON_WITH_METADATA");
        },
        |stdout, _| {
            let value = json_event(stdout, "JSON_WITH_METADATA");
            for key in [
                "timestamp",
                "filename",
                "line_number",
                "threadName",
                "threadId",
            ] {
                assert!(value.get(key).is_some(), "missing {key}: {value}");
            }
        },
    );
}
#[test]
fn file_and_console_use_requested_json_flags() {
    child_case(
        "file_and_console_use_requested_json_flags",
        |dir| {
            logging::init_with_file(
                LogConfig {
                    json_format: true,
                    ..config()
                },
                log_path(dir).to_str().unwrap(),
            )
            .unwrap();
            tracing::info!("FILE_JSON_NO_METADATA");
        },
        |stdout, dir| {
            for text in [stdout.to_owned(), file(dir)] {
                let value = json_event(&text, "FILE_JSON_NO_METADATA");
                for key in [
                    "timestamp",
                    "filename",
                    "line_number",
                    "threadName",
                    "threadId",
                ] {
                    assert!(value.get(key).is_none(), "unexpected {key}: {value}");
                }
                assert!(!text.contains('\u{1b}'));
            }
        },
    );
}
#[test]
fn file_target_filters_match_console() {
    child_case(
        "file_target_filters_match_console",
        |dir| {
            logging::init_with_file(
                LogConfig {
                    level: Level::ERROR,
                    target_filters: Some("contract_target=debug".into()),
                    ..config()
                },
                log_path(dir).to_str().unwrap(),
            )
            .unwrap();
            tracing::debug!(target:"contract_target", "TARGET_DEBUG_VISIBLE");
            tracing::info!(target:"other_target", "OTHER_INFO_HIDDEN");
        },
        |stdout, dir| {
            for text in [stdout.to_owned(), file(dir)] {
                event(&text, "TARGET_DEBUG_VISIBLE");
                assert!(!text.contains("OTHER_INFO_HIDDEN"));
            }
        },
    );
}
#[test]
fn environment_level_overrides_default() {
    child_case(
        "environment_level_overrides_default",
        |_| {
            logging::init(config());
            tracing::debug!("ENV_DEBUG_VISIBLE");
        },
        |stdout, _| {
            event(stdout, "ENV_DEBUG_VISIBLE");
        },
    );
}
#[test]
fn test_file_profile_is_quiet() {
    child_case(
        "test_file_profile_is_quiet",
        |dir| {
            logging::init_test_with_file(log_path(dir).to_str().unwrap());
            tracing::info!("TEST_INFO_HIDDEN");
            tracing::warn!("TEST_WARN_VISIBLE");
        },
        |stdout, dir| {
            for text in [stdout.to_owned(), file(dir)] {
                assert!(!text.contains("TEST_INFO_HIDDEN"));
                let line = event(&text, "TEST_WARN_VISIBLE");
                assert!(line.trim_start().starts_with("WARN"));
                assert!(line.contains(".rs:"));
                assert!(!line.contains("ThreadId"));
            }
        },
    );
}
#[test]
fn development_file_profile_includes_debug() {
    child_case(
        "development_file_profile_includes_debug",
        |dir| {
            logging::init_development_with_file(log_path(dir).to_str().unwrap());
            tracing::debug!("DEV_DEBUG_VISIBLE");
            tracing::trace!(target:"parrot::thread", "DEV_TRACE_VISIBLE");
        },
        |stdout, dir| {
            for text in [stdout.to_owned(), file(dir)] {
                event(&text, "DEV_DEBUG_VISIBLE");
                event(&text, "DEV_TRACE_VISIBLE");
            }
        },
    );
}
#[test]
fn production_file_profile_is_structured() {
    child_case(
        "production_file_profile_is_structured",
        |dir| {
            logging::init_production_with_file(log_path(dir).to_str().unwrap());
            tracing::info!("PRODUCTION_JSON");
        },
        |stdout, dir| {
            for text in [stdout.to_owned(), file(dir)] {
                let value = json_event(&text, "PRODUCTION_JSON");
                assert!(value.get("timestamp").is_some() && value.get("threadId").is_some());
                assert!(value.get("filename").is_none() && value.get("line_number").is_none());
            }
        },
    );
}
#[test]
fn successful_initialization_is_idempotent_and_appends() {
    child_case(
        "successful_initialization_is_idempotent_and_appends",
        |dir| {
            std::fs::write(log_path(dir), "OLD_CONTENT\n").unwrap();
            logging::init_with_file(config(), log_path(dir).to_str().unwrap()).unwrap();
            logging::init_with_file(config(), dir.join("missing/unused.log").to_str().unwrap())
                .unwrap();
            tracing::info!("ORIGINAL_DESTINATION");
        },
        |stdout, dir| {
            event(stdout, "ORIGINAL_DESTINATION");
            let text = file(dir);
            assert!(text.starts_with("OLD_CONTENT\n"));
            event(&text, "ORIGINAL_DESTINATION");
        },
    );
}

#[test]
fn default_file_flag_creates_named_append_file() {
    child_case(
        "default_file_flag_creates_named_append_file",
        |dir| {
            // Use the documented filename literal: baseline builds must not depend
            // on the new public constant before the behavior is implemented.
            std::fs::write(dir.join("parrot.log"), "EXISTING_LOG\n").unwrap();
            logging::init_default(true);
            tracing::info!("DEFAULT_FILE_VISIBLE");
        },
        |stdout, dir| {
            event(stdout, "DEFAULT_FILE_VISIBLE");
            let text = std::fs::read_to_string(dir.join("parrot.log")).unwrap();
            assert!(text.starts_with("EXISTING_LOG\n"));
            event(&text, "DEFAULT_FILE_VISIBLE");
        },
    );
}
#[test]
fn default_console_flag_does_not_create_file() {
    child_case(
        "default_console_flag_does_not_create_file",
        |_| {
            logging::init_default(false);
            tracing::info!("DEFAULT_CONSOLE_VISIBLE");
        },
        |stdout, dir| {
            event(stdout, "DEFAULT_CONSOLE_VISIBLE");
            assert!(!dir.join("parrot.log").exists());
        },
    );
}
fn initialize_concurrently(dir: &Path) {
    let start = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for index in 0..8 {
            let start = &start;
            scope.spawn(move || {
                let path = dir.join(format!("destination-{index}.log"));
                start.wait();
                logging::init_with_file(config(), path.to_str().unwrap()).unwrap();
            });
        }
    });
    tracing::warn!("CONCURRENT_DESTINATION");
}
fn verify_one_destination(stdout: &str, dir: &Path) {
    event(stdout, "CONCURRENT_DESTINATION");
    let files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("destination-")
        })
        .collect();
    assert_eq!(files.len(), 1, "initialization opened unused destinations");
    event(
        &std::fs::read_to_string(&files[0]).unwrap(),
        "CONCURRENT_DESTINATION",
    );
}
#[test]
fn concurrent_initializers_publish_one_destination() {
    child_case(
        "concurrent_initializers_publish_one_destination",
        initialize_concurrently,
        verify_one_destination,
    );
}
#[test]
fn failed_open_then_concurrent_retry_succeeds() {
    child_case(
        "failed_open_then_concurrent_retry_succeeds",
        |dir| {
            assert!(
                logging::init_with_file(config(), dir.join("missing/events.log").to_str().unwrap())
                    .is_err()
            );
            initialize_concurrently(dir);
        },
        verify_one_destination,
    );
}
#[test]
fn concurrent_records_remain_complete_json() {
    child_case(
        "concurrent_records_remain_complete_json",
        |dir| {
            logging::init_with_file(
                LogConfig {
                    json_format: true,
                    ..config()
                },
                log_path(dir).to_str().unwrap(),
            )
            .unwrap();
            let start = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                for worker in 0..8 {
                    let start = &start;
                    scope.spawn(move || {
                        start.wait();
                        for sequence in 0..16 {
                            tracing::info!(worker, sequence, "CONCURRENT_RECORD");
                        }
                    });
                }
            });
        },
        |stdout, dir| {
            for text in [stdout.to_owned(), file(dir)] {
                let mut observed = std::collections::BTreeSet::new();
                for line in text
                    .lines()
                    .filter(|line| line.contains("CONCURRENT_RECORD"))
                {
                    let value: serde_json::Value =
                        serde_json::from_str(line).expect("complete JSON record");
                    let worker = value["worker"].as_u64().unwrap();
                    let sequence = value["sequence"].as_u64().unwrap();
                    assert!(worker < 8 && sequence < 16);
                    assert!(observed.insert((worker, sequence)), "duplicate record");
                }
                assert_eq!(observed.len(), 128, "lost concurrent records");
            }
        },
    );
}
#[test]
fn console_initialization_prevents_later_file_side_effects() {
    child_case(
        "console_initialization_prevents_later_file_side_effects",
        |dir| {
            logging::init(config());
            logging::init_default(true);
            logging::init_with_file(config(), log_path(dir).to_str().unwrap()).unwrap();
            tracing::info!("FIRST_CONSOLE_RETAINED");
        },
        |stdout, dir| {
            event(stdout, "FIRST_CONSOLE_RETAINED");
            assert!(!dir.join("parrot.log").exists() && !log_path(dir).exists());
        },
    );
}
