use parrot_app::manifest::{AppManifest, ArtifactRef, ComponentSpec, EngineKind, InstancePolicy};
use parrot_app::orchestrator::{
    AppSupervisor, DesiredStateRecord, DesiredStateStore, FakeClock, MemStateStore, ObservedState,
    ReconcileAction,
};
use parrot_remote::admin_v2::ComponentStateReport;
use std::sync::Arc;

fn spec(name: &str, instances: InstancePolicy) -> ComponentSpec {
    ComponentSpec {
        name: name.into(),
        engine: EngineKind::Parrot,
        artifact: ArtifactRef::Props {
            factory: "test.echo".into(),
        },
        alt_artifact: None,
        instances,
        placement: Default::default(),
        upgrade: Default::default(),
        deps: vec![],
        config: None,
        hooks: Default::default(),
    }
}
fn manifest(components: Vec<ComponentSpec>) -> AppManifest {
    AppManifest {
        name: "state-contract".into(),
        version: "1.0.0".into(),
        components,
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: None,
    }
}
fn supervisor(m: AppManifest) -> AppSupervisor {
    AppSupervisor::new(
        m,
        Arc::new(MemStateStore::new()),
        Arc::new(FakeClock::new(0)),
    )
    .unwrap()
}
fn report(path: &str, state: &str) -> ComponentStateReport {
    ComponentStateReport {
        path: path.into(),
        state: state.into(),
        version: "1.0.0".into(),
    }
}
fn observed(rows: Vec<ComponentStateReport>) -> ObservedState {
    let mut o = ObservedState::default();
    o.components.insert("a".into(), rows);
    o
}

#[test]
fn partial_pool_health_never_reports_convergence() {
    let s = supervisor(manifest(vec![spec("a", InstancePolicy::Pool(2))]));
    for state in ["starting", "stopped", "failed", "draining"] {
        let result = s.reconcile_once(&observed(vec![
            report("/user/a-0", "running"),
            report("/user/a-1", state),
        ]));
        assert!(
            !result.converged,
            "partially unhealthy pool reported convergence: {state}"
        );
        assert!(!result.actions.is_empty());
    }
}

#[test]
fn duplicate_or_foreign_paths_cannot_substitute_for_expected_instances() {
    let s = supervisor(manifest(vec![spec("a", InstancePolicy::Pool(2))]));
    for path in [
        "/user/a-0",
        "/user/b-1",
        "/user/a-01",
        "/user/a-1/child",
        "parrot://other/user/a-1",
    ] {
        assert!(
            !s.reconcile_once(&observed(vec![
                report("/user/a-0", "running"),
                report(path, "running")
            ]))
            .converged,
            "invalid report accepted: {path}"
        );
    }
}

#[test]
fn same_version_configuration_change_is_an_upgrade() {
    let mut m = manifest(vec![spec("a", InstancePolicy::Singleton)]);
    let mut s = supervisor(m.clone());
    m.components[0].config = Some(toml::from_str("business_limit = 7").unwrap());
    s.submit(m).unwrap();
    assert!(
        s.upgrade_in_flight("a").is_some(),
        "config-only change disappeared from upgrade tracking"
    );
    assert!(
        !s.reconcile_once(&observed(vec![report("/user/a", "running")]))
            .converged
    );
}

#[test]
fn absent_report_does_not_bypass_in_flight_upgrade_ownership() {
    let mut m = manifest(vec![spec("a", InstancePolicy::Singleton)]);
    let mut s = supervisor(m.clone());
    m.components[0].artifact = ArtifactRef::Props {
        factory: "v2".into(),
    };
    s.submit(m).unwrap();
    let result = s.reconcile_once(&ObservedState::default());
    assert!(!result.converged);
    assert!(
        result
            .actions
            .iter()
            .all(|a| matches!(a, ReconcileAction::Wait { .. })),
        "reconciler raced active rollout: {:?}",
        result.actions
    );
}

#[test]
fn restart_recovers_committed_manifest_instead_of_stale_bootstrap() {
    let initial = manifest(vec![spec("a", InstancePolicy::Singleton)]);
    let store = Arc::new(MemStateStore::new());
    let mut s =
        AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0))).unwrap();
    let mut latest = initial.clone();
    latest.version = "2.0.0".into();
    s.submit(latest.clone()).unwrap();
    drop(s);
    let recovered = AppSupervisor::new(initial, store, Arc::new(FakeClock::new(0))).unwrap();
    assert_eq!(
        recovered.desired(),
        &latest,
        "restart discarded committed desired state"
    );
}

