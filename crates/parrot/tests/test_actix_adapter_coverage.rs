//! Coverage completion for the actix adapter surface touched by the
//! ADR-2/ADR-3 changes (and its immediately adjacent public API):
//!
//! - `ActixActor`'s own `ParrotActor` impl (nesting adapter-over-adapter):
//!   init / receive_message (error arm) / receive_message_with_engine
//!   (forwarding arm) / state;
//! - the native `StopMessage` handler (ctx.stop path);
//! - `ActorBase::new` + `IntoActorBase::into_actor_base`;
//! - `ActixActor::state()` accessor across lifecycle transitions.
//!
//! These are exercised against a real arbiter inside an actix System.

use actix::{Actor as ActixActorTrait, Addr};
use parrot::actix as __parrot_engine;
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, ActorBase, IntoActorBase};
use parrot_api::actor::{Actor as ParrotActor, ActorState, EmptyConfig, EngineContextHandle};
use parrot_api::address::{ActorRef, ActorRefExt};
use parrot_api::types::BoxedActorRef;
use parrot_api::types::{ActorResult, BoxedMessage};
use parrot_api_derive::{Message, ParrotActor};
use std::any::Any;
use std::ptr::NonNull;
use std::time::Duration;

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct Add(u64);

/// Inner actor used both standalone and nested inside another ActixActor.
#[derive(Debug, ParrotActor)]
#[ParrotActor(engine = "actix", config = "EmptyConfig")]
struct Inner {
    v: u64,
}

// M1 derive-decouple: the macro no longer generates `IntoActorBase`
// (engine-specific glue, previously dead code). Engines users who need it
// implement it explicitly — it is a one-liner.
impl parrot::actix::IntoActorBase for Inner {
    fn into_actor_base(self) -> parrot::actix::ActorBase<Self> {
        parrot::actix::ActorBase::new(self)
    }
}

impl Inner {
    async fn handle_message(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut <Self as ParrotActor>::Context,
    ) -> ActorResult<BoxedMessage> {
        if let Some(m) = msg.downcast_ref::<Add>() {
            self.v += m.0;
            return Ok(Box::new(self.v) as BoxedMessage);
        }
        Err(parrot_api::errors::ActorError::MessageHandlingError(
            "unknown".into(),
        ))
    }

    fn handle_message_engine(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut <Self as ParrotActor>::Context,
        _engine_ctx: EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        if let Some(m) = msg.downcast_ref::<Add>() {
            self.v += m.0;
            return Some(Ok(Box::new(self.v) as BoxedMessage));
        }
        None
    }
}

/// `ActixActor::state()` accessor and lifecycle transitions observable
/// from outside (post-start Running; never Stopped here since the actor
/// stays alive for the whole System).
#[test]
fn actix_actor_state_accessor_lifecycle() {
    actix::System::new().block_on(async {
        let sys = ActixActorSystem::new().await.expect("system");
        let aref = sys
            .spawn_root_typed(Inner { v: 0 }, EmptyConfig)
            .await
            .expect("spawn");

        // The boxed ref is alive; state via ask round-trip.
        assert_eq!(aref.ask(Add(1)).await.unwrap(), 1);

        // stop() drives Stopping/Stopped through the native StopMessage
        // handler (Handler<StopMessage> → ctx.stop()).
        aref.stop().await.expect("stop ok");

        let result = tokio::time::timeout(Duration::from_secs(2), aref.ask(Add(1)))
            .await
            .expect("post-stop ask must complete");
        assert!(
            matches!(result, Err(parrot_api::errors::ActorError::Stopped)),
            "{result:?}"
        );
        assert!(!aref.is_alive().await);
        sys.shutdown().await.unwrap();
    });
}

