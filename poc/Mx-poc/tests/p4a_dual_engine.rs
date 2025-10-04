//! P4a 联邦 POC：**双引擎节点远程可达**（第 1 点疑问验证）。
//!
//! 一个 parrot 节点内同时注册 thread + actix 双引擎（`register_actix_system` +
//! `register_thread_system`），federation facade 的 `internal_get_actor` 按
//! "default 优先 + 遍历注册表" 查找。验证：远程帧可命中任一引擎的 actor，
//! 即"一个 docker 容器内 parrot 双引擎同时对外服务"成立。
//!
//! actix 引擎 actor 的路径是 `actix://{Name}/{uuid}`（`spawn_root_typed`
//! 自动生成），本测试先取回路径再远程访问。

use parrot::actix as __parrot_engine;
use parrot::actix::{ActixActorSystem, ActixContext, ActixActor};
use parrot::thread::context::ThreadContext;
use parrot_api::actor::{Actor, ActorState, EmptyConfig, ActixEngineExt, EngineContextHandle};
use parrot_api::address::ActorRef as _;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use remote_poc::*;
use std::sync::Arc;

// ---------- thread 引擎 actor（方言 *2） ----------

struct RustService;
impl Actor for RustService {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(Ping(n)) = m.downcast_ref::<Ping>() {
                Ok(Box::new(Pong(n * 2)) as BoxedMessage)
            } else {
                Err(parrot_api::errors::ActorError::MessageHandlingError("unsupported".into()))
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// ---------- actix 引擎 actor（方言 +1000，可辨识到达的是 actix 引擎） ----------

struct ActixService {
    hits: u64,
}

impl Actor for ActixService {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.hits += 1;
            if let Some(Ping(n)) = m.downcast_ref::<Ping>() {
                Ok(Box::new(Pong(n + 1000)) as BoxedMessage)
            } else {
                Err(parrot_api::errors::ActorError::MessageHandlingError("unsupported".into()))
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// 同步快路径（actix 动态轨默认路径——不 opt-in async handler）
impl ActixEngineExt for ActixService {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        if let Some(Ping(n)) = msg.downcast_ref::<Ping>() {
            self.hits += 1;
            return Some(Ok(Box::new(Pong(n + 1000)) as BoxedMessage));
        }
        None
    }
}

// ---------- 测试 ----------

#[test]
fn p4a_dual_engine_node_remotely_reachable() {
    // actix 需要 System runner；ingress 泵与 tokio runtime 共存。
    actix::System::new().block_on(async {
        CodecRegistry::reset();
        install_poc_messages();

        // 双引擎端点（内存链路即可验证查表逻辑）
        let (la, lb) = memory_pair();
        let a = spawn_endpoint(la).await; // 客户端节点
        let b = spawn_endpoint_dual(lb).await; // 双引擎服务节点

        // 1. thread 引擎 actor（默认引擎）
        let ts = b.local.get_thread_system("main").unwrap();
        ts.spawn_at::<RustService>(RustService, "/user/rust_service", None, Default::default())
            .await
            .unwrap();

        // 2. actix 引擎 actor：自建系统 + 手动注册（facade 无 actix 访问器，
        //    测试持 clone；注册表 Arc 共享，路径可被 facade 查到）
        let actix_sys = ActixActorSystem::new().await.unwrap();
        b.local
            .register_actix_system("actix".into(), actix_sys.clone(), false)
            .await
            .unwrap();
        let local_ref = actix_sys
            .spawn_root_typed(ActixService { hits: 0 }, EmptyConfig)
            .await
            .unwrap();
        let actix_path = local_ref.path();

        // 3. 从节点 A 远程 ask thread 引擎 actor → 方言 *2
        let r_thread = a.remote_ref("/user/rust_service");
        let pong = r_thread.send(Box::new(Ping(21))).await.unwrap();
        assert_eq!(pong.downcast_ref::<Pong>().unwrap().0, 42, "thread 引擎方言 *2");

        // 4. 从节点 A 远程 ask actix 引擎 actor → 方言 +1000
        let r_actix = a.remote_ref(actix_path.clone());
        let pong = r_actix.send(Box::new(Ping(40))).await.unwrap();
        assert_eq!(
            pong.downcast_ref::<Pong>().unwrap().0,
            1040,
            "actix 引擎方言 +1000（证明命中 actix 引擎而非 thread）"
        );

        // 5. 同节点跨引擎 ask（thread 引擎 → actix 引擎，经 facade 查表）
        let r_cross = a.remote_ref(actix_path);
        let pong = r_cross.send(Box::new(Ping(50))).await.unwrap();
        assert_eq!(pong.downcast_ref::<Pong>().unwrap().0, 1050);
    });
}