#[test]
fn stale_or_reused_revision_cannot_overwrite_store() {
    let store = MemStateStore::new();
    let first = manifest(vec![spec("a", InstancePolicy::Singleton)]);
    let record = DesiredStateRecord {
        mirrors: Default::default(),
        revision: 1,
        manifest: first.clone(),
        pending: Default::default(),
    };
    store.commit(None, &record).unwrap();
    let mut conflicting = record;
    conflicting.manifest.version = "9.0.0".into();
    assert!(
        store.commit(Some(1), &conflicting).is_err(),
        "same revision overwrote committed state"
    );
    assert_eq!(store.latest().unwrap().unwrap().manifest, first);
}

#[test]
fn revisions_and_pending_ownership_survive_restart_and_late_completions() {
    let initial = manifest(vec![spec("a", InstancePolicy::Singleton)]);
    let store = Arc::new(MemStateStore::new());
    let mut s =
        AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0))).unwrap();
    assert_eq!(s.revision(), 1);
    let mut update = initial.clone();
    update.components[0].config = Some(toml::from_str("value = 1").unwrap());
    s.submit(update.clone()).unwrap();
    let first = s.upgrade_generation("a").unwrap();
    update.components[0].config = Some(toml::from_str("value = 2").unwrap());
    s.submit(update).unwrap();
    let second = s.upgrade_generation("a").unwrap();
    assert!(second > first);
    drop(s);
    let mut recovered =
        AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0))).unwrap();
    assert_eq!(recovered.upgrade_generation("a"), Some(second));
    assert!(!recovered.finish_upgrade("a", first).unwrap());
    assert_eq!(recovered.upgrade_generation("a"), Some(second));
    assert!(recovered.finish_upgrade("a", second).unwrap());
    assert_eq!(recovered.revision(), second + 1);
    drop(recovered);
    let recovered = AppSupervisor::new(initial, store, Arc::new(FakeClock::new(0))).unwrap();
    assert!(recovered.upgrade_in_flight("a").is_none());
    assert_eq!(recovered.revision(), second + 1);
}

#[test]
fn all_deployment_inputs_trigger_upgrade_and_unchanged_nan_is_idempotent() {
    for change in 0..6 {
        let initial = manifest(vec![spec("a", InstancePolicy::Singleton)]);
        let mut s = supervisor(initial.clone());
        let mut update = initial;
        match change {
            0 => update.version = "2.0.0".into(),
            1 => update.components[0].config = Some(toml::from_str("v=nan\nz=-0.0").unwrap()),
            2 => update.config_overlay = Some(toml::from_str("[thread]\nnum_workers=2").unwrap()),
            3 => update.components[0].hooks.on_start = Some(".".into()),
            4 => update.components[0].instances = InstancePolicy::Pool(2),
            _ => update.wiring.push(parrot_app::WireSpec {
                from: "a:/user/a/query".into(),
                to: "a:/user/a".into(),
                qos: "lan".into(),
            }),
        }
        s.submit(update.clone()).unwrap();
        assert!(s.upgrade_in_flight("a").is_some(), "change {change}");
        let revision = s.revision();
        s.submit(update).unwrap();
        assert_eq!(s.revision(), revision);
    }
}

#[test]
fn deleted_component_drops_pending_and_new_component_can_deploy() {
    let mut initial = manifest(vec![
        spec("a", InstancePolicy::Singleton),
        spec("b", InstancePolicy::Singleton),
    ]);
    let mut s = supervisor(initial.clone());
    initial.components[0].config = Some(toml::from_str("v=1").unwrap());
    s.submit(initial.clone()).unwrap();
    assert!(s.upgrade_in_flight("a").is_some());
    initial.components.remove(0);
    s.submit(initial).unwrap();
    assert!(s.upgrade_in_flight("a").is_none());
    let r = s.reconcile_once(&observed(vec![report("/user/a", "running")]));
    assert!(r
        .actions
        .contains(&ReconcileAction::Stop { comp: "a".into() }));
}

