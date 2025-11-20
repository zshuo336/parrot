//! A4 辅助诊断：RemoteActorSystem 接受真实 erl 网关注册（隔离 AssemblingContext）。
#![cfg(feature = "host")]

use std::sync::Arc;

use parrot::thread::system::ThreadActorSystem;
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};

use parrot_app::host::HostLookup;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_accepts_erl_registration() {
    let ts = ThreadActorSystem::shared(Default::default());
    let sa: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let r = RemoteActorSystem::new(
        RemoteConfig::tcp("t-app", Some(sa)),
        Arc::new(HostLookup { ts: ts.clone() }),
    )
    .unwrap();
    r.start().await.unwrap();

    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let gateway_dir = repo_root.join("interop/erlang");
    let status = tokio::process::Command::new("erlc")
        .current_dir(&gateway_dir)
        .args([
            "parrot_gw.erl",
            "parrot_trace.erl",
            "parrot_component_runtime.erl",
            "parrot_beam_snapshot.erl",
            "parrot_beam_guest.erl",
            "parrot_beam_host.erl",
        ])
        .kill_on_drop(true)
        .status()
        .await
        .expect("Erlang compiler is required");
    assert!(status.success(), "gateway compilation failed: {status}");
    let mut cmd = tokio::process::Command::new("erl");
    cmd.args(["-noshell", "-pa"])
        .arg(&gateway_dir)
        .arg("-eval")
        .arg(format!(
            "parrot_gw:main([0, \"parrot={}\"])",
            r.local_addr().unwrap()
        ))
        .env("PARROT_NODE_ID", "erl-gw-1")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    let mut child = cmd.spawn().expect("spawn erl");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("inspect gateway process") {
            panic!("Erlang gateway exited before registration: {status}");
        }
        if r.links_snapshot().await.iter().any(|(node, _, status)| {
            node == "erl-gw-1" && status.get() == parrot_remote::node::NodeState::Connected
        }) {
            ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    child.kill().await.expect("kill and reap gateway");
    r.shutdown().await.unwrap();
    ts.shutdown_internal().await.unwrap();
    assert!(ok, "erl-gw-1 should register within 30s");
}

#[derive(Debug)]
struct BeamBudgetPayload(Vec<u8>);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot.test.BeamBudget#v1",
        type_id: std::any::TypeId::of::<BeamBudgetPayload>(),
        encode: |message: &parrot_api::types::BoxedMessage| Ok(message.downcast_ref::<BeamBudgetPayload>().ok_or("BeamBudgetPayload")?.0.clone()),
        decode: |bytes: &[u8]| Ok(Box::new(BeamBudgetPayload(bytes.to_vec())) as parrot_api::types::BoxedMessage),
    }
}

#[tokio::test]
#[ignore = "requires Erlang compiler and runtime"]
async fn real_beam_compiled_uri_rechecks_payload_budget_and_retires_on_rejection() {
    use parrot_app::host::{ApplicationLookup, GatewayRoutes, ProcessGatewayFactory};
    use parrot_app::{ApplicationLimits, ArtifactRef, DeployError, EngineKind, GatewayFactory};
    use parrot_remote::LocalLookup;
    use std::time::Duration;
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("gateway_budget_fixture.erl");
    std::fs::write(&source, b"-module(gateway_budget_fixture).\n-export([parrot_init/1,parrot_service/3]).\nparrot_init(C) -> {ok,C}.\nparrot_service(K,P,S) -> {reply,{K,P},S}.\n").unwrap();
    let spec = parrot_app::ComponentSpec {
        name: "compiled-budget".into(),
        engine: EngineKind::Erlang,
        artifact: ArtifactRef::Beam {
            app: "gateway_budget_fixture".into(),
            uri: None,
        },
        alt_artifact: None,
        instances: parrot_app::InstancePolicy::Pool(1),
        placement: Default::default(),
        upgrade: Default::default(),
        deps: vec![],
        config: Some(toml::Table::from_iter([(
            "padding".into(),
            toml::Value::String("a".repeat(256)),
        )])),
        hooks: Default::default(),
    };
    let initial_payload = spec.deployment("1.0.0").unwrap();
    let wire_bytes = serde_json::to_vec(&initial_payload).unwrap().len() as u64;
    assert!(serde_json::to_vec(&(&spec, "1.0.0")).unwrap().len() as u64 <= wire_bytes);
    let routes = Arc::new(GatewayRoutes::default());
    let engine = ThreadActorSystem::shared(Default::default());
    let lookup = Arc::new(ApplicationLookup {
        ts: engine.clone(),
        routes: routes.clone(),
    });
    let remote = parrot_remote::RemoteActorSystem::new(
        parrot_remote::RemoteConfig::tcp("beam-budget-host", Some("127.0.0.1:0".parse().unwrap())),
        lookup.clone(),
    )
    .unwrap();
    remote.start().await.unwrap();
    let factory = ProcessGatewayFactory::new_with_routes(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf(),
        remote.clone(),
        routes,
    )
    .with_beam_sources(std::collections::HashMap::from([(
        spec.name.clone(),
        source,
    )]))
    .with_application_limits(ApplicationLimits {
        max_metadata_bytes: wire_bytes,
        ..Default::default()
    });
    let paths = ["/user/compiled-budget-0".into()];
    let result = factory.start_gateway(&spec, &paths, "1.0.0").await;
    let rejected = matches!(&result, Err(DeployError::Config(error)) if error.contains("metadata bytes budget exceeded"));
    let diagnostic = match &result {
        Ok(actors) => format!("unexpected success with {} actors", actors.len()),
        Err(error) => error.to_string().chars().take(512).collect(),
    };
    if !rejected {
        factory.procs().kill_all().await.unwrap();
        remote.shutdown().await.unwrap();
        engine.shutdown_internal().await.unwrap();
    }
    assert!(
        rejected,
        "compiled URI must be checked before the deploy RPC: {diagnostic}"
    );
    assert_eq!(factory.procs().child_count(), 0);
    assert_eq!(factory.procs().budget().in_use(), 0);
    assert!(factory.component_node(&spec.name).is_none());
    assert!(lookup.lookup(&paths[0]).await.is_none());
    let relaxed = factory.with_application_limits(ApplicationLimits::default());
    let actors = relaxed.start_gateway(&spec, &paths, "1.0.0").await.unwrap();
    assert_eq!(actors.len(), 1);
    let bytes = vec![0, 255];
    let reply = actors[0]
        .send_with_timeout(
            Box::new(BeamBudgetPayload(bytes.clone())),
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert_eq!(reply.downcast_ref::<BeamBudgetPayload>().unwrap().0, bytes);
    relaxed.stop_gateway(&spec).await.unwrap();
    assert_eq!(relaxed.procs().child_count(), 0);
    assert_eq!(relaxed.procs().budget().in_use(), 0);
    assert!(lookup.lookup(&paths[0]).await.is_none());
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}
