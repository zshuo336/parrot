//! 跨引擎互操作 PoC（可行性实证，第十二轮前置验证）。
//!
//! 目标：同进程内 thread + actix 双引擎并存，运行在不同引擎上的 actor
//! 通过统一的 BoxedActorRef（持有即交互）与 ParrotActorSystem 路径解析
//! （按 path 跨系统查找）互相访问。
//!
//! 验证维度：
//!   X1 双系统同进程注册，各自 spawn 正常
//!   X2 thread actor → actix actor ask（跨引擎往返）
//!   X3 actix actor → thread actor ask（反方向）
//!   X4 跨引擎 tell（fire-and-forget deliver）
//!   X5 跨引擎消息类型不匹配的错误契约（B6 同款语义）
//!   X6 跨引擎嵌套 ask（thread actor 的 handler 内 ask actix actor）
//!   X7 ParrotActorSystem 按路径跨系统解析（先默认后遍历 fallback）
//!   X8 跨引擎 stop 后 send 的死信语义

mod engine_stress_common;

use parrot::system::ParrotActorSystem;
use parrot::thread::config::ThreadActorSystemConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::{ActorPath, ActorRefExt};
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::time::Duration;

// ===========================================================================
// 消息（跨引擎通用：BoxedMessage = Box<dyn Any + Send>，两引擎同语言）
// ===========================================================================