#[test]
fn concurrent_store_commits_have_exactly_one_winner_and_stale_supervisor_waits() {
    let initial = manifest(vec![spec("a", InstancePolicy::Singleton)]);
    let store = Arc::new(MemStateStore::new());
    let mut stale =
        AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0))).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let store = store.clone();
            let barrier = barrier.clone();
            let mut next = initial.clone();
            next.version = format!("{}.0.0", i + 2);
            std::thread::spawn(move || {
                barrier.wait();
                store.commit(
                    Some(1),
                    &DesiredStateRecord {
                        mirrors: Default::default(),
                        revision: 2,
                        manifest: next,
                        pending: Default::default(),
                    },
                )
            })
        })
        .collect();
    barrier.wait();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap().is_ok() as usize)
            .sum::<usize>(),
        1
    );
    let r = stale.reconcile_once(&ObservedState::default());
    assert!(!r.converged);
    assert!(r
        .actions
        .iter()
        .all(|a| matches!(a, ReconcileAction::Wait { .. })));
    assert!(stale.submit(initial).is_err());
    stale.refresh().unwrap();
    assert_eq!(stale.revision(), 2);
    assert!(stale
        .reconcile_once(&ObservedState::default())
        .actions
        .iter()
        .any(|a| matches!(a, ReconcileAction::Deploy { .. })));
}

#[test]
fn known_empty_links_wrong_engine_and_unverifiable_placement_never_deploy() {
    let initial = manifest(vec![spec("a", InstancePolicy::Singleton)]);
    let s = supervisor(initial.clone());
    for live in [vec![], vec!["ray-gw-1".into()], vec!["parrotx-gw-1".into()]] {
        let o = ObservedState {
            live_nodes: live,
            links_known: true,
            ..Default::default()
        };
        let r = s.reconcile_once(&o);
        assert!(!r.converged);
        assert!(r
            .actions
            .iter()
            .all(|a| matches!(a, ReconcileAction::Wait { .. })));
    }
    let mut restricted = initial;
    restricted.components[0].placement.role = Some("hub".into());
    let s = supervisor(restricted);
    let o = ObservedState {
        live_nodes: vec!["parrot-hub-gw-1".into()],
        links_known: true,
        ..Default::default()
    };
    assert!(s
        .reconcile_once(&o)
        .actions
        .iter()
        .all(|a| matches!(a, ReconcileAction::Wait { .. })));
}

#[test]
fn incomplete_dependencies_block_dependents_and_partial_cohort_replacement_stops_first() {
    let mut b = spec("b", InstancePolicy::Singleton);
    b.deps = vec!["a".into()];
    let s = supervisor(manifest(vec![b, spec("a", InstancePolicy::Pool(2))]));
    let r = s.reconcile_once(&observed(vec![report("/user/a-0", "running")]));
    assert!(matches!(&r.actions[0],ReconcileAction::Wait{comp,..} if comp=="b"));
    assert_eq!(
        &r.actions[1..],
        &[
            ReconcileAction::Drain { comp: "a".into() },
            ReconcileAction::Stop { comp: "a".into() },
            ReconcileAction::Deploy {
                comp: "a".into(),
                node: "parrot-gw-1".into()
            }
        ]
    );
}

#[test]
fn record_recovery_rejects_foreign_identity_invalid_pending_and_revision_exhaustion() {
    struct Fixed(DesiredStateRecord);
    impl DesiredStateStore for Fixed {
        fn latest(&self) -> Result<Option<DesiredStateRecord>, String> {
            Ok(Some(self.0.clone()))
        }
        fn commit(&self, _: Option<u64>, _: &DesiredStateRecord) -> Result<(), String> {
            panic!("invalid recovered state must never commit")
        }
    }
    let initial = manifest(vec![spec("a", InstancePolicy::Singleton)]);
    let record = DesiredStateRecord {
        mirrors: Default::default(),
        revision: u64::MAX,
        manifest: initial.clone(),
        pending: Default::default(),
    };
    let mut s = AppSupervisor::new(
        initial.clone(),
        Arc::new(Fixed(record.clone())),
        Arc::new(FakeClock::new(0)),
    )
    .unwrap();
    let mut changed = initial.clone();
    changed.version = "2.0.0".into();
    assert!(s.submit(changed).unwrap_err().contains("exhausted"));
    let mut foreign = initial.clone();
    foreign.name = "foreign".into();
    assert!(AppSupervisor::new(
        foreign,
        Arc::new(Fixed(record.clone())),
        Arc::new(FakeClock::new(0))
    )
    .is_err());
    let mut invalid = record;
    invalid.pending.insert(
        "ghost".into(),
        parrot_app::orchestrator::PendingUpgrade {
            generation: 1,
            policy: Default::default(),
        },
    );
    assert!(AppSupervisor::new(
        initial,
        Arc::new(Fixed(invalid)),
        Arc::new(FakeClock::new(0))
    )
    .is_err());
}