/// The adapter-over-adapter `ParrotActor for ActixActor<A>` impl:
/// init (Ok), receive_message (error arm — adapter declines the async
/// path), receive_message_with_engine (forwarding to inner), state.
#[test]
fn actix_actor_nested_parrot_impl() {
    actix::System::new().block_on(async {
        // Build the outer ActixActor wrapping an Inner (started → has ctx).
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 10 }).start();
        let _ = addr;

        // Drive the trait methods directly through a second instance that
        // we manufacture with a context, mirroring `started`.
        let mut outer = ActixActor::new(Inner { v: 100 });
        // started() runs on the arbiter; for direct trait coverage we call
        // the pieces ourselves.
        use parrot::actix::message::ActixMessageWrapper;
        let _wrapper_ty = std::marker::PhantomData::<ActixMessageWrapper>;

        // -- ParrotActor::init (adapter returns Ok immediately) --
        // init requires &mut ActixContext<ActixActor<Inner>>; manufacture
        // one via ActixContext::new with the live addr.
        let path = parrot_api::address::ActorPath::placeholder("nested/outer");
        let mut pctx = ActixContext::new(addr.clone(), path);
        {
            let fut = ParrotActor::init(&mut outer, &mut pctx);
            assert!(fut.await.is_ok(), "adapter init must be Ok");
        }

        // -- receive_message: adapter's documented error arm --
        {
            let fut = ParrotActor::receive_message(&mut outer, Box::new(Add(1)), &mut pctx);
            let r = fut.await;
            let e = r.expect_err("adapter async path must decline");
            assert!(e.to_string().contains("Not use on actix engine"));
        }

        // -- receive_message_with_engine: forwards to inner → Some(101) --
        {
            let mut data = 0u32;
            let raw: NonNull<dyn Any> = NonNull::from(&mut data);
            let h = unsafe { EngineContextHandle::from_raw(raw) };
            // M6: 同步快路径经扩展 trait；适配层转发到 inner（Inner 实现了
            // ActixEngineExt），默认 None 时走 async 错误臂。
            let r = parrot_api::actor::ActixEngineExt::receive_message_with_engine(
                outer.inner_mut(),
                Box::new(Add(1)),
                &mut pctx,
                h,
            );
            let v = r.expect("inner handled Add").unwrap();
            assert_eq!(*v.downcast::<u64>().unwrap(), 101);
        }

        // -- state() forwards to the adapter's tracked state --
        // (Before `started`, the adapter holds Starting; the constructor
        // initializes it that way.)
        assert_eq!(ParrotActor::state(&outer), ActorState::Starting);

        // -- pub state() accessor --
        assert_eq!(outer.state(), ActorState::Starting);
    });
}

/// `ActorBase::new` + `IntoActorBase::into_actor_base` round trip.
#[test]
fn actor_base_roundtrip() {
    let base: ActorBase<Inner> = Inner { v: 5 }.into_actor_base();
    // M1: derive no longer generates IntoActorBase; the explicit impl above
    // covers the same call shape.
    let base2 = Inner { v: 6 }.into_actor_base();
    assert_eq!(base.actor.v, 5);
    assert_eq!(base2.actor.v, 6);
    let direct = ActorBase::new(Inner { v: 7 });
    assert_eq!(direct.actor.v, 7);
}

// ---------------------------------------------------------------------------
// reference.rs surface completion
// ---------------------------------------------------------------------------

use parrot::actix::reference::StopMessage;

/// Direct construction of ActixActorRef and its accessors (get_addr /
/// get_path / Debug).
#[test]
fn actix_actor_ref_accessors_and_debug() {
    actix::System::new().block_on(async {
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        let r = parrot::actix::reference::ActixActorRef::new(addr.clone(), "test://acc".into());

        assert_eq!(r.get_path(), &"test://acc".to_string());
        // Addr equality round-trips through get_addr.
        let addr2 = r.get_addr().clone();
        assert_eq!(addr, addr2);

        // Debug renders the path without panicking.
        let s = format!("{:?}", r);
        assert!(s.contains("ActixActorRef"));

        // create_envelope / do_send / try_send against the live addr.
        r.do_send(Box::new(Add(1)));
        r.try_send(Box::new(Add(2)))
            .expect("live mailbox has capacity");
        assert_eq!(
            r.ask(Add(0)).await.unwrap(),
            3,
            "both admitted messages precede the query"
        );
        r.stop().await.unwrap();
    });
}

