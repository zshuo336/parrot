//! G2/M5（DEV_09 §3.7）：`test_assemble` helper 验收测试。
//!
//! 形态：一行装配 + refs/started/paths 断言 + teardown 逆序停用。
//! 与 app_run_scenarios 手写样板行为等价（公共收敛点——后续装配类
//! 测试迁移目标位）。

#![cfg(feature = "host")]

use parrot::thread::config::ThreadActorConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture};
use parrot_app::host::ThreadPropsSpawner;
use parrot_app::manifest::{AppManifest, ArtifactRef, ComponentSpec, EngineKind, InstancePolicy};
use parrot_app::planner::LocalTopology;
use parrot_app::test_support::test_assemble_default_cfg;
use std::sync::Arc;

// ── 测试组件（PropsFactory——app.m5 命名空间）──

pub struct M5Echo;

impl Actor for M5Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: parrot_api::types::BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn spawn_m5_echo(
    ts: Arc<ThreadActorSystem>,
    path: String,
    _config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let r = ts
            .spawn_at(M5Echo, &path, None, ThreadActorConfig::default())
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(format!("spawn: {e}")))?;
        Ok(Box::new(r) as BoxedActorRef)
    })
}

/// Each test owns its engine; factories receive that engine explicitly.
fn thread_system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(Default::default())
}

parrot_api::message::inventory::submit! {
    parrot_app::host::ThreadPropsFactory {
        name: "app.m5.echo",
        spawn: spawn_m5_echo,
    }
}

// ── 无网关部署面（纯 parrot 组件——gateway 分支不可达）──

struct NoGateway;
#[async_trait::async_trait]
impl parrot_app::assemble::GatewayFactory for NoGateway {
    async fn configure(
        &self,
        _: &parrot_config::Resolved,
    ) -> Result<(), parrot_app::assemble::DeployError> {
        Ok(())
    }
    async fn start_gateway(
        &self,
        _comp: &ComponentSpec,
        _paths: &[String],
        _version: &str,
    ) -> Result<Vec<BoxedActorRef>, parrot_app::assemble::DeployError> {
        Err(parrot_app::assemble::DeployError::Engine(
            "no gateway in m5 test".into(),
        ))
    }
    async fn stop_gateway(
        &self,
        _comp: &ComponentSpec,
    ) -> Result<(), parrot_app::assemble::DeployError> {
        Ok(())
    }
}

fn spec(name: &str) -> ComponentSpec {
    ComponentSpec {
        name: name.into(),
        engine: EngineKind::Parrot,
        artifact: ArtifactRef::Props {
            factory: "app.m5.echo".into(),
        },
        alt_artifact: None,
        instances: InstancePolicy::Singleton,
        placement: Default::default(),
        upgrade: Default::default(),
        deps: vec![],
        config: None,
        hooks: Default::default(),
    }
}

fn manifest(comps: Vec<ComponentSpec>) -> AppManifest {
    AppManifest {
        name: "m5-app".into(),
        version: "1.0.0".into(),
        components: comps,
        wiring: vec![],
        mirrors: Vec::new(),
        config_overlay: None,
    }
}