#[cfg(unix)]
mod disk {
    use super::*;
    use parrot_app::orchestrator::FileStateStore;
    struct KillOnDrop(std::process::Child);
    impl std::ops::Deref for KillOnDrop {
        type Target = std::process::Child;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl std::ops::DerefMut for KillOnDrop {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn open(path: &std::path::Path) -> Arc<FileStateStore> {
        Arc::new(FileStateStore::open(path, Default::default()).unwrap())
    }
    fn configured() -> AppManifest {
        let mut m = manifest(vec![spec("a", InstancePolicy::Singleton)]);
        m.components[0].config = Some(
            toml::from_str("value=nan\nnegative_zero=-0.0\nwhen=2026-10-15T00:00:00Z").unwrap(),
        );
        m
    }
    #[test]
    fn file_store_preserves_lossless_config_pending_generation_and_completion() {
        let dir = tempfile::tempdir().unwrap();
        let initial = configured();
        let store = open(dir.path());
        assert!(FileStateStore::open(dir.path(), Default::default()).is_err());
        let mut s = AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0)))
            .unwrap();
        let mut update = initial.clone();
        update.version = "2.0.0".into();
        s.submit(update.clone()).unwrap();
        drop(s);
        drop(store);
        let store = open(dir.path());
        let mut s = AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0)))
            .unwrap();
        assert_eq!(s.desired().to_toml().unwrap(), update.to_toml().unwrap());
        let config = s.desired().components[0].config.as_ref().unwrap();
        assert_eq!(
            config["negative_zero"].as_float().unwrap().to_bits(),
            (-0.0f64).to_bits()
        );
        assert!(config["value"].as_float().unwrap().is_nan());
        assert!(config["when"].as_datetime().is_some());
        let generation = s.upgrade_generation("a").unwrap();
        assert_eq!(generation, 2);
        s.finish_upgrade("a", generation).unwrap();
        drop(s);
        drop(store);
        let s = AppSupervisor::new(initial, open(dir.path()), Arc::new(FakeClock::new(0))).unwrap();
        assert_eq!(s.revision(), 3);
        assert!(s.upgrade_in_flight("a").is_none());
    }
    #[test]
    fn corrupt_snapshot_and_interrupted_bootstrap_fail_closed_without_reset() {
        let dir = tempfile::tempdir().unwrap();
        let initial = configured();
        drop(
            AppSupervisor::new(
                initial.clone(),
                open(dir.path()),
                Arc::new(FakeClock::new(0)),
            )
            .unwrap(),
        );
        let path = dir.path().join("desired.state");
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        let next = dir.path().join("desired.next");
        std::fs::write(&next, b"preserve evidence").unwrap();
        assert!(FileStateStore::open(dir.path(), Default::default()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read(&next).unwrap(), b"preserve evidence");
        let empty = tempfile::tempdir().unwrap();
        std::fs::write(empty.path().join("desired.next"), b"incomplete").unwrap();
        assert!(FileStateStore::open(empty.path(), Default::default()).is_err());
        assert!(!empty.path().join("desired.state").exists());
    }
    #[test]
    fn file_write_failure_invalidates_owner_and_reopen_recovers_last_commit() {
        let dir = tempfile::tempdir().unwrap();
        let initial = configured();
        let store = open(dir.path());
        let mut s = AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0)))
            .unwrap();
        let before = std::fs::read(dir.path().join("desired.state")).unwrap();
        std::fs::create_dir(dir.path().join("desired.next")).unwrap();
        let mut update = initial.clone();
        update.version = "2.0.0".into();
        assert!(s.submit(update).is_err());
        assert_eq!(s.revision(), 1);
        assert!(store.latest().is_err());
        assert!(!s.reconcile_once(&ObservedState::default()).converged);
        assert_eq!(
            std::fs::read(dir.path().join("desired.state")).unwrap(),
            before
        );
        drop(s);
        drop(store);
        assert!(FileStateStore::open(dir.path(), Default::default()).is_err()); // never delete a directory pretending to be a temporary file
        std::fs::remove_dir(dir.path().join("desired.next")).unwrap();
        let s = AppSupervisor::new(
            initial.clone(),
            open(dir.path()),
            Arc::new(FakeClock::new(0)),
        )
        .unwrap();
        assert_eq!(s.desired().to_toml().unwrap(), initial.to_toml().unwrap());
        assert_eq!(s.revision(), 1);
    }
    #[test]
    fn symlink_and_hardlink_storage_inputs_are_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::NamedTempFile::new().unwrap();
        symlink(target.path(), dir.path().join(".parrot-desired.lock")).unwrap();
        assert!(FileStateStore::open(dir.path(), Default::default()).is_err());
        std::fs::remove_file(dir.path().join(".parrot-desired.lock")).unwrap();
        std::fs::hard_link(target.path(), dir.path().join(".parrot-desired.lock")).unwrap();
        assert!(FileStateStore::open(dir.path(), Default::default()).is_err());
        assert!(std::fs::read(target.path()).unwrap().is_empty());
        let alias = dir.path().join("alias");
        symlink(dir.path(), &alias).unwrap();
        assert!(FileStateStore::open(alias, Default::default()).is_err());
    }
    #[test]
    fn sigkill_after_successful_commit_preserves_revision_and_pending_rollout() {
        if let Some(dir) = std::env::var_os("PARROT_DESIRED_CHILD_DIRECTORY") {
            let dir = std::path::PathBuf::from(dir);
            let initial = configured();
            let mut s =
                AppSupervisor::new(initial.clone(), open(&dir), Arc::new(FakeClock::new(0)))
                    .unwrap();
            let mut update = initial;
            update.version = "7.8.9".into();
            s.submit(update).unwrap();
            std::fs::write(dir.join("ready"), s.revision().to_string()).unwrap();
            loop {
                std::thread::park();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let log = std::fs::File::create(dir.path().join("child.log")).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "disk::sigkill_after_successful_commit_preserves_revision_and_pending_rollout",
                "--nocapture",
            ])
            .env("PARROT_DESIRED_CHILD_DIRECTORY", dir.path())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut child = KillOnDrop(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let ready = loop {
            if dir.path().join("ready").exists() {
                break true;
            }
            if child.try_wait().unwrap().is_some() || std::time::Instant::now() > deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let _ = child.kill();
        let status = child.wait().unwrap();
        assert!(
            ready,
            "child never committed: {}",
            std::fs::read_to_string(dir.path().join("child.log")).unwrap()
        );
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        let s = AppSupervisor::new(configured(), open(dir.path()), Arc::new(FakeClock::new(0)))
            .unwrap();
        assert_eq!(s.desired().version, "7.8.9");
        assert_eq!(s.revision(), 2);
        assert_eq!(s.upgrade_generation("a"), Some(2));
        assert!(!s.reconcile_once(&ObservedState::default()).converged);
    }
}

#[test]
fn placement_uses_exact_metadata_instead_of_node_name_substrings() {
    use parrot_app::orchestrator::ObservedNode;
    let mut c = spec("a", InstancePolicy::Singleton);
    c.placement.role = Some("hub".into());
    c.placement.realm = Some("west".into());
    c.placement.label = Some("ssd".into());
    let s = supervisor(manifest(vec![c]));
    let mut o = ObservedState {
        live_nodes: vec!["opaque-node".into(), "parrot-hub-west-ssd".into()],
        links_known: true,
        ..Default::default()
    };
    o.node_metadata.insert(
        "opaque-node".into(),
        ObservedNode {
            engine: EngineKind::Parrot,
            role: "hub".into(),
            realm: "west".into(),
            labels: vec!["ssd".into()],
        },
    );
    o.node_metadata.insert(
        "parrot-hub-west-ssd".into(),
        ObservedNode {
            engine: EngineKind::Ray,
            role: "hub".into(),
            realm: "west".into(),
            labels: vec!["ssd".into()],
        },
    );
    // Placement eligibility and an observed empty cohort are both required.
    o.components.insert("a".into(), vec![]);
    o.component_nodes.insert("a".into(), "opaque-node".into());
    assert_eq!(
        s.reconcile_once(&o).actions,
        vec![ReconcileAction::Deploy {
            comp: "a".into(),
            node: "opaque-node".into()
        }]
    );
    o.node_metadata.get_mut("opaque-node").unwrap().labels = vec!["ssd-extra".into()];
    assert!(s
        .reconcile_once(&o)
        .actions
        .iter()
        .all(|a| matches!(a, ReconcileAction::Wait { .. })));
}