#[derive(Debug, Clone, PartialEq)]
struct Ping {
    hop: u64,
    payload: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Pong {
    hop: u64,
    echo: u64,
}

/// thread actor 不认识的消息（测 X5 错误契约）
#[derive(Debug)]
struct OnlyActix;

/// 让 thread actor 主动跨引擎 ask 对方的指令（测 X6）
struct AskRemote {
    remote: BoxedActorRef,
    value: u64,
}

/// 让 thread actor 主动 tell 对方的指令（测 X4 由 thread 侧发起）
struct TellRemote {
    remote: BoxedActorRef,
    value: u64,
}

struct GetSeen;
impl Message for GetSeen {
    type Result = Vec<u64>;
}

// ===========================================================================
// thread 引擎侧 actor
// ===========================================================================

struct ThreadSide {
    /// 跨引擎持有的对方引用（actix actor 的 BoxedActorRef）
    #[allow(dead_code)]
    remote: Option<BoxedActorRef>,
    seen: std::sync::Mutex<Vec<u64>>,
}

impl Actor for ThreadSide {
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
            if let Some(p) = m.downcast_ref::<Ping>() {
                // 收到 actix 侧发来的 Ping：记录并回 Pong（跨引擎 ask 的应答方向）
                self.seen.lock().unwrap().push(p.payload);
                Ok(Box::new(Pong {
                    hop: p.hop,
                    echo: p.payload,
                }) as BoxedMessage)
            } else if let Some(ar) = m.downcast_ref::<AskRemote>() {
                // X6：handler 内嵌套跨引擎 ask（thread handler 跑在 spawn_blocking，安全）
                let pong: Pong = ar
                    .remote
                    .ask(Ping {
                        hop: 99,
                        payload: ar.value,
                    })
                    .await?;
                self.seen.lock().unwrap().push(pong.echo);
                Ok(Box::new(pong.echo) as BoxedMessage)
            } else if let Some(t) = m.downcast_ref::<TellRemote>() {
                // X4：等待远端邮箱准入，不等待业务响应
                t.remote
                    .tell(Ping {
                        hop: 98,
                        payload: t.value,
                    })
                    .await?;
                Ok(Box::new(()) as BoxedMessage)
            } else if m.downcast_ref::<GetSeen>().is_some() {
                let snapshot = self.seen.lock().unwrap().clone();
                Ok(Box::new(snapshot) as BoxedMessage)
            } else {
                Err(parrot_api::errors::ActorError::MessageHandlingError(
                    format!("thread actor: unknown msg {:?}", m.downcast_ref::<String>()),
                ))
            }
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// ===========================================================================
// actix 引擎侧 actor
// ===========================================================================

use parrot::actix::actor::ActixActor;
use parrot::actix::context::ActixContext;
use parrot_api::message::Message;

impl Message for Ping {
    type Result = Pong;
    fn extract_result(r: BoxedMessage) -> ActorResult<Pong> {
        r.downcast::<Pong>()
            .map(|b| *b)
            .map_err(|_| parrot_api::errors::ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for Pong {
    type Result = Pong;
    fn extract_result(r: BoxedMessage) -> ActorResult<Pong> {
        r.downcast::<Pong>()
            .map(|b| *b)
            .map_err(|_| parrot_api::errors::ActorError::MessageHandlingError("type".into()))
    }
}
impl Message for TellRemote {
    type Result = ();
    fn extract_result(_r: BoxedMessage) -> ActorResult<()> {
        Ok(())
    }
}
impl Message for AskRemote {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| parrot_api::errors::ActorError::MessageHandlingError("type".into()))
    }
}

struct ActixSide {
    seen: std::sync::Mutex<Vec<u64>>,
}

impl Actor for ActixSide {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        _m: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async {
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "use engine path".into(),
            ))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// M6: actix 同步快路径移至引擎侧扩展 trait（ActixEngineExt）。
impl parrot_api::actor::ActixEngineExt for ActixSide {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        m: BoxedMessage,
        _c: &'a mut Self::Context,
        _e: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        // 同步引擎路径：actix 默认走这里
        if let Some(p) = m.downcast_ref::<Ping>() {
            self.seen.lock().unwrap().push(p.payload);
            Some(Ok(Box::new(Pong {
                hop: p.hop,
                echo: p.payload,
            }) as BoxedMessage))
        } else if m.downcast_ref::<GetSeen>().is_some() {
            Some(Ok(
                Box::new(self.seen.lock().unwrap().clone()) as BoxedMessage
            ))
        } else if m.downcast_ref::<OnlyActix>().is_some() {
            Some(Ok(Box::new(42u64) as BoxedMessage))
        } else {
            Some(Err(parrot_api::errors::ActorError::MessageHandlingError(
                "actix actor: unknown msg".into(),
            )))
        }
    }
}

// ===========================================================================
// PoC 测试
// ===========================================================================

#[test]
fn cross_engine_poc() {
    // actix 引擎要求 System 上下文
    actix::System::new().block_on(async {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap();

        // X1：双系统同进程注册
        let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
        parrot.register_thread_system("eng-thread".into(), ts.clone(), true).await.unwrap();
        let asys = parrot::actix::system::ActixActorSystem::new().await.unwrap();
        parrot.register_actix_system("eng-actix".into(), asys.clone(), false).await.unwrap();
        assert_eq!(parrot.list_registered_systems().unwrap().len(), 2);

        // 各自 spawn
        let t_ref: BoxedActorRef = {
            let typed = ts
                .spawn_at::<ThreadSide>(ThreadSide { remote: None, seen: Default::default() }, "/x/thread-side", None, Default::default())
                .await
                .unwrap();
            Box::new(typed)
        };
        let a_ref: BoxedActorRef = {
            asys
                .spawn_root_typed_with_strategy(ActixSide { seen: Default::default() }, EmptyConfig, parrot_api::supervisor::SupervisorStrategyType::Default(parrot_api::supervisor::DefaultStrategy::ResumeOnFailure))
                .await
                .expect("spawn ok")
        };

        // X2：跨引擎 ask（测试侧同时持有两引擎 ref，互发验证双向）
        let pong_from_actix = a_ref
            .ask(Ping { hop: 2, payload: 11 })
            .await
            .expect("cross-engine ask (caller→actix) must succeed");
        assert_eq!(pong_from_actix, Pong { hop: 2, echo: 11 }, "actix 侧业务回包正确");

        let pong_from_thread = t_ref
            .ask(Ping { hop: 3, payload: 13 })
            .await
            .expect("cross-engine ask (caller→thread) must succeed");
        assert_eq!(pong_from_thread, Pong { hop: 3, echo: 13 }, "thread 侧业务回包正确");

        // X6：thread actor handler 内嵌套 ask actix actor
        //    （把 actix ref 作为消息内容传进 thread actor，handler 里 ask）
        let nested = t_ref
            .ask(AskRemote { remote: a_ref.clone_boxed(), value: 21 })
            .await
            .expect("nested cross-engine ask inside thread handler");
        assert_eq!(nested, 21u64, "嵌套跨引擎 ask 的业务值穿透正确");

        // X4：thread handler 内 tell actix（不等待）
        t_ref.tell(TellRemote { remote: a_ref.clone_boxed(), value: 31 }).await.unwrap();
        // The first barrier waits for the sender actor to finish forwarding;
        // the second reads actual receiver state, not unrelated liveness.
        t_ref.ask(GetSeen).await.unwrap();
        assert_eq!(a_ref.ask(GetSeen).await.unwrap(), vec![11, 21, 31]);

        // X5：跨引擎消息类型不匹配 → 类型化业务错误（B6 契约）
        let wrong = a_ref.send(Box::new("nonsense".to_string()) as BoxedMessage).await;
        assert!(wrong.is_err(), "unknown msg type must yield typed error, got {:?}", wrong);

        // X7：ParrotActorSystem 按路径跨系统解析
        let t_path = t_ref.path();
        use parrot_api::system::ActorSystem as _;
        let resolved_t = parrot
            .get_actor(&ActorPath::placeholder(&t_path))
            .await;
        assert!(resolved_t.is_some(), "thread actor 可经门面按路径解析（默认系统）");
        // actix actor 路径形如 actix://ActixSide/{uuid}，非默认系统 → 走遍历 fallback
        let a_path = a_ref.path();
        assert!(a_path.starts_with("actix://"), "actix path scheme: {}", a_path);
        let resolved_a = parrot.get_actor(&ActorPath::placeholder(&a_path)).await;
        assert!(resolved_a.is_some(), "actix actor 可经门面跨系统 fallback 解析");
        assert_eq!(resolved_a.unwrap().path(), a_path);

        // X8：跨引擎 stop 后死信语义
        a_ref.stop().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let dead = a_ref.send(Box::new(Ping { hop: 5, payload: 51 }) as BoxedMessage).await;
        assert!(dead.is_err(), "send to stopped cross-engine actor must error");
        // thread 侧不受影响
        let still = t_ref.ask(Ping { hop: 6, payload: 61 }).await;
        assert!(still.is_ok(), "thread actor 不因对方死亡受影响");

        println!(
            "[X-PoC] 双引擎互操作全部通过：双向 ask / 嵌套 ask / tell / 类型错误契约 / 跨系统路径解析 / 死信语义 ✓"
        );

        let _ = ts.shutdown_internal().await;
        let _ = parrot.internal_shutdown().await;
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_tell_fifo_across_boxed_clones() {
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    let actor = ts
        .spawn_at::<ThreadSide>(
            ThreadSide {
                remote: None,
                seen: Default::default(),
            },
            "/tell-fifo",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    use parrot_api::address::ActorRef;
    let other = actor.clone_boxed();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        for value in 0..20_000 {
            let target: &dyn ActorRef = if value % 2 == 0 { &actor } else { &*other };
            target
                .tell(Ping {
                    hop: 0,
                    payload: value,
                })
                .await
                .unwrap();
        }
        ActorRefExt::ask(&actor, GetSeen).await.unwrap()
    })
    .await;
    ts.shutdown_internal().await.unwrap();
    assert_eq!(result.unwrap(), (0..20_000).collect::<Vec<_>>());
    assert!(
        actor
            .tell(Ping {
                hop: 0,
                payload: 20_001
            })
            .await
            .is_err()
    );
}

#[test]
fn actix_tell_fifo_across_boxed_clones() {
    actix::System::new().block_on(async {
        const WINDOW: u64 = 16;
        let system = parrot::actix::system::ActixActorSystem::with_settings(
            parrot::actix::ActixSystemSettings {
                mailbox_capacity: 32,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let actor = system
            .spawn_root_typed_with_strategy(
                ActixSide {
                    seen: Default::default(),
                },
                EmptyConfig,
                parrot_api::supervisor::SupervisorStrategyType::Default(
                    parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                ),
            )
            .await
            .unwrap();
        let other = actor.clone_boxed();
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            for value in 0..2_000 {
                let target = if value % 2 == 0 { &actor } else { &other };
                target
                    .tell(Ping {
                        hop: 0,
                        payload: value,
                    })
                    .await
                    .unwrap();
                // tell reports admission, not processing. A local yield does
                // not guarantee progress on the other arbiter. Keep each burst
                // below the configured capacity and await a real FIFO barrier.
                if (value + 1) % WINDOW == 0 {
                    assert_eq!(
                        actor.ask(GetSeen).await.unwrap(),
                        (0..=value).collect::<Vec<_>>()
                    );
                }
            }
            actor.ask(GetSeen).await.unwrap()
        })
        .await;
        system.shutdown().await.unwrap();
        assert_eq!(result.unwrap(), (0..2_000).collect::<Vec<_>>());
        assert!(
            actor
                .tell(Ping {
                    hop: 0,
                    payload: 2_001
                })
                .await
                .is_err()
        );
    });
}