async fn assemble(comps: Vec<ComponentSpec>) -> parrot_app::test_support::Assembled {
    let ts = thread_system();
    test_assemble_default_cfg(
        &manifest(comps),
        Box::new(LocalTopology),
        Box::new(NoGateway),
        Box::new(ThreadPropsSpawner::new(ts)),
    )
    .await
    .expect("test_assemble")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_component_assembles_and_echoes() {
    let mut a = assemble(vec![spec("only")]).await;
    assert_eq!(a.started(), &["only".to_string()]);
    let r = a.first("only").expect("ref");
    // 原样回声（BoxedMessage 透传——M5Echo receive_message 直接返回 msg）
    let reply = r
        .send(Box::new("ping".to_string()) as parrot_api::types::BoxedMessage)
        .await
        .expect("send");
    let v = reply.downcast::<String>().expect("String echo");
    assert_eq!(*v, "ping");
    a.teardown().await.expect("teardown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependency_order_in_started() {
    let mut b = spec("b");
    b.deps = vec!["a".into()];
    let mut c = spec("c");
    c.deps = vec!["b".into()];
    let mut a = assemble(vec![c, b, spec("a")]).await; // 乱序声明
                                                       // 装配序 = 依赖序（a → b → c——与声明序无关）
    assert_eq!(
        a.started(),
        &["a".to_string(), "b".to_string(), "c".to_string()]
    );
    a.teardown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paths_default_singleton() {
    let mut a = assemble(vec![spec("solo")]).await;
    assert_eq!(a.paths("solo"), Some(&["/user/solo".to_string()][..]));
    a.teardown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_factory_rolls_back_atomic() {
    // 未注册工厂 → 装配失败（fine 先成功——ctx 内部回卷后返回 Err）
    let mut bad = spec("bad");
    bad.artifact = ArtifactRef::Props {
        factory: "app.m5.noexist".into(),
    };
    let mut never = spec("never");
    never.deps = vec!["bad".into()];
    let r = test_assemble_default_cfg(
        &manifest(vec![spec("fine"), bad, never]),
        Box::new(LocalTopology),
        Box::new(NoGateway),
        Box::new(ThreadPropsSpawner::new(thread_system())),
    )
    .await;
    assert!(r.is_err(), "未注册工厂必须失败");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_stops_actors() {
    let mut a = assemble(vec![spec("victim")]).await;
    let r = a.first("victim").expect("ref");
    // 停用前可通信
    r.send(Box::new("hi".to_string()) as parrot_api::types::BoxedMessage)
        .await
        .expect("alive before teardown");
    a.teardown().await.unwrap();
    // 停用后 mailbox 已关闭——消息必失败（stop 语义：close + drain）
    let mut dead = false;
    for _ in 0..50 {
        if r.send(Box::new("hi".to_string()) as parrot_api::types::BoxedMessage)
            .await
            .is_err()
        {
            dead = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(dead, "teardown 后 actor 必须已停（消息不可达）");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_instance_paths() {
    let mut pool = spec("pool");
    pool.instances = InstancePolicy::Pool(3);
    let mut a = assemble(vec![pool]).await;
    let paths = a.paths("pool").expect("paths");
    assert_eq!(paths.len(), 3);
    assert!(paths.contains(&"/user/pool-0".to_string()));
    assert!(paths.contains(&"/user/pool-2".to_string()));
    a.teardown().await.unwrap();
}

#[test]
fn local_topology_still_works_directly() {
    // helper 与 LocalTopology 兼容（未强制自定拓扑）
    let m = manifest(vec![spec("x")]);
    let p = parrot_app::planner::plan(&m, &LocalTopology).expect("plan");
    assert_eq!(p.order.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplied_engine_owns_spawned_actors() {
    let engine = ThreadActorSystem::shared(Default::default());
    let mut assembled = test_assemble_default_cfg(
        &manifest(vec![spec("engine-owner")]),
        Box::new(LocalTopology),
        Box::new(NoGateway),
        Box::new(ThreadPropsSpawner::new(engine.clone())),
    )
    .await
    .unwrap();
    let owned = engine.get_actor_ref("/user/engine-owner").is_some();
    assembled.teardown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    assert!(
        owned,
        "PropsFactory ignored the supplied engine and spawned on global state"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_admission_budget_allows_real_pool_messages_and_owned_teardown() {
    use parrot_app::assemble::{AssemblingContext, LocalDeployer};
    use parrot_app::limits::ApplicationLimits;
    let limits = ApplicationLimits {
        max_instances: 3,
        ..Default::default()
    };
    let mut pool = spec("admitted");
    pool.instances = InstancePolicy::Pool(3);
    let manifest = manifest(vec![pool]);
    let plan = parrot_app::planner::plan_with_limits(&manifest, &LocalTopology, &limits).unwrap();
    let engine = thread_system();
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new_with_limits(limits);
    context
        .assemble(&plan, &deployer, &manifest, &Default::default())
        .await
        .unwrap();
    let references: Vec<_> = context
        .component_refs("admitted")
        .unwrap()
        .iter()
        .map(|r| r.clone_boxed())
        .collect();
    assert_eq!(references.len(), 3);
    for (i, actor) in references.iter().enumerate() {
        let request = format!("message to instance {i}");
        let reply = actor
            .send(Box::new(request.clone()))
            .await
            .unwrap()
            .downcast::<String>()
            .unwrap();
        assert_eq!(*reply, request);
        assert!(engine
            .get_actor_ref(&format!("/user/admitted-{i}"))
            .is_some());
    }
    context.teardown(&deployer).await.unwrap();
    for (i, actor) in references.iter().enumerate() {
        assert!(actor
            .send(Box::new("after stop".to_string()))
            .await
            .is_err());
        assert!(engine
            .get_actor_ref(&format!("/user/admitted-{i}"))
            .is_none());
    }
    engine.shutdown_internal().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_repairs_a_real_partial_pool_and_executes_recovered_desired_state() {
    use parrot_app::assemble::{AssemblingContext, LocalDeployer, ParrotSpawner};
    use parrot_app::orchestrator::{
        AppSupervisor, FakeClock, FileStateStore, ObservedState, ReconcileAction,
    };
    use parrot_remote::admin_v2::ComponentStateReport;
    async fn observe(engine: &ThreadActorSystem, paths: &[String], version: &str) -> ObservedState {
        let mut reports = Vec::new();
        for path in paths {
            if let Some(actor) = engine.get_actor_ref(path) {
                let response = actor
                    .send(Box::new(path.clone()))
                    .await
                    .unwrap()
                    .downcast::<String>()
                    .unwrap();
                assert_eq!(*response, *path);
                reports.push(ComponentStateReport {
                    path: path.clone(),
                    state: "running".into(),
                    version: version.into(),
                });
            }
        }
        let mut observed = ObservedState::default();
        observed.components.insert("pool".into(), reports);
        observed
    }
    let dir = tempfile::tempdir().unwrap();
    let mut pool = spec("pool");
    pool.instances = InstancePolicy::Pool(2);
    let initial = manifest(vec![pool]);
    let store = Arc::new(FileStateStore::open(dir.path(), Default::default()).unwrap());
    let mut supervisor =
        AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0))).unwrap();
    let engine = thread_system();
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let plan = parrot_app::plan(supervisor.desired(), &LocalTopology).unwrap();
    context
        .assemble(&plan, &deployer, supervisor.desired(), &Default::default())
        .await
        .unwrap();
    let paths = context.component_paths("pool").unwrap().to_vec();
    assert!(
        supervisor
            .reconcile_once(&observe(&engine, &paths, "1.0.0").await)
            .converged
    );
    // Kill one actual actor. Counts from a synthetic vector cannot prove this repair.
    spawner.stop_paths(&[paths[1].clone()]).await.unwrap();
    assert!(engine.get_actor_ref(&paths[1]).is_none());
    let report = supervisor.reconcile_once(&observe(&engine, &paths, "1.0.0").await);
    assert_eq!(
        report.actions,
        vec![
            ReconcileAction::Drain {
                comp: "pool".into()
            },
            ReconcileAction::Stop {
                comp: "pool".into()
            },
            ReconcileAction::Deploy {
                comp: "pool".into(),
                node: "parrot-gw-1".into()
            }
        ]
    );
    for action in report.actions {
        match action {
            ReconcileAction::Drain { .. } => context.drain(&deployer).await.unwrap(),
            ReconcileAction::Stop { .. } => context.teardown(&deployer).await.unwrap(),
            ReconcileAction::Deploy { .. } => context
                .assemble(&plan, &deployer, supervisor.desired(), &Default::default())
                .await
                .unwrap(),
            other => panic!("unexpected action: {other:?}"),
        }
    }
    assert!(
        supervisor
            .reconcile_once(&observe(&engine, &paths, "1.0.0").await)
            .converged
    );
    let mut update = initial.clone();
    update.components[0].instances = InstancePolicy::Pool(3);
    update.version = "2.0.0".into();
    supervisor.submit(update).unwrap();
    let generation = supervisor.upgrade_generation("pool").unwrap();
    drop(supervisor);
    drop(store);
    let store = Arc::new(FileStateStore::open(dir.path(), Default::default()).unwrap());
    let mut recovered =
        AppSupervisor::new(initial.clone(), store.clone(), Arc::new(FakeClock::new(0))).unwrap();
    assert_eq!(recovered.upgrade_generation("pool"), Some(generation));
    assert!(recovered
        .reconcile_once(&observe(&engine, &paths, "1.0.0").await)
        .actions
        .iter()
        .all(|a| matches!(a, ReconcileAction::Wait { .. })));
    // The rollout owner executes the recovered manifest, then acknowledges its exact generation.
    context.drain(&deployer).await.unwrap();
    context.teardown(&deployer).await.unwrap();
    let plan = parrot_app::plan(recovered.desired(), &LocalTopology).unwrap();
    context
        .assemble(&plan, &deployer, recovered.desired(), &Default::default())
        .await
        .unwrap();
    let paths = context.component_paths("pool").unwrap().to_vec();
    assert_eq!(paths.len(), 3);
    let observed = observe(&engine, &paths, "2.0.0").await;
    assert_eq!(observed.instance_count("pool"), 3);
    assert!(recovered.finish_upgrade("pool", generation).unwrap());
    assert!(recovered.reconcile_once(&observed).converged);
    drop(recovered);
    drop(store);
    let recovered = AppSupervisor::new(
        initial,
        Arc::new(FileStateStore::open(dir.path(), Default::default()).unwrap()),
        Arc::new(FakeClock::new(0)),
    )
    .unwrap();
    assert!(recovered.upgrade_in_flight("pool").is_none());
    assert_eq!(
        recovered.desired().components[0].instances,
        InstancePolicy::Pool(3)
    );
    context.teardown(&deployer).await.unwrap();
    for path in paths {
        assert!(engine.get_actor_ref(&path).is_none());
    }
    engine.shutdown_internal().await.unwrap();
}
