//! K2 receptionist 集成锚点（DEV_02 §8.4）：ActorContext 三方法 →
//! ReceptionistGateway 注入 → 事件流（快照回放 + 续流）端到端贯通。

mod common;

#[allow(unused_imports)]
use common::*;
use parrot::system::{ParrotActorSystem, ReceptionistGateway};
use parrot::thread::config::ThreadActorConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, EmptyConfig};
use parrot_api::address::ActorPath;
use parrot_api::context::ActorContext;
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_remote::receptionist::ReceptionistFacade;
use std::sync::Arc;

/// 桩 actor：收到 "reg" 消息时把自身注册到 receptionist key "jvm/echo"。
struct RegActor;

impl Actor for RegActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.downcast_ref::<String>().is_some_and(|s| s == "reg") {
                let key = parrot_api::receptionist::ReceptionistKey::new("jvm/echo").unwrap();
                ctx.receptionist_register(key).await?;
                return Ok(Box::new("ok".to_string()) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unhandled".into(),
            ))
        })
    }

    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receptionist_context_roundtrip() {
    let facade = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );

    // gateway 注入（先注入后注册 thread 系统——继承路径）
    let gw = Arc::new(ReceptionistFacade::new("node-a"));
    facade
        .register_receptionist_gateway(gw.clone())
        .await
        .unwrap();

    let ts = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();
    ts.spawn_at(RegActor, "/user/reg", None, ThreadActorConfig::default())
        .await
        .unwrap();

    // 订阅先行（后续注册事件实时到达）
    let mut stream = {
        let key = parrot_api::receptionist::ReceptionistKey::new("jvm/echo").unwrap();
        gw.subscribe(key).await.unwrap()
    };

    // actor 收 "reg" → ctx.receptionist_register
    let reg = facade
        .get_actor(&ActorPath::placeholder("/user/reg"))
        .await
        .unwrap();
    let r = reg.send(Box::new("reg".to_string())).await.unwrap();
    assert_eq!(r.downcast_ref::<String>().unwrap(), "ok");

    // 事件流：Registered 到达（remote_path 指向注册者）
    let ev = tokio::time::timeout(std::time::Duration::from_secs(3), stream.recv())
        .await
        .expect("event within 3s")
        .expect("stream open");
    match ev {
        parrot_api::receptionist::ReceptionistEvent::Registered { key, remote_path } => {
            assert_eq!(key.as_str(), "jvm/echo");
            assert!(remote_path.contains("/user/reg"), "path={remote_path}");
        }
        other => panic!("unexpected event: {other:?}"),
    }

    // 后注册的 thread 系统也继承 gateway（同 facade 二次注册验证）
    let ts2 = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("eng2".into(), ts2.clone(), false)
        .await
        .unwrap();
    assert!(
        ts2.receptionist_gateway().is_some(),
        "late thread system inherits gateway"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receptionist_not_enabled_without_gateway() {
    let facade = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();
    ts.spawn_at(RegActor, "/user/reg2", None, ThreadActorConfig::default())
        .await
        .unwrap();

    let reg = facade
        .get_actor(&ActorPath::placeholder("/user/reg2"))
        .await
        .unwrap();
    let r = reg.send(Box::new("reg".to_string())).await;
    assert!(
        r.is_err(),
        "receptionist must be Unsupported without gateway injection"
    );
}