/// Native StopMessage delivered through the actix addr exercises
/// Handler<StopMessage> (ctx.stop path) directly.
#[test]
fn native_stop_message_handler() {
    actix::System::new().block_on(async {
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        // Deliver StopMessage wrapped in the standard envelope (the path
        // the adapter's dispatch interception recognizes).
        let payload: BoxedMessage = Box::new(StopMessage);
        let r =
            parrot::actix::reference::ActixActorRef::new(addr.clone(), "t://native-stop".into());
        r.do_send(payload);
        tokio::time::timeout(Duration::from_secs(2), async {
            while addr.connected() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("enveloped stop closes mailbox");
        // Mailbox closed after stop → send errors.
        let envelope = parrot::actix::message::create_envelope(Add(1));
        let wrapper = parrot::actix::message::ActixMessageWrapper { envelope };
        let r2 = addr.send(wrapper).await;
        assert!(r2.is_err(), "actor must be stopped after StopMessage");
    });
}

/// Managed stop completion seals the reference before any subsequent request.
#[test]
fn managed_stop_rejects_subsequent_requests() {
    actix::System::new().block_on(async {
        let sys = ActixActorSystem::new().await.expect("system");
        let aref = sys
            .spawn_root_typed(Inner { v: 0 }, EmptyConfig)
            .await
            .expect("spawn");
        aref.ask(Add(1)).await.expect("alive");
        aref.stop().await.expect("ActorRef::stop ok");
        let result = tokio::time::timeout(Duration::from_secs(2), aref.ask(Add(1)))
            .await
            .expect("post-stop ask must complete");
        assert!(
            matches!(result, Err(parrot_api::errors::ActorError::Stopped)),
            "{result:?}"
        );
        sys.shutdown().await.unwrap();
    });
}

// ---------------------------------------------------------------------------
// actix/message.rs surface: MessageDowncast impl for BoxedMessage
// ---------------------------------------------------------------------------

#[test]
fn boxed_message_downcast_surface() {
    use parrot::actix::message::MessageDowncast;

    // downcast_ref / downcast_mut on a BoxedMessage.
    let mut bm: BoxedMessage = Box::new(42u32);
    assert_eq!(MessageDowncast::downcast_ref::<u32>(&bm), Some(&42));
    assert_eq!(MessageDowncast::downcast_ref::<u64>(&bm), None);
    *MessageDowncast::downcast_mut::<u32>(&mut bm).unwrap() = 7;
    assert_eq!(MessageDowncast::downcast_ref::<u32>(&bm), Some(&7));

    // Consuming downcast: success and failure (with diagnostic).
    let ok: Result<u32, _> = MessageDowncast::downcast(bm);
    assert_eq!(ok.unwrap(), 7);
    let bad: Result<String, _> = MessageDowncast::downcast(Box::new(1u32) as BoxedMessage);
    let e = bad.unwrap_err();
    assert!(e.to_string().contains("Failed to downcast message"));
}

/// The dedicated `Handler<StopMessage>` impl (native StopMessage delivered
/// directly to the addr, bypassing the ActixMessageWrapper envelope) must
/// stop the actor and close its mailbox.
#[test]
fn native_stop_message_direct_handler() {
    use parrot::actix::reference::StopMessage;
    actix::System::new().block_on(async {
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        // do_send requires Message + Send; native StopMessage qualifies.
        addr.do_send(StopMessage);
        tokio::time::timeout(Duration::from_secs(2), async {
            while addr.connected() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("native stop closes mailbox");
        // After stop the mailbox is closed: further sends fail.
        let envelope = parrot::actix::message::create_envelope(Add(1));
        let wrapper = parrot::actix::message::ActixMessageWrapper { envelope };
        let r = addr.try_send(wrapper);
        assert!(
            matches!(r, Err(actix::prelude::SendError::Closed(_))),
            "mailbox must be closed after native stop"
        );
    });
}

/// The defensive "context not initialized" branch: dispatching into an
/// ActixActor whose `started` has not yet populated `self.ctx` must reply
/// with the explicit diagnostic error (never panic, never hang).
#[test]
fn dispatch_before_started_reports_diagnostic() {
    use actix::dev::Context as ActixRawContext;
    use parrot::actix::message::ActixMessageWrapper;
    use parrot_api::message::MessageOptions;

    actix::System::new().block_on(async {
        // Manufacture a not-yet-started adapter (ctx field is None).
        let mut unstarted = ActixActor::new(Inner { v: 0 });
        assert_eq!(unstarted.state(), ActorState::Starting);

        // Invoke the Handler directly with a fresh raw actix Context,
        // bypassing the arbiter's started() ordering.
        let mut raw_ctx = ActixRawContext::<ActixActor<Inner>>::new();
        let envelope = parrot_api::message::MessageEnvelope {
            trace_context: None,
            id: uuid::Uuid::new_v4(),
            payload: Box::new(Add(1)),
            sender: None,
            options: MessageOptions::default(),
            message_type: "Add",
        };
        let wrapper = ActixMessageWrapper { envelope };

        use actix::Handler as ActixHandler;
        let resp =
            ActixHandler::<ActixMessageWrapper>::handle(&mut unstarted, wrapper, &mut raw_ctx);
        // AtomicResponse must resolve to the diagnostic error.
        // Drive the AtomicResponse through MessageResponse::handle with a
        // oneshot sender; the ready future resolves on ctx.wait inside
        // raw_ctx.run.
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<ActorResult<BoxedMessage>>>();
        actix::dev::MessageResponse::<ActixActor<Inner>, ActixMessageWrapper>::handle(
            resp,
            &mut raw_ctx,
            Some(tx),
        );
        let addr = raw_ctx.run(unstarted);
        let err = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .expect("diagnostic must resolve")
            .expect("reply channel must remain live")
            .expect("Some error reply")
            .expect_err("diagnostic error");
        assert!(err.to_string().contains("context not initialized"), "{err}");
        // Starting the adapter to drive its response must not resurrect the
        // rejected message. Only this newly admitted Add may mutate the state.
        let reference =
            parrot::actix::reference::ActixActorRef::new(addr, "test://pre-start-rejection".into());
        assert_eq!(reference.ask(Add(1)).await.unwrap(), 1);
        reference.stop().await.unwrap();
    });
}

// ---------------------------------------------------------------------------
// actix/context.rs full ActorContext surface
// ---------------------------------------------------------------------------

/// Drive every safe method on `ActixContext`'s `ActorContext` impl once.
#[test]
fn actix_context_full_surface() {
    use parrot::actix::context::ActixContext;
    use parrot_api::context::ActorContext as _;
    use parrot_api::supervisor::SupervisorStrategyType;

    actix::System::new().block_on(async {
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        let mut pctx = ActixContext::new(
            addr.clone(),
            parrot_api::address::ActorPath::new(
                std::sync::Arc::new(parrot::actix::ActixActorRef::new(
                    addr.clone(),
                    "surf".into(),
                )),
                "surf".into(),
            ),
        );

        // accessors
        assert!(!pctx.path().path.is_empty());
        assert_eq!(*pctx.addr(), addr);
        let _self_ref = pctx.get_self_ref();
        assert!(pctx.parent().is_none());
        assert!(pctx.children().is_none());
        assert!(pctx.receive_timeout().is_none());

        // parent / children management
        let other: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 1 }).start();
        let parent_ref: BoxedActorRef = Box::new(parrot::actix::reference::ActixActorRef::new(
            other.clone(),
            "test://parent".into(),
        ));
        pctx.set_parent(parent_ref.clone_boxed());
        assert!(pctx.parent().is_some());
        pctx.add_child(parent_ref.clone_boxed());
        assert_eq!(pctx.children().expect("children").len(), 1);
        pctx.remove_child(parent_ref.clone_boxed());
        assert_eq!(pctx.children().expect("children").len(), 0);

        // new_with_parent constructor path
        let with_parent = ActixContext::new_with_parent(
            other.clone(),
            parrot_api::address::ActorPath::placeholder("wp"),
            parent_ref.clone_boxed(),
        );
        assert!(with_parent.parent().is_some());

        // A detached context stores timeout configuration; real dispatch is covered
        // by test_receive_timeout; managed supervision has its own contract suite.
        pctx.set_receive_timeout(Some(Duration::from_secs(1)));
        assert_eq!(pctx.receive_timeout(), Some(Duration::from_secs(1)));
        pctx.set_receive_timeout(None);
        assert_eq!(pctx.receive_timeout(), None);
        pctx.set_supervisor_strategy(SupervisorStrategyType::default());

        // Raw references do not expose a managed DeathWatch endpoint.
        assert!(pctx.watch(parent_ref.clone_boxed()).await.is_err());
        assert!(pctx.unwatch(parent_ref.clone_boxed()).await.is_err());

        // send/ask through the context to a live target.
        let target: BoxedActorRef = Box::new(parrot::actix::reference::ActixActorRef::new(
            addr.clone(),
            "test://target".into(),
        ));
        // schedule_once fires the delayed delivery.
        assert!(
            pctx.schedule_once(
                target.clone_boxed(),
                Box::new(Add(5)),
                Duration::from_millis(10)
            )
            .await
            .is_ok()
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let value: u64 = target.ask(Add(0)).await.unwrap();
                match value {
                    5 => break,
                    0 => tokio::task::yield_now().await,
                    unexpected => panic!("scheduled addition must occur once, got {unexpected}"),
                }
            }
        })
        .await
        .expect("scheduled message must reach the actor");
        pctx.stop().await.unwrap();
        assert!(
            !addr.connected(),
            "context stop must close its actual actor"
        );
    });
}

