//! Executes the actual built ABI library and Wasm component through public hosts.
//! Build both with `make build-crawler-business` before running this test.
#[path = "../src/cli.rs"]
#[allow(dead_code)]
mod cli;
#[path = "../src/hub.rs"]
#[allow(dead_code)]
// This integration target exercises independently owned engines; assembled ownership is tested in the binary.
mod hub;
use crawler_lab_core::{Config, Tick};
use std::{path::PathBuf, process::Command};

fn forms() -> Vec<cli::Hub> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("python3")
        .arg(root.join("scripts/crawler_business_artifacts.py"))
        .arg("verify")
        .arg("--test-executable")
        .arg(std::env::current_exe().unwrap())
        .output()
        .expect("Python 3 is required to verify business build receipts");
    assert!(
        output.status.success(),
        "business artifact verification failed: {}. Build first with make build-crawler-business",
        String::from_utf8_lossy(&output.stderr)
    );
    let artifacts: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let path = |name: &str| artifacts[name]["path"].as_str().unwrap().to_owned();
    let digest = |name: &str| artifacts[name]["sha256"].as_str().unwrap().to_owned();
    vec![
        cli::Hub::Embedded,
        cli::Hub::Dylib {
            path: path("dylib"),
            digest: digest("dylib"),
        },
        cli::Hub::Wasm {
            path: path("wasm"),
            digest: digest("wasm"),
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_components_preserve_business_vectors_isolation_and_maximum_batch() {
    let mut expected: Option<Vec<Tick>> = None;
    for form in forms() {
        let mut hub = hub::BusinessHub::new(
            form,
            Config {
                pages: 1_000_000,
                depth: u16::MAX,
                fanout: 256,
                batch: 256,
            },
        )
        .await
        .unwrap();
        let first = hub
            .tick(&[(0, crawler_lab_core::url_of(0), 0)])
            .await
            .unwrap();
        assert_eq!((first.fetched, first.pages.len()), (1, 1));
        assert!(String::from_utf8(first.pages[0].1.clone())
            .unwrap()
            .contains("mtls inverted router raft receptionist throughput frontier"));
        assert_eq!(
            first.terms.iter().find(|x| x.0 == "parrot"),
            Some(&("parrot".into(), 0, 3))
        );
        let max = hub
            .tick(
                &(1..257)
                    .map(|id| (id, crawler_lab_core::url_of(id), 0))
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();
        assert_eq!((max.fetched, max.pages.len()), (257, 256));
        assert!(max.outlinks.len() > 60_000);
        let duplicate = hub
            .tick(&[(0, crawler_lab_core::url_of(0), 0)])
            .await
            .unwrap();
        assert!(duplicate.pages.is_empty());
        let actual = vec![first, max, duplicate];
        if let Some(expected) = &expected {
            assert_eq!(&actual, expected);
        } else {
            expected = Some(actual);
        }
        hub.close().await.unwrap();
    }
    // New component instances start with fresh visited sets and progress.
    for form in forms() {
        let mut hub = hub::BusinessHub::new(
            form,
            Config {
                depth: 0,
                ..Config::default()
            },
        )
        .await
        .unwrap();
        let tick = hub
            .tick(&[(0, crawler_lab_core::url_of(0), 0)])
            .await
            .unwrap();
        assert_eq!((tick.sequence, tick.fetched), (1, 1));
        assert!(tick.outlinks.is_empty());
        hub.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_instances_keep_state_and_configuration_separate() {
    for (form, other) in forms().into_iter().zip(forms()) {
        let mut first = hub::BusinessHub::new(form, Config::default())
            .await
            .unwrap();
        let mut second = hub::BusinessHub::new(
            other,
            Config {
                depth: 0,
                ..Config::default()
            },
        )
        .await
        .unwrap();
        let entries = [(0, crawler_lab_core::url_of(0), 0)];
        let a = first.tick(&entries).await.unwrap();
        let b = second.tick(&entries).await.unwrap();
        assert_eq!(a.pages, b.pages);
        assert!(!a.outlinks.is_empty() && b.outlinks.is_empty());
        assert!(first.tick(&entries).await.unwrap().pages.is_empty());
        first.close().await.unwrap();
        // Stopping one instance cannot drain or reset its peer.
        assert_eq!(
            second
                .tick(&[(1, crawler_lab_core::url_of(1), 0)])
                .await
                .unwrap()
                .fetched,
            2
        );
        second.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_artifact_digest_mismatch_is_rejected() {
    for mut form in forms().into_iter().skip(1) {
        match &mut form {
            cli::Hub::Dylib { digest, .. } | cli::Hub::Wasm { digest, .. } => {
                *digest = "0".repeat(64)
            }
            cli::Hub::Embedded => unreachable!(),
        }
        match hub::BusinessHub::new(form, Config::default()).await {
            Err(error) => assert!(error.to_lowercase().contains("digest"), "{error}"),
            Ok(hub) => {
                hub.close().await.unwrap();
                panic!("incorrect artifact digest accepted");
            }
        }
    }
}
