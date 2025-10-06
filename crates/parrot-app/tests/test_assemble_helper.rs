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
use parrot_app::host::ThreadPropsSpawner;
use parrot_app::manifest::{AppManifest, ArtifactRef, ComponentSpec, EngineKind, InstancePolicy};
use parrot_app::planner::LocalTopology;
use parrot_app::test_support::test_assemble_default_cfg;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture};
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

fn spawn_m5_echo(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    let path = path.to_string();
    Box::pin(async move {
        let ts = thread_system();
        let r = ts
            .spawn_at(M5Echo, &path, None, ThreadActorConfig::default())
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(format!("spawn: {e}")))?;
        Ok(Box::new(r) as BoxedActorRef)
    })
}

/// 共享 thread 引擎（app_run_scenarios 同款——进程级专用 runtime 保活，
/// 规避 set_self_weak 绑首个测试 runtime 的坑）。
static TS: std::sync::OnceLock<Arc<ThreadActorSystem>> = std::sync::OnceLock::new();

fn thread_system() -> Arc<ThreadActorSystem> {
    TS.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("dedicated engine runtime");
        let handle = rt.handle().clone();
        Box::leak(Box::new(rt)); // 进程级保活（worker/timer 永生）
        ThreadActorSystem::shared_with_handle(Default::default(), handle)
    })
    .clone()
}

parrot_api::message::inventory::submit! {
    parrot_remote::admin::PropsFactory {
        name: "app.m5.echo",
        spawn: spawn_m5_echo,
    }
}

// ── 无网关部署面（纯 parrot 组件——gateway 分支不可达）──

struct NoGateway;
impl parrot_app::assemble::GatewayFactory for NoGateway {
    fn start_gateway(
        &self,
        _comp: &ComponentSpec,
        _paths: &[String],
    ) -> Result<Vec<BoxedActorRef>, parrot_app::assemble::DeployError> {
        Err(parrot_app::assemble::DeployError::Engine(
            "no gateway in m5 test".into(),
        ))
    }
    fn stop_gateway(&self, _comp: &ComponentSpec) -> Result<(), parrot_app::assemble::DeployError> {
        Ok(())
    }
}

fn spec(name: &str) -> ComponentSpec {
    ComponentSpec {
        name: name.into(),
        engine: EngineKind::Parrot,
        artifact: ArtifactRef::Props { factory: "app.m5.echo".into() },
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
    let a = assemble(vec![spec("only")]).await;
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
    let a = assemble(vec![c, b, spec("a")]).await; // 乱序声明
    // 装配序 = 依赖序（a → b → c——与声明序无关）
    assert_eq!(
        a.started(),
        &["a".to_string(), "b".to_string(), "c".to_string()]
    );
    a.teardown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paths_default_singleton() {
    let a = assemble(vec![spec("solo")]).await;
    assert_eq!(a.paths("solo"), Some(&["/user/solo".to_string()][..]));
    a.teardown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_factory_rolls_back_atomic() {
    // 未注册工厂 → 装配失败（fine 先成功——ctx 内部回卷后返回 Err）
    let mut bad = spec("bad");
    bad.artifact = ArtifactRef::Props { factory: "app.m5.noexist".into() };
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
    let a = assemble(vec![spec("victim")]).await;
    let r = a.first("victim").expect("ref");
    // 停用前可通信
    r.send(Box::new("hi".to_string()) as parrot_api::types::BoxedMessage)
        .await
        .expect("alive before teardown");
    a.teardown().await.unwrap();
    // 停用后 mailbox 已关闭——消息必失败（stop 语义：close + drain）
    let mut dead = false;
    for _ in 0..50 {
        if r
            .send(Box::new("hi".to_string()) as parrot_api::types::BoxedMessage)
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
    let a = assemble(vec![pool]).await;
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