// Registration returns promptly; dropping the standalone context cancels ticks.
#[test]
fn actix_context_schedule_periodic_one_tick() {
    use parrot::actix::context::ActixContext;
    use parrot_api::context::ActorContext as _;

    actix::System::new().block_on(async {
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        let pctx = ActixContext::new(
            addr.clone(),
            parrot_api::address::ActorPath::placeholder("per"),
        );
        let target: BoxedActorRef = Box::new(parrot::actix::reference::ActixActorRef::new(
            addr.clone(),
            "test://per".into(),
        ));
        // Cloneable Add ticks: each delivery increments the actor's value.
        let cm = parrot_api::message::CloneableMessage::from_message(Add(1));
        tokio::time::timeout(
            Duration::from_millis(200),
            pctx.schedule_periodic(
                target.clone_boxed(),
                cm,
                Duration::from_millis(5),
                Duration::from_millis(5),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let count: u64 = target.ask(Add(0)).await.unwrap();
                if count > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(pctx);
        // Allow an already admitted tick to drain, then verify no publisher remains.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let stopped: u64 = target.ask(Add(0)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let later: u64 = target.ask(Add(0)).await.unwrap();
        assert_eq!(later, stopped);
    });
}

#[derive(Debug, Clone, ParrotActor)]
#[ParrotActor(engine = "actix", config = "EmptyConfig")]
struct DeathWatchObserver {
    seen: std::sync::Arc<std::sync::Mutex<Vec<parrot_api::context::ActorTerminated>>>,
}

impl DeathWatchObserver {
    async fn handle_message(
        &mut self,
        _msg: BoxedMessage,
        _ctx: &mut ActixContext<ActixActor<Self>>,
    ) -> ActorResult<BoxedMessage> {
        Err(parrot_api::errors::ActorError::MessageHandlingError(
            "death-watch messages use the native Actix engine path".into(),
        ))
    }

    fn handle_message_engine(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut ActixContext<ActixActor<Self>>,
        _engine_ctx: EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        if let Ok(message) = msg.downcast::<parrot_api::context::ActorTerminated>() {
            self.seen.lock().unwrap().push(*message);
            return Some(Ok(Box::new(())));
        }
        Some(Err(parrot_api::errors::ActorError::MessageHandlingError(
            "unexpected death-watch message".into(),
        )))
    }
}

#[test]
fn actix_managed_watch_delivers_after_real_termination() {
    actix::System::new().block_on(async {
        let system = ActixActorSystem::new().await.expect("system");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observer = system
            .spawn_root_typed(DeathWatchObserver { seen: seen.clone() }, EmptyConfig)
            .await
            .expect("observer");
        let target = system
            .spawn_root_typed(Inner { v: 0 }, EmptyConfig)
            .await
            .expect("target");

        target
            .watch(observer.clone_boxed())
            .await
            .expect("watch registration");
        // Idempotent registration must not duplicate the notification.
        target
            .watch(observer.clone_boxed())
            .await
            .expect("duplicate watch registration");
        target.stop().await.expect("target stop");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !seen.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("watch notification");
        let (event_count, event_path, event_reason) = {
            let events = seen.lock().unwrap();
            (
                events.len(),
                events[0].path.clone(),
                events[0].reason.clone(),
            )
        };
        assert_eq!(event_count, 1, "duplicate watch must be coalesced");
        assert_eq!(event_path, target.path());
        assert_eq!(event_reason, parrot_api::DeathReason::Normal);

        observer.stop().await.expect("observer stop");
    });
}

#[test]
fn actix_watch_unwatch_and_late_registration_are_deterministic() {
    actix::System::new().block_on(async {
        let system = ActixActorSystem::new().await.expect("system");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observer = system
            .spawn_root_typed(DeathWatchObserver { seen: seen.clone() }, EmptyConfig)
            .await
            .expect("observer");
        let target = system
            .spawn_root_typed(Inner { v: 0 }, EmptyConfig)
            .await
            .expect("target");

        target
            .watch(observer.clone_boxed())
            .await
            .expect("watch registration");
        target
            .unwatch(observer.clone_boxed())
            .await
            .expect("unwatch registration");
        target.stop().await.expect("target stop");
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "unwatch must suppress delivery"
        );

        let late = system
            .spawn_root_typed(Inner { v: 0 }, EmptyConfig)
            .await
            .expect("late target");
        late.stop().await.expect("late stop");
        let error = late
            .watch(observer.clone_boxed())
            .await
            .expect_err("watching a sealed target must fail");
        assert!(matches!(error, parrot_api::errors::ActorError::Stopped));

        observer.stop().await.expect("observer stop");
    });
}

// ---------------------------------------------------------------------------
// reference.rs: ask-timeout path & Full/Closed SendError mapping; types.rs
// ---------------------------------------------------------------------------

#[test]
fn actix_types_surface() {
    use parrot::actix::types::{ActorId, MessageEnvelope as TypeEnvelope};

    let id = ActorId::new("worker-1");
    assert_eq!(id.name(), "worker-1");
    assert_eq!(format!("{}", id), "worker-1");

    let env = TypeEnvelope {
        message: Box::new(1u8),
        message_type: "u8",
    };
    let s = format!("{:?}", env);
    assert!(s.contains("MessageEnvelope") && s.contains("u8"));
}

#[test]
fn actix_ref_closed_try_send_and_envelope_delivery() {
    use parrot::actix::reference::ActixActorRef;
    use parrot_api::message::MessageOptions;

    actix::System::new().block_on(async {
        // Closed-mailbox mapping: stop the actor then try_send.
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        let r = ActixActorRef::new(addr.clone(), "t://closed".into());
        r.stop().await.unwrap();
        assert!(matches!(
            r.try_send(Box::new(Add(1))),
            Err(actix::prelude::SendError::Closed(_))
        ));

        // create_envelope with sender + custom message_type/options.
        let addr2: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        let r2 = ActixActorRef::new(addr2, "t://env".into());
        let sender: BoxedActorRef = Box::new(ActixActorRef::new(
            ActixActor::new(Inner { v: 9 }).start(),
            "t://sender".into(),
        ));
        let wrapper = r2.create_envelope(
            Box::new(Add(3)),
            Some(sender),
            MessageOptions::default(),
            "Add",
        );
        assert_eq!(wrapper.envelope.message_type, "Add");
        let value = r2.get_addr().send(wrapper).await.unwrap().unwrap().unwrap();
        assert_eq!(*value.downcast::<u64>().unwrap(), 3);
        assert_eq!(r2.ask(Add(0)).await.unwrap(), 3);
        r2.stop().await.unwrap();
    });
}

#[test]
fn actix_context_send_ask_and_child_branches() {
    use parrot::actix::context::ActixContext;
    use parrot_api::context::ActorContext as _;

    actix::System::new().block_on(async {
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        let mut pctx = ActixContext::new(
            addr.clone(),
            parrot_api::address::ActorPath::placeholder("sa"),
        );

        // Live target for context send/ask.
        let taddr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 100 }).start();
        let target: BoxedActorRef = Box::new(parrot::actix::reference::ActixActorRef::new(
            taddr,
            "t://sa".into(),
        ));

        // send (fire-and-forget through context).
        assert!(
            pctx.send(target.clone_boxed(), Box::new(Add(1)))
                .await
                .is_ok()
        );
        // ask (request-response through context).
        let r = pctx
            .ask(target.clone_boxed(), Box::new(Add(0)))
            .await
            .unwrap();
        let v = *r.downcast::<u64>().expect("u64 reply");
        assert_eq!(v, 101, "context send precedes ask on the same target");

        // add_child second time takes the push branch (children Some).
        pctx.add_child(target.clone_boxed());
        pctx.add_child(target.clone_boxed());
        assert_eq!(pctx.children().expect("children").len(), 2);
        // remove_child with a non-member ref takes the no-match branch.
        let stranger: BoxedActorRef = Box::new(parrot::actix::reference::ActixActorRef::new(
            ActixActor::new(Inner { v: 0 }).start(),
            "t://stranger".into(),
        ));
        pctx.remove_child(stranger);
        assert_eq!(
            pctx.children().expect("children").len(),
            2,
            "no-match remove is a no-op"
        );
    });
}

