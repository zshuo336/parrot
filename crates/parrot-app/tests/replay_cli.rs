#![cfg(feature = "host")]
#[path = "../examples/support/replay_counter.rs"]
mod counter;
use parrot_app::debug::{record_sink, FileSink, RecordSink};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// A separate OS process executes the exact public CLI entrypoint with this
// application binary's registered factory/codecs. It never calls a mock replay.
#[test]
fn fixture_command() {
    let Some(args) = std::env::var_os("PARROT_REPLAY_TEST_ARGS") else {
        return;
    };
    let args: Vec<String> = serde_json::from_str(args.to_str().unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        if args[0] == "capture" {
            counter::capture(PathBuf::from(&args[1])).await
        } else {
            parrot_app::cli::execute(args.into_iter().map(Into::into)).await
        }
    });
    if let Err(error) = &result {
        eprintln!("CLI_ERROR={error}");
    }
    drop(runtime);
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}
struct Case {
    dir: tempfile::TempDir,
    manifest: PathBuf,
    input: PathBuf,
    report: PathBuf,
    journal: PathBuf,
}
impl Case {
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("app.toml");
        let input = dir.path().join("capture.jsonl");
        let report = dir.path().join("report.json");
        let journal = dir.path().join("actor.log");
        std::fs::write(&manifest, format!("name = 'replay-case'\nversion = '1.0.0'\n[[components]]\nname = 'counter'\nengine = 'parrot'\nartifact = {{ Props = {{ factory = 'example.replay-counter' }} }}\n[components.config]\njournal = '{}'\n{config}\n[config_overlay.remote.node]\nbind = '127.0.0.1:0'\n[config_overlay.thread]\nshared_pool_size = 1\nshared_burst_workers_max = 0\n", journal.display())).unwrap();
        Self {
            dir,
            manifest,
            input,
            report,
            journal,
        }
    }
    fn args(&self) -> Vec<String> {
        vec![
            "replay".into(),
            "--manifest".into(),
            self.manifest.to_str().unwrap().into(),
            "--input".into(),
            self.input.to_str().unwrap().into(),
            "--report".into(),
            self.report.to_str().unwrap().into(),
        ]
    }
    fn legacy(&self, entries: &[(i64, u8, &str, &str)]) {
        let mut sink = FileSink::open(&self.input).unwrap();
        for (seq, (value, kind, direction, path)) in entries.iter().enumerate() {
            let payload =
                parrot_api::message::serde_remote_serialize(&counter::Count(*value)).unwrap();
            sink.write(&record_sink(
                seq as u64,
                100 + seq as u64,
                direction,
                *kind,
                path,
                "bin:example.ReplayCount#v1",
                &payload,
            ))
            .unwrap();
        }
        sink.finish().unwrap();
    }
    fn read(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(&self.report).unwrap()).unwrap()
    }
    fn journal(&self) -> String {
        std::fs::read_to_string(&self.journal).unwrap_or_default()
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        if let Some(destination) = std::env::var_os("PARROT_REPLAY_CLI_EVIDENCE") {
            let saved = PathBuf::from(destination).join(self.dir.path().file_name().unwrap());
            let result = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(&saved)?;
                fn copy(source: &Path, destination: &Path) -> std::io::Result<()> {
                    std::fs::create_dir_all(destination)?;
                    for entry in std::fs::read_dir(source)? {
                        let entry = entry?;
                        if entry.file_type()?.is_dir() {
                            copy(&entry.path(), &destination.join(entry.file_name()))?;
                        } else if entry.file_type()?.is_file() {
                            std::fs::copy(entry.path(), destination.join(entry.file_name()))?;
                        }
                    }
                    Ok(())
                }
                copy(self.dir.path(), &saved)?;
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("CLI evidence preservation failed: {error}");
            }
        }
    }
}
fn output(path: PathBuf) -> std::fs::File {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
}
fn start(args: &[String], dir: &Path) -> OwnedChild {
    use std::io::Write;
    writeln!(
        output(dir.join("commands.jsonl")),
        "{}",
        serde_json::to_string(args).unwrap()
    )
    .unwrap();
    OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_command", "--nocapture"])
            .env(
                "PARROT_REPLAY_TEST_ARGS",
                serde_json::to_string(args).unwrap(),
            )
            .env_remove("PARROT_APP_READY_FILE")
            .stdout(Stdio::from(output(dir.join("stdout.log"))))
            .stderr(Stdio::from(output(dir.join("stderr.log"))))
            .spawn()
            .unwrap(),
    )
}
struct OwnedChild(std::process::Child);
impl std::ops::Deref for OwnedChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
fn wait(child: &mut OwnedChild, dir: &Path) -> bool {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.success();
        }
        if Instant::now() >= end {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!(
                "CLI watchdog: {}",
                std::fs::read_to_string(dir.join("stderr.log")).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn run(args: &[String], dir: &Path) -> bool {
    wait(&mut start(args, dir), dir)
}
fn replies(report: &serde_json::Value) -> Vec<i64> {
    report["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            assert_eq!(r["type_key"], "bin:example.ReplayCount#v1");
            let hex = r["payload_hex"].as_str().unwrap();
            assert_eq!(hex.len() % 2, 0);
            let bytes: Vec<_> = hex
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
                .collect();
            parrot_api::message::serde_remote_deserialize::<counter::Count>(&bytes)
                .unwrap()
                .0
        })
        .collect()
}
const ASK: u8 = parrot_remote::frame::frame_type::ASK;
const TELL: u8 = parrot_remote::frame::frame_type::TELL;
#[test]
fn actual_capture_replays_into_fresh_actor_and_syncs_report() {
    let c = Case::new("");
    assert!(run(
        &["capture".into(), c.input.to_str().unwrap().into()],
        c.dir.path()
    ));
    let mut args = c.args();
    args.extend(["--target-node".into(), "capture-server".into()]);
    assert!(run(&args, c.dir.path()));
    let report = c.read();
    assert_eq!(report["status"], "succeeded");
    assert_eq!(report["delivered"], 3);
    assert_eq!(replies(&report), [2, 5, 9]);
    assert!(c.journal().ends_with("stopped\ndisposed\n"));
    assert!(!std::fs::read_to_string(c.dir.path().join("stdout.log"))
        .unwrap()
        .contains("PARROT_APP_READY="));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&c.report).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
#[test]
fn legacy_requires_explicit_opt_in_before_any_actor_starts() {
    let c = Case::new("");
    c.legacy(&[(2, ASK, "rx", "/user/counter")]);
    assert!(!run(&c.args(), c.dir.path()));
    assert!(c.journal().is_empty());
    assert!(!c.report.exists());
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(run(&args, c.dir.path()));
    assert_eq!(replies(&c.read()), [2]);
}
#[test]
fn truncated_capture_is_rejected_before_startup() {
    let c = Case::new("");
    assert!(run(
        &["capture".into(), c.input.to_str().unwrap().into()],
        c.dir.path()
    ));
    let text = std::fs::read_to_string(&c.input).unwrap();
    let cut = text.rfind('\n').unwrap();
    let cut = text[..cut].rfind('\n').unwrap();
    std::fs::write(&c.input, &text[..=cut]).unwrap();
    assert!(!run(&c.args(), c.dir.path()));
    assert!(c.journal().is_empty());
    assert!(!c.report.exists());
}
#[test]
fn existing_report_is_not_overwritten_and_no_actor_starts() {
    let c = Case::new("");
    c.legacy(&[(2, ASK, "rx", "/user/counter")]);
    std::fs::write(&c.report, "keep").unwrap();
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(!run(&args, c.dir.path()));
    assert_eq!(std::fs::read_to_string(&c.report).unwrap(), "keep");
    assert!(c.journal().is_empty());
}
#[test]
fn one_direction_is_replayed_and_other_copies_are_not_delivered() {
    for direction in ["rx", "tx"] {
        let c = Case::new("");
        c.legacy(&[
            (2, ASK, "rx", "/user/counter"),
            (7, ASK, "tx", "/user/counter"),
        ]);
        let mut args = c.args();
        args.extend([
            "--allow-legacy-log".into(),
            "--direction".into(),
            direction.into(),
        ]);
        assert!(run(&args, c.dir.path()));
        assert_eq!(replies(&c.read()), [if direction == "rx" { 2 } else { 7 }]);
        assert_eq!(c.read()["delivered"], 1);
    }
}
#[test]
fn ordinary_tell_then_ask_preserves_actor_order() {
    let c = Case::new("");
    c.legacy(&[
        (2, TELL, "rx", "/user/counter"),
        (3, ASK, "rx", "/user/counter"),
    ]);
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(run(&args, c.dir.path()));
    let report = c.read();
    assert_eq!(report["delivered"], 2);
    assert_eq!(report["tell_mode"], "completion");
    assert_eq!(report["replies"].as_array().unwrap().len(), 2);
    assert_completion(&report["replies"][0], 0, 100);
    assert_eq!(report["replies"][1]["cid"], 101);
    assert_eq!(
        replies(&serde_json::json!({"replies": [report["replies"][1]]})),
        [5]
    );
}
fn assert_completion(reply: &serde_json::Value, seq: u64, cid: u64) {
    assert_eq!(reply["seq"], seq);
    assert_eq!(reply["cid"], cid);
    assert!(reply["batch_index"].is_null());
    assert_eq!(reply["type_key"], parrot_remote::frame::TELL_COMPLETION_KEY);
    assert_eq!(reply["payload_hex"], "");
}
#[test]
fn trailing_tell_finishes_business_effect_before_host_retirement() {
    for explicit in [false, true] {
        let c = Case::new("delay_ms = 500");
        c.legacy(&[(2, TELL, "rx", "/user/counter")]);
        let mut args = c.args();
        args.push("--allow-legacy-log".into());
        if explicit {
            args.extend(["--tell-mode".into(), "completion".into()]);
        }
        assert!(run(&args, c.dir.path()));
        let report = c.read();
        assert_eq!(report["status"], "succeeded");
        assert_eq!(report["tell_mode"], "completion");
        assert_eq!(report["delivered"], 1);
        assert_eq!(report["replay_completed"], true);
        assert_eq!(report["replies"].as_array().unwrap().len(), 1);
        assert_completion(&report["replies"][0], 0, 100);
        assert_eq!(
            c.journal(),
            "started\nhandling 2\nvalue 2\nstopped\ndisposed\n"
        );
    }
}
#[test]
fn explicit_admission_does_not_fabricate_completion_receipts() {
    let c = Case::new("delay_ms = 500");
    c.legacy(&[(2, TELL, "rx", "/user/counter")]);
    let mut args = c.args();
    args.extend([
        "--allow-legacy-log".into(),
        "--tell-mode".into(),
        "admission".into(),
    ]);
    assert!(run(&args, c.dir.path()));
    let report = c.read();
    assert_eq!(report["tell_mode"], "admission");
    assert_eq!(report["status"], "succeeded");
    assert_eq!(report["delivered"], 1);
    assert_eq!(report["replies"], serde_json::json!([]));
    // Admission makes no promise either way about the handler's business effect.
    assert!(c.journal().ends_with("disposed\n"));
}
#[test]
fn ordinary_tell_handler_failure_is_not_replay_success() {
    let c = Case::new("");
    c.legacy(&[
        (2, TELL, "rx", "/user/counter"),
        (-1, TELL, "rx", "/user/counter"),
        (7, TELL, "rx", "/user/counter"),
    ]);
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(!run(&args, c.dir.path()));
    let report = c.read();
    assert_eq!(report["tell_mode"], "completion");
    assert_eq!(report["status"], "failed");
    assert_eq!(report["delivered"], 1);
    assert_eq!(report["failure_seq"], 1);
    assert_eq!(report["replay_completed"], false);
    assert!(report["replies"].is_null());
    assert!(report["error"]
        .as_str()
        .unwrap()
        .contains("negative increments"));
    assert!(!c.journal().contains("handling 7"));
    assert!(c.journal().ends_with("disposed\n"));
}
#[test]
fn ordinary_tell_deadline_cannot_be_reported_as_completed() {
    let c = Case::new("delay_ms = 500");
    c.legacy(&[(2, TELL, "rx", "/user/counter")]);
    let mut args = c.args();
    args.extend([
        "--allow-legacy-log".into(),
        "--call-timeout-ms".into(),
        "20".into(),
    ]);
    assert!(!run(&args, c.dir.path()));
    let report = c.read();
    assert_eq!(report["status"], "failed");
    assert_eq!(report["tell_mode"], "completion");
    assert_eq!(report["delivered"], 0);
    assert_eq!(report["failure_seq"], 0);
    assert_eq!(report["replay_completed"], false);
    assert!(report["replies"].is_null());
    assert!(c.journal().ends_with("disposed\n"));
}
#[test]
fn missing_later_target_fails_preflight_without_earlier_delivery() {
    let c = Case::new("");
    c.legacy(&[
        (2, ASK, "rx", "/user/counter"),
        (3, ASK, "rx", "/user/missing"),
    ]);
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(!run(&args, c.dir.path()));
    let report = c.read();
    assert_eq!(report["status"], "failed");
    assert_eq!(report["delivered"], 0);
    assert_eq!(report["failure_seq"], 1);
    assert!(!c.journal().contains("handling"));
    assert!(c.journal().contains("stopped"));
}
#[test]
fn actor_failure_reports_confirmed_progress_and_stops_later_calls() {
    let c = Case::new("");
    c.legacy(&[
        (2, ASK, "rx", "/user/counter"),
        (-1, ASK, "rx", "/user/counter"),
        (7, ASK, "rx", "/user/counter"),
    ]);
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(!run(&args, c.dir.path()));
    let report = c.read();
    assert_eq!(report["status"], "failed");
    assert_eq!(report["delivered"], 1);
    assert_eq!(report["failure_seq"], 1);
    assert!(report["replies"].is_null());
    assert!(!c.journal().contains("handling 7"));
}
#[test]
fn call_timeout_fails_and_owned_actor_is_retired() {
    let c = Case::new("delay_ms = 200");
    c.legacy(&[(2, ASK, "rx", "/user/counter")]);
    let mut args = c.args();
    args.extend([
        "--allow-legacy-log".into(),
        "--call-timeout-ms".into(),
        "20".into(),
    ]);
    assert!(!run(&args, c.dir.path()));
    assert_eq!(c.read()["status"], "failed");
    assert_eq!(c.read()["delivered"], 0);
    assert!(c.journal().contains("stopped"));
}
#[test]
fn teardown_failure_keeps_delivery_details_but_is_not_success() {
    let c = Case::new("fail_stop = true");
    c.legacy(&[(2, ASK, "rx", "/user/counter")]);
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(!run(&args, c.dir.path()));
    let report = c.read();
    assert_eq!(report["status"], "failed");
    assert_eq!(report["replay_completed"], true);
    assert_eq!(report["delivered"], 1);
    assert_eq!(replies(&report), [2]);
    assert!(c.journal().contains("stopped"));
}
#[test]
fn invalid_arguments_fail_before_loading_or_starting_resources() {
    for extra in [
        vec!["--direction", "both"],
        vec!["--direction", "rx", "--direction", "tx"],
        vec!["--tell-mode", "unknown"],
        vec!["--tell-mode", "completion", "--tell-mode", "admission"],
        vec!["--tell-mode"],
        vec!["--call-timeout-ms", "0"],
        vec!["--call-timeout-ms", "86400001"],
        vec!["--target-node", ""],
        vec!["--allow-legacy-log", "--allow-legacy-log"],
        vec!["--record", "invalid"],
    ] {
        let c = Case::new("");
        let mut args = c.args();
        args.extend(extra.into_iter().map(String::from));
        assert!(!run(&args, c.dir.path()));
        assert!(c.journal().is_empty());
        assert!(!c.report.exists());
    }
}
#[test]
fn explicit_uri_node_mapping_is_required() {
    let c = Case::new("");
    c.legacy(&[(2, ASK, "rx", "parrot://original/user/counter")]);
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(!run(&args, c.dir.path()));
    assert_eq!(c.read()["delivered"], 0);
    assert!(!c.journal().contains("handling"));
}
#[cfg(unix)]
#[test]
fn termination_during_replay_is_failure_and_runs_cleanup() {
    let c = Case::new("delay_ms = 1000");
    c.legacy(&[(2, ASK, "rx", "/user/counter")]);
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    let mut child = start(&args, c.dir.path());
    let end = Instant::now() + Duration::from_secs(5);
    while !c.journal().contains("handling 2") {
        if Instant::now() >= end {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("actor never entered");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    assert!(!wait(&mut child, c.dir.path()));
    let report = c.read();
    assert_eq!(report["status"], "failed");
    assert_eq!(report["replay_completed"], false);
    assert!(report["delivered"].is_null());
    assert!(c.journal().contains("stopped"));
}

#[test]
fn unknown_later_codec_prevents_all_business_delivery() {
    let c = Case::new("");
    c.legacy(&[
        (2, ASK, "rx", "/user/counter"),
        (3, ASK, "rx", "/user/counter"),
    ]);
    let text = std::fs::read_to_string(&c.input).unwrap();
    let mut lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines[1]["type_key"] = "bin:unknown.Type#v1".into();
    std::fs::write(
        &c.input,
        lines
            .iter()
            .map(|line| serde_json::to_string(line).unwrap() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(!run(&args, c.dir.path()));
    assert_eq!(c.read()["delivered"], 0);
    assert!(!c.journal().contains("handling"));
    assert!(c.journal().contains("stopped"));
}
#[test]
fn invalid_manifest_produces_failure_report_without_claiming_progress() {
    let c = Case::new("");
    c.legacy(&[(2, ASK, "rx", "/user/counter")]);
    std::fs::write(&c.manifest, "not valid toml").unwrap();
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    assert!(!run(&args, c.dir.path()));
    let report = c.read();
    assert_eq!(report["status"], "failed");
    assert!(report["delivered"].is_null());
    assert!(report["replies"].is_null());
    assert!(c.journal().is_empty());
}
fn generic(args: &[String], dir: &Path, ready: Option<&Path>) -> OwnedChild {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_parrot-app"));
    cmd.args(args)
        .stdout(Stdio::from(output(dir.join("stdout.log"))))
        .stderr(Stdio::from(output(dir.join("stderr.log"))));
    cmd.env_remove("PARROT_APP_READY_FILE");
    if let Some(ready) = ready {
        cmd.env("PARROT_APP_READY_FILE", ready);
    }
    OwnedChild(cmd.spawn().unwrap())
}
fn proxy_manifest(path: &Path) {
    let config = parrot_app::durable::CloudProxyConfig {
        directory: path.parent().unwrap().join("outbox"),
        receiver: "tcp://unused-peer@127.0.0.1:9".into(),
        path: "/sink".into(),
        session: "replay-fixture".into(),
        ..Default::default()
    };
    let mut manifest: parrot_app::AppManifest = toml::from_str("name = 'generic-replay'\nversion = '1.0.0'\n[[components]]\nname = 'proxy'\nengine = 'parrot'\nartifact = { Props = { factory = 'parrot.cloud-proxy' } }\n[components.hooks]\non_start = '.'\non_stop = '.'\n[config_overlay.remote.node]\nbind = '127.0.0.1:0'\n").unwrap();
    manifest.components[0].config = Some(
        toml::Value::try_from(config)
            .unwrap()
            .as_table()
            .unwrap()
            .clone(),
    );
    std::fs::write(path, toml::to_string(&manifest).unwrap()).unwrap();
}
#[test]
fn shipped_generic_binary_accepts_replay_and_complete_empty_capture() {
    let c = Case::new("");
    proxy_manifest(&c.manifest);
    parrot_app::debug::Recording::start(&c.input, Default::default(), Default::default())
        .unwrap()
        .finish()
        .unwrap();
    let mut child = generic(&c.args(), c.dir.path(), None);
    assert!(wait(&mut child, c.dir.path()));
    let report = c.read();
    assert_eq!(report["status"], "succeeded");
    assert_eq!(report["delivered"], 0);
    assert!(report["replies"].as_array().unwrap().is_empty());
}
#[cfg(unix)]
#[test]
fn shipped_run_command_keeps_readiness_recording_and_signal_cleanup() {
    let c = Case::new("");
    proxy_manifest(&c.manifest);
    let ready = c.dir.path().join("ready");
    let args = vec![
        "run".into(),
        "--manifest".into(),
        c.manifest.to_str().unwrap().into(),
        "--record".into(),
        c.input.to_str().unwrap().into(),
    ];
    let mut child = generic(&args, c.dir.path(), Some(&ready));
    let end = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < end, "readiness never published");
        std::thread::sleep(Duration::from_millis(5));
    }
    let endpoint = std::fs::read_to_string(&ready).unwrap();
    assert!(std::net::TcpStream::connect(&endpoint).is_ok());
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    assert!(wait(&mut child, c.dir.path()));
    assert!(std::net::TcpStream::connect(&endpoint).is_err());
    parrot_app::debug::ReplayLog::load_capture(&c.input).unwrap();
}

#[test]
fn shipped_generic_binary_replays_builtin_component_through_real_codec() {
    let c = Case::new("");
    proxy_manifest(&c.manifest);
    let mut sink = FileSink::open(&c.input).unwrap();
    let payload =
        parrot_api::message::serde_remote_serialize(&parrot_app::durable::CloudProxyInspect)
            .unwrap();
    sink.write(&record_sink(
        0,
        1,
        "rx",
        ASK,
        "/user/proxy",
        "bin:parrot.app.cloud-proxy.inspect#v1",
        &payload,
    ))
    .unwrap();
    sink.finish().unwrap();
    let mut args = c.args();
    args.push("--allow-legacy-log".into());
    let mut child = generic(&args, c.dir.path(), None);
    assert!(wait(&mut child, c.dir.path()));
    let report = c.read();
    assert_eq!(report["status"], "succeeded");
    assert_eq!(report["delivered"], 1);
    let reply = &report["replies"][0];
    assert_eq!(reply["type_key"], "bin:parrot.app.cloud-proxy.status#v1");
    let hex = reply["payload_hex"].as_str().unwrap();
    assert_eq!(hex.len() % 2, 0);
    let bytes: Vec<_> = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect();
    let status: parrot_app::durable::CloudProxyStatus =
        parrot_api::message::serde_remote_deserialize(&bytes).unwrap();
    assert_eq!(status.pending, 0);
    assert_eq!(status.last_enqueued, 0);
    assert_eq!(status.attempts, 0);
}