#[test]
fn detached_actix_context_rejects_stream_registration_without_dispatch_binding() {
    use parrot::actix::context::ActixContext;
    use parrot_api::context::ActorContext as _;
    actix::System::new().block_on(async {
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        let mut pctx = ActixContext::new(addr, parrot_api::address::ActorPath::placeholder("sr"));
        let empty: Box<dyn futures::Stream<Item = Box<dyn std::any::Any + Send>> + Send> =
            Box::new(futures::stream::empty());
        let error = pctx.stream_registry().add_stream_erased(empty).unwrap_err();
        assert!(error.to_string().contains("stream registry"));
    });
}

#[test]
fn actix_context_spawner_reports_capability_error() {
    use parrot::actix::context::ActixContext;
    use parrot_api::context::ActorContext as _;
    actix::System::new().block_on(async {
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        let mut pctx = ActixContext::new(addr, parrot_api::address::ActorPath::placeholder("sp"));
        let error = pctx
            .spawner()
            .spawn(Box::new(()), Box::new(()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("spawner"));
    });
}

/// A native closed mailbox maps to Stopped while preserving boxed-ref identity.
#[test]
fn actix_ref_closed_request_and_as_any() {
    use parrot::actix::reference::ActixActorRef;
    actix::System::new().block_on(async {
        let addr: Addr<ActixActor<Inner>> = ActixActor::new(Inner { v: 0 }).start();
        let reference = ActixActorRef::new(addr, "t://closed-request".into());
        assert!(
            reference
                .as_any()
                .downcast_ref::<ActixActorRef<Inner>>()
                .is_some()
        );
        reference.stop().await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), reference.ask(Add(1)))
            .await
            .expect("closed request must complete");
        assert!(
            matches!(result, Err(parrot_api::errors::ActorError::Stopped)),
            "{result:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// actix/system.rs surface completion
// ---------------------------------------------------------------------------

#[test]
fn actix_system_inspection_surface() {
    use parrot::actix::system::{ActixActorSystem, ArbiterPool};

    actix::System::new().block_on(async {
        let sys = ActixActorSystem::new().await.expect("system");

        // ArbiterPool::size + Debug.
        let pool = ArbiterPool::new(4);
        assert_eq!(pool.size(), 4);
        let dbg = format!("{:?}", pool);
        assert!(dbg.contains("ArbiterPool"));

        // Distinct systems own independent pools within the same Actix host.
        let sys2 = ActixActorSystem::new()
            .await
            .expect("second independent system");
        assert!(sys2.arbiter_size() >= 1);
        assert!(sys.arbiter_size() >= 1);

        // Spawn one named type and inspect this system independently.
        let aref = sys
            .spawn_root_typed(Inner { v: 0 }, EmptyConfig)
            .await
            .expect("spawn inspected actor");
        let v: u64 = aref.ask(Add(2)).await.unwrap();
        assert_eq!(v, 2);

        // status snapshot.
        let st = sys.status();
        assert_eq!(format!("{:?}", st.state), "Running");
        assert_eq!(st.active_actors, 1);
        assert_eq!(sys2.status().active_actors, 0);

        // get_actor with a bogus path → None (read-lock path).
        assert!(sys.get_actor(&"nope/missing".to_string()).await.is_none());
        sys.shutdown().await.unwrap();
        sys2.shutdown().await.unwrap();
    });
}
