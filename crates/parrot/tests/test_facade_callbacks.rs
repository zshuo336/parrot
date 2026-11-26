//! Public-facade callback and retirement contracts. Bounded child processes turn
//! synchronous deadlocks into failures without abandoning blocked test threads.
use parrot::system::{ParrotActorSystem, PrefixResolver, ReceptionistGateway, RemoteGateway};
use parrot::thread::system::ThreadActorSystem;
use parrot_api::{
    address::{ActorPath, ActorRef},
    receptionist::{ReceptionistKey, ReceptionistStream},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::sync::Arc;
use std::time::Duration;

type Callback = Box<dyn Fn() + Send + Sync>;

#[derive(Debug)]
struct OriginalLookupPanic;
struct OriginalThrowingLookupPanic;
impl Drop for OriginalThrowingLookupPanic {
    fn drop(&mut self) {
        panic!("original callback panic payload disposal");
    }
}
struct ThrowingLookupPanic;
impl Drop for ThrowingLookupPanic {
    fn drop(&mut self) {
        panic!("lookup panic payload disposal");
    }
}

fn assert_lookup_panic(payload: Box<dyn std::any::Any + Send>, callback_failed: bool) {
    let original = payload.is::<OriginalLookupPanic>();
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("")
        .to_owned();
    // Wrong old-production payloads may themselves throw on Drop. Retire them
    // before assertions so the test cannot manufacture a second unwind.
    if let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
        && let Err(last) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(secondary)))
    {
        std::mem::forget(last);
    }
    if callback_failed {
        assert!(
            original,
            "callback panic identity must survive owner cleanup"
        );
    } else {
        assert!(
            !message.is_empty(),
            "cleanup failure must propagate as a safe panic"
        );
    }
}

fn throwing_retirement(count: Arc<std::sync::atomic::AtomicUsize>) -> Callback {
    Box::new(move || {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::panic::panic_any(ThrowingLookupPanic);
    })
}

async fn prefix_owner_failure(resolver_kind: bool, callback_failed: bool, result_failed: bool) {
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let f = facade().await;
    let retired = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(AtomicUsize::new(0));
    let weak = Arc::downgrade(&f);
    let result_count = results.clone();
    let callback = move || {
        assert!(
            weak.upgrade()
                .unwrap()
                .unregister_prefix_handler("/owned/")
                .unwrap()
        );
        if callback_failed {
            std::panic::panic_any(OriginalLookupPanic);
        }
        let result_count = result_count.clone();
        Box::new(Ref {
            tag: 9,
            on_clone: None,
            retire: Some(Box::new(move || {
                result_count.fetch_add(1, Ordering::SeqCst);
                if result_failed {
                    std::panic::panic_any(ThrowingLookupPanic);
                }
            })),
        }) as BoxedActorRef
    };
    if resolver_kind {
        f.register_prefix_resolver(
            "/owned/",
            Arc::new(Resolver {
                resolve: Box::new(move |path| {
                    assert_eq!(path, "/owned/key");
                    Some(callback())
                }),
                retire: Some(throwing_retirement(retired.clone())),
            }),
        )
        .unwrap();
    } else {
        // The original static handler's clone callback runs before clone_boxed
        // returns its plain reference; result disposal is tested by the resolver.
        f.register_prefix_handler(
            "/owned/",
            Arc::new(Ref {
                tag: 9,
                on_clone: Some(Box::new(move || {
                    drop(callback());
                })),
                retire: Some(throwing_retirement(retired.clone())),
            }),
        )
        .unwrap();
    }
    let outcome =
        std::panic::AssertUnwindSafe(f.internal_get_actor(&ActorPath::placeholder("/owned/key")))
            .catch_unwind()
            .await;
    let payload = outcome.expect_err("failed owner cleanup must not publish a result");
    assert_lookup_panic(payload, callback_failed);
    assert_eq!(retired.load(Ordering::SeqCst), 1);
    assert_eq!(
        results.load(Ordering::SeqCst),
        usize::from(!callback_failed)
    );
    assert!(
        f.internal_get_actor(&ActorPath::placeholder("/owned/key"))
            .await
            .is_none()
    );
    f.register_prefix_resolver("/owned/", resolver(7)).unwrap();
    assert_eq!(
        f.internal_get_actor(&ActorPath::placeholder("/owned/key"))
            .await
            .unwrap()
            .path(),
        "ref-7"
    );
}

macro_rules! prefix_failure_case {
    ($name:ident, $resolver:expr, $callback:expr, $result:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                run(prefix_owner_failure($resolver, $callback, $result))
            });
        }
    };
}
prefix_failure_case!(
    resolver_callback_and_last_owner_panics_are_independent,
    true,
    true,
    false
);
prefix_failure_case!(
    resolver_last_owner_failure_retires_unpublished_result,
    true,
    false,
    false
);
prefix_failure_case!(
    resolver_owner_and_unpublished_result_panics_are_independent,
    true,
    false,
    true
);
prefix_failure_case!(
    static_clone_and_last_owner_panics_are_independent,
    false,
    true,
    false
);
prefix_failure_case!(static_last_owner_failure_rejects_clone, false, false, false);

struct ResultGateway {
    lookup: ResolveCallback,
    retire: Callback,
}

#[test]
fn original_throwing_callback_payload_remains_caller_owned_after_owner_failure() {
    isolated(
        "original_throwing_callback_payload_remains_caller_owned_after_owner_failure",
        || {
            run(async {
                use futures::FutureExt;
                use std::sync::atomic::{AtomicUsize, Ordering};
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                let retired = Arc::new(AtomicUsize::new(0));
                f.register_prefix_resolver(
                    "/owned/",
                    Arc::new(Resolver {
                        resolve: Box::new(move |_| {
                            assert!(
                                weak.upgrade()
                                    .unwrap()
                                    .unregister_prefix_handler("/owned/")
                                    .unwrap()
                            );
                            std::panic::panic_any(OriginalThrowingLookupPanic);
                        }),
                        retire: Some(throwing_retirement(retired.clone())),
                    }),
                )
                .unwrap();
                let payload = std::panic::AssertUnwindSafe(
                    f.internal_get_actor(&ActorPath::placeholder("/owned/key")),
                )
                .catch_unwind()
                .await
                .expect_err("original panic must propagate");
                let original = payload.is::<OriginalThrowingLookupPanic>();
                let secondary =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)));
                assert!(original, "callback payload must not be replaced by cleanup");
                assert_eq!(retired.load(Ordering::SeqCst), 1);
                assert!(
                    secondary.is_err(),
                    "caller must retain the original payload's disposal semantics"
                );
                drop(secondary);
                assert!(
                    f.internal_get_actor(&ActorPath::placeholder("/owned/key"))
                        .await
                        .is_none()
                );
            });
        },
    );
}
impl RemoteGateway for ResultGateway {
    fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (self.lookup)(path)
    }
}
impl Drop for ResultGateway {
    fn drop(&mut self) {
        (self.retire)();
    }
}

async fn remote_owner_failure(mode: u8, callback_failed: bool, result_failed: bool) {
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let f = facade().await;
    let retired = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(AtomicUsize::new(0));
    let weak = Arc::downgrade(&f);
    let result_count = results.clone();
    let gateway = Arc::new(ResultGateway {
        lookup: Box::new(move |path| {
            assert_eq!(path, "parrot://node/user/a");
            futures::executor::block_on(weak.upgrade().unwrap().register_remote_gateway(remote()))
                .unwrap();
            if callback_failed {
                std::panic::panic_any(OriginalLookupPanic);
            }
            let count = result_count.clone();
            Some(Box::new(Ref {
                tag: 9,
                on_clone: None,
                retire: Some(Box::new(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    if result_failed {
                        std::panic::panic_any(ThrowingLookupPanic);
                    }
                })),
            }))
        }),
        retire: throwing_retirement(retired.clone()),
    });
    let mut actix = None;
    let mut thread = None;
    if mode == 1 {
        let engine = ThreadActorSystem::shared(Default::default());
        f.register_thread_system("thread".into(), engine.clone(), true)
            .await
            .unwrap();
        thread = Some(engine);
    } else if mode == 2 {
        let engine = parrot::actix::ActixActorSystem::with_arbiter_count(1)
            .await
            .unwrap();
        f.register_actix_system("actix".into(), engine.clone(), true)
            .await
            .unwrap();
        actix = Some(engine);
    }
    f.register_remote_gateway(gateway).await.unwrap();
    let outcome = std::panic::AssertUnwindSafe(async {
        match mode {
            0 => {
                f.internal_get_actor(&ActorPath::placeholder("parrot://node/user/a"))
                    .await
            }
            1 => thread
                .as_ref()
                .unwrap()
                .resolve_actor("parrot://node/user/a"),
            2 => actix
                .as_ref()
                .unwrap()
                .resolve_actor("parrot://node/user/a"),
            _ => unreachable!(),
        }
    })
    .catch_unwind()
    .await;
    assert_lookup_panic(
        outcome.expect_err("cleanup failure must not publish a remote reference"),
        callback_failed,
    );
    assert_eq!(retired.load(Ordering::SeqCst), 1);
    assert_eq!(
        results.load(Ordering::SeqCst),
        usize::from(!callback_failed)
    );
    assert!(
        f.internal_get_actor(&ActorPath::placeholder("parrot://node/user/a"))
            .await
            .is_none()
    );
    f.shutdown_runtime().await.unwrap();
}
macro_rules! remote_failure_case {
    ($name:ident, $mode:expr, $callback:expr, $result:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                actix::System::new().block_on(remote_owner_failure($mode, $callback, $result));
            });
        }
    };
}
remote_failure_case!(
    remote_lookup_and_last_owner_panics_are_independent,
    0,
    true,
    false
);
remote_failure_case!(
    remote_last_owner_failure_retires_unpublished_result,
    0,
    false,
    false
);
remote_failure_case!(
    remote_owner_and_unpublished_result_panics_are_independent,
    0,
    false,
    true
);
remote_failure_case!(
    thread_forwarder_preserves_original_callback_panic,
    1,
    true,
    false
);
remote_failure_case!(
    actix_forwarder_preserves_original_callback_panic,
    2,
    true,
    false
);
remote_failure_case!(thread_forwarder_retires_unpublished_result, 1, false, true);
remote_failure_case!(actix_forwarder_retires_unpublished_result, 2, false, true);

async fn direct_gateway_owner_failure(actix_kind: bool, callback_failed: bool) {
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let retired = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(AtomicUsize::new(0));
    let thread = (!actix_kind).then(|| ThreadActorSystem::shared(Default::default()));
    let actix = if actix_kind {
        Some(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        )
    } else {
        None
    };
    let thread_weak = thread.as_ref().map(Arc::downgrade);
    let actix_ref = actix.clone();
    let result_count = results.clone();
    let gateway = Arc::new(ResultGateway {
        lookup: Box::new(move |_| {
            if let Some(engine) = &actix_ref {
                engine.set_remote_gateway(remote());
            }
            if let Some(engine) = &thread_weak {
                engine.upgrade().unwrap().set_remote_gateway(remote());
            }
            if callback_failed {
                std::panic::panic_any(OriginalLookupPanic);
            }
            let count = result_count.clone();
            Some(Box::new(Ref {
                tag: 9,
                on_clone: None,
                retire: Some(Box::new(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    std::panic::panic_any(ThrowingLookupPanic);
                })),
            }))
        }),
        retire: throwing_retirement(retired.clone()),
    });
    if let Some(engine) = &actix {
        engine.set_remote_gateway(gateway);
    } else {
        thread.as_ref().unwrap().set_remote_gateway(gateway);
    }
    let outcome = std::panic::AssertUnwindSafe(async {
        if let Some(engine) = &actix {
            engine.resolve_actor("parrot://node/user/a")
        } else {
            thread
                .as_ref()
                .unwrap()
                .resolve_actor("parrot://node/user/a")
        }
    })
    .catch_unwind()
    .await;
    assert_lookup_panic(
        outcome.expect_err("direct engine cleanup must reject publication"),
        callback_failed,
    );
    assert_eq!(retired.load(Ordering::SeqCst), 1);
    assert_eq!(
        results.load(Ordering::SeqCst),
        usize::from(!callback_failed)
    );
    if let Some(engine) = actix {
        assert!(engine.resolve_actor("parrot://node/user/a").is_none());
        engine.shutdown().await.unwrap();
    }
    if let Some(engine) = thread {
        assert!(engine.resolve_actor("parrot://node/user/a").is_none());
        engine.shutdown_internal().await.unwrap();
    }
}
macro_rules! direct_failure_case {
    ($name:ident, $actix:expr, $callback:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                actix::System::new().block_on(direct_gateway_owner_failure($actix, $callback));
            });
        }
    };
}
direct_failure_case!(
    thread_direct_gateway_callback_owner_panics_are_independent,
    false,
    true
);
direct_failure_case!(
    actix_direct_gateway_callback_owner_panics_are_independent,
    true,
    true
);
direct_failure_case!(
    thread_direct_gateway_owner_result_panics_are_independent,
    false,
    false
);
direct_failure_case!(
    actix_direct_gateway_owner_result_panics_are_independent,
    true,
    false
);

async fn mirror_owner_failure(resolver_kind: bool, callback_failed: bool) {
    use futures::FutureExt;
    use parrot::mirror::{MirrorCopier, MirrorPolicy};
    use parrot_api::system::ActorSystem;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let f = facade().await;
    let retired = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(AtomicUsize::new(0));
    let weak = Arc::downgrade(&f);
    let result_count = results.clone();
    let callback = move || {
        assert!(
            weak.upgrade()
                .unwrap()
                .unregister_prefix_handler("/shadow/")
                .unwrap()
        );
        if callback_failed {
            std::panic::panic_any(OriginalLookupPanic);
        }
        let count = result_count.clone();
        Box::new(Ref {
            tag: 9,
            on_clone: None,
            retire: Some(Box::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
                std::panic::panic_any(ThrowingLookupPanic);
            })),
        }) as BoxedActorRef
    };
    if resolver_kind {
        f.register_prefix_resolver(
            "/shadow/",
            Arc::new(Resolver {
                resolve: Box::new(move |path| {
                    assert_eq!(path, "/shadow/key");
                    Some(callback())
                }),
                retire: Some(throwing_retirement(retired.clone())),
            }),
        )
        .unwrap();
    } else {
        f.register_prefix_handler(
            "/shadow/",
            Arc::new(Ref {
                tag: 9,
                on_clone: Some(Box::new(move || {
                    drop(callback());
                })),
                retire: Some(throwing_retirement(retired.clone())),
            }),
        )
        .unwrap();
    }
    f.register_prefix_handler("/primary/", Arc::new(Ref::plain(1)))
        .unwrap();
    f.register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
        .unwrap();
    let handle = f
        .register_mirror_policy(MirrorPolicy::new("/primary/", "/shadow/key"))
        .unwrap();
    let outcome = std::panic::AssertUnwindSafe(async {
        let primary = f
            .get_actor(&ActorPath::placeholder("/primary/key"))
            .await
            .unwrap();
        assert_eq!(
            *primary
                .send(Box::new(41_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            1
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            while handle.stats().delivery_failed != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(retired.load(Ordering::SeqCst), 1);
        assert_eq!(
            results.load(Ordering::SeqCst),
            usize::from(!callback_failed)
        );
        assert_eq!(handle.stats().admitted, 0);
        assert_eq!(handle.stats().worker_failed, 0);
        // A failed lookup must not kill the worker or poison future resolution.
        f.register_prefix_resolver("/shadow/", resolver(7)).unwrap();
        assert_eq!(
            *primary
                .send(Box::new(42_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            1
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            while handle.stats().admitted != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(handle.stats().delivery_failed, 1);
        assert_eq!(handle.stats().worker_failed, 0);
    })
    .catch_unwind()
    .await;
    // Even a failed routing assertion must retire the throwing fixture outside
    // an unwind, so it cannot obscure the failure with a test-created abort.
    handle.shutdown().await;
    let _ = f.unregister_prefix_handler("/shadow/");
    f.shutdown_runtime().await.unwrap();
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}
macro_rules! mirror_failure_case {
    ($name:ident, $resolver:expr, $callback:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                run(mirror_owner_failure($resolver, $callback))
            });
        }
    };
}
mirror_failure_case!(
    mirror_resolver_callback_owner_panics_preserve_primary_and_worker,
    true,
    true
);
mirror_failure_case!(
    mirror_resolver_owner_result_panics_preserve_primary_and_worker,
    true,
    false
);
mirror_failure_case!(
    mirror_static_callback_owner_panics_preserve_primary_and_worker,
    false,
    true
);
mirror_failure_case!(
    mirror_static_clone_owner_panics_preserve_primary_and_worker,
    false,
    false
);

async fn register_remote_engine(facade: &ParrotActorSystem, actix: bool, neutral: bool) {
    if actix {
        let engine = parrot::actix::ActixActorSystem::with_arbiter_count(1)
            .await
            .unwrap();
        if neutral {
            facade
                .register_runtime("engine".into(), Arc::new(engine), false)
                .unwrap();
        } else {
            facade
                .register_actix_system("engine".into(), engine, false)
                .await
                .unwrap();
        }
    } else {
        let engine = ThreadActorSystem::shared(Default::default());
        if neutral {
            facade
                .register_runtime("engine".into(), engine, false)
                .unwrap();
        } else {
            facade
                .register_thread_system("engine".into(), engine, false)
                .await
                .unwrap();
        }
    }
}

#[test]
fn remote_retirement_cannot_overwrite_a_newer_engine_publication() {
    isolated(
        "remote_retirement_cannot_overwrite_a_newer_engine_publication",
        || {
            actix::System::new().block_on(async {
                use std::sync::atomic::{AtomicUsize, Ordering};
                for actix in [false, true] {
                    for neutral in [false, true] {
                        let f = facade().await;
                        register_remote_engine(&f, actix, neutral).await;
                        let calls = Arc::new(AtomicUsize::new(0));
                        let count = calls.clone();
                        let newest: Arc<dyn RemoteGateway> = Arc::new(Remote {
                            lookup: Some(Box::new(move || {
                                count.fetch_add(1, Ordering::SeqCst);
                            })),
                            retire: None,
                        });
                        let check = newest.clone();
                        let weak = Arc::downgrade(&f);
                        f.register_remote_gateway(Arc::new(Remote {
                            lookup: None,
                            retire: Some(Box::new(move || {
                                let facade = weak.upgrade().unwrap();
                                assert_eq!(facade.list_registered_systems().unwrap(), ["engine"]);
                                futures::executor::block_on(
                                    facade.register_remote_gateway(newest.clone()),
                                )
                                .unwrap();
                            })),
                        }))
                        .await
                        .unwrap();
                        f.register_remote_gateway(remote()).await.unwrap();
                        assert!(Arc::ptr_eq(&f.remote_gateway().unwrap(), &check));
                        assert!(
                            f.runtime("engine")
                                .unwrap()
                                .get_actor("parrot://node/user/a")
                                .await
                                .is_none()
                        );
                        assert_eq!(
                            calls.load(Ordering::SeqCst),
                            1,
                            "engine must observe the newer gateway published by a retiring callback"
                        );
                        f.shutdown_runtime().await.unwrap();
                    }
                }
            });
        },
    );
}

#[test]
fn late_engine_remote_retirement_can_reenter_facade_registration() {
    isolated(
        "late_engine_remote_retirement_can_reenter_facade_registration",
        || {
            actix::System::new().block_on(async {
            use std::sync::atomic::{AtomicUsize, Ordering};
            for actix in [false, true] {
                for neutral in [false, true] {
                    let f = facade().await;
                    let calls = Arc::new(AtomicUsize::new(0));
                    let count = calls.clone();
                    let newest: Arc<dyn RemoteGateway> = Arc::new(Remote {
                        lookup: Some(Box::new(move || { count.fetch_add(1, Ordering::SeqCst); })),
                        retire: None,
                    });
                    let weak = Arc::downgrade(&f);
                    let captured = newest.clone();
                    let old: Arc<dyn RemoteGateway> = Arc::new(Remote {
                        lookup: None,
                        retire: Some(Box::new(move || {
                            let facade = weak.upgrade().unwrap();
                            assert_eq!(facade.list_registered_systems().unwrap(), ["late"]);
                            futures::executor::block_on(facade.register_remote_gateway(captured.clone())).unwrap();
                        })),
                    });
                    f.register_remote_gateway(remote()).await.unwrap();
                    if actix {
                        let engine = parrot::actix::ActixActorSystem::with_arbiter_count(1).await.unwrap();
                        engine.set_remote_gateway(old);
                        if neutral {
                            f.register_runtime("late".into(), Arc::new(engine), false).unwrap();
                        } else {
                            f.register_actix_system("late".into(), engine, false).await.unwrap();
                        }
                    } else {
                        let engine = ThreadActorSystem::shared(Default::default());
                        engine.set_remote_gateway(old);
                        if neutral {
                            f.register_runtime("late".into(), engine, false).unwrap();
                        } else {
                            f.register_thread_system("late".into(), engine, false).await.unwrap();
                        }
                    }
                    assert!(Arc::ptr_eq(&f.remote_gateway().unwrap(), &newest));
                    assert!(f.runtime("late").unwrap().get_actor("parrot://node/user/a").await.is_none());
                    assert_eq!(calls.load(Ordering::SeqCst), 1,
                        "late engine registration must propagate and retire gateways outside facade locks");
                    f.shutdown_runtime().await.unwrap();
                }
            }
        });
        },
    );
}

#[test]
fn remote_propagation_retires_all_displaced_gateways_even_when_one_panics() {
    isolated(
        "remote_propagation_retires_all_displaced_gateways_even_when_one_panics",
        || {
            actix::System::new().block_on(async {
                use std::sync::atomic::{AtomicUsize, Ordering};
                let f = facade().await;
                let thread = ThreadActorSystem::shared(Default::default());
                let actix = parrot::actix::ActixActorSystem::with_arbiter_count(1)
                    .await
                    .unwrap();
                f.register_thread_system("thread".into(), thread.clone(), false)
                    .await
                    .unwrap();
                f.register_actix_system("actix".into(), actix.clone(), false)
                    .await
                    .unwrap();
                let retired = Arc::new(AtomicUsize::new(0));
                let gateway = |panic: bool| {
                    let retired = retired.clone();
                    let weak = Arc::downgrade(&f);
                    Arc::new(Remote {
                        lookup: None,
                        retire: Some(Box::new(move || {
                            assert_eq!(
                                weak.upgrade()
                                    .unwrap()
                                    .list_registered_systems()
                                    .unwrap()
                                    .len(),
                                2
                            );
                            retired.fetch_add(1, Ordering::SeqCst);
                            assert!(!panic, "controlled retired remote gateway panic");
                        })),
                    }) as Arc<dyn RemoteGateway>
                };
                thread.set_remote_gateway(gateway(true));
                actix.set_remote_gateway(gateway(false));
                assert!(
                    f.register_remote_gateway(remote()).await.is_err(),
                    "retired remote panic must remain visible in the publication receipt"
                );
                assert_eq!(
                    retired.load(Ordering::SeqCst),
                    2,
                    "a retired remote panic must not abandon sibling gateway disposal"
                );
                f.register_remote_gateway(remote()).await.unwrap();
                f.shutdown_runtime().await.unwrap();
            });
        },
    );
}
#[path = "support/isolated.rs"]
mod isolated_child;

fn isolated(name: &str, body: impl FnOnce()) {
    isolated_child::run(name, "PARROT_FACADE_CASE", Duration::from_secs(8), body);
}
fn run(f: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}
async fn facade() -> Arc<ParrotActorSystem> {
    Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap())
}
struct Remote {
    lookup: Option<Callback>,
    retire: Option<Callback>,
}
impl RemoteGateway for Remote {
    fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        if let Some(callback) = &self.lookup {
            callback();
        }
        None
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        if let Some(callback) = &self.retire {
            callback();
        }
    }
}
fn remote() -> Arc<dyn RemoteGateway> {
    Arc::new(Remote {
        lookup: None,
        retire: None,
    })
}
struct Receptionist {
    retire: Option<Callback>,
}
#[async_trait::async_trait]
impl ReceptionistGateway for Receptionist {
    async fn register(&self, _: ReceptionistKey, _: String) -> ActorResult<()> {
        Ok(())
    }
    async fn deregister(&self, _: ReceptionistKey, _: String) -> ActorResult<()> {
        Ok(())
    }
    async fn subscribe(&self, _: ReceptionistKey) -> ActorResult<ReceptionistStream> {
        Ok(tokio::sync::mpsc::channel(1).1.into())
    }
}
impl Drop for Receptionist {
    fn drop(&mut self) {
        if let Some(callback) = &self.retire {
            callback();
        }
    }
}
fn receptionist() -> Arc<dyn ReceptionistGateway> {
    Arc::new(Receptionist { retire: None })
}
struct Ref {
    tag: u64,
    on_clone: Option<Callback>,
    retire: Option<Callback>,
}
impl Ref {
    fn plain(tag: u64) -> Self {
        Self {
            tag,
            on_clone: None,
            retire: None,
        }
    }
}
impl std::fmt::Debug for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Ref").field(&self.tag).finish()
    }
}
impl Drop for Ref {
    fn drop(&mut self) {
        if let Some(callback) = &self.retire {
            callback();
        }
    }
}
impl ActorRef for Ref {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(Box::new(self.tag) as BoxedMessage) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        format!("ref-{}", self.tag)
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        if let Some(callback) = &self.on_clone {
            callback();
        }
        Box::new(Self::plain(self.tag))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[test]
fn remote_lookup_can_replace_its_gateway() {
    isolated("remote_lookup_can_replace_its_gateway", || {
        run(async {
            let f = facade().await;
            let weak = Arc::downgrade(&f);
            f.register_remote_gateway(Arc::new(Remote {
                retire: None,
                lookup: Some(Box::new(move || {
                    futures::executor::block_on(
                        weak.upgrade().unwrap().register_remote_gateway(remote()),
                    )
                    .unwrap();
                })),
            }))
            .await
            .unwrap();
            assert!(
                f.internal_get_actor(&ActorPath::placeholder("parrot://node/user/a"))
                    .await
                    .is_none()
            );
            f.register_remote_gateway(remote()).await.unwrap();
        })
    });
}
#[test]
fn remote_retirement_can_reenter_lookup() {
    isolated("remote_retirement_can_reenter_lookup", || {
        run(async {
            let f = facade().await;
            let weak = Arc::downgrade(&f);
            f.register_remote_gateway(Arc::new(Remote {
                lookup: None,
                retire: Some(Box::new(move || {
                    assert!(
                        futures::executor::block_on(
                            weak.upgrade()
                                .unwrap()
                                .internal_get_actor(&ActorPath::placeholder(
                                    "parrot://node/user/a"
                                ))
                        )
                        .is_none()
                    );
                })),
            }))
            .await
            .unwrap();
            f.register_remote_gateway(remote()).await.unwrap();
        })
    });
}
#[test]
fn facade_receptionist_retirement_can_query_current_gateway() {
    isolated(
        "facade_receptionist_retirement_can_query_current_gateway",
        || {
            run(async {
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                f.register_receptionist_gateway(Arc::new(Receptionist {
                    retire: Some(Box::new(move || {
                        assert!(weak.upgrade().unwrap().receptionist_gateway().is_some());
                    })),
                }))
                .await
                .unwrap();
                f.register_receptionist_gateway(receptionist())
                    .await
                    .unwrap();
            })
        },
    );
}
#[test]
fn thread_receptionist_retirement_can_query_current_gateway() {
    isolated(
        "thread_receptionist_retirement_can_query_current_gateway",
        || {
            run(async {
                let t = ThreadActorSystem::shared(Default::default());
                let weak = Arc::downgrade(&t);
                t.set_receptionist_gateway(Arc::new(Receptionist {
                    retire: Some(Box::new(move || {
                        assert!(weak.upgrade().unwrap().receptionist_gateway().is_some());
                    })),
                }));
                t.set_receptionist_gateway(receptionist());
                t.shutdown_internal().await.unwrap();
            })
        },
    );
}
#[test]
fn inherited_gateway_releases_old_capture_outside_facade_system_lock() {
    isolated(
        "inherited_gateway_releases_old_capture_outside_facade_system_lock",
        || {
            run(async {
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                let t = ThreadActorSystem::shared(Default::default());
                t.set_receptionist_gateway(Arc::new(Receptionist {
                    retire: Some(Box::new(move || {
                        assert_eq!(
                            weak.upgrade().unwrap().list_registered_systems().unwrap(),
                            ["thread"]
                        );
                    })),
                }));
                f.register_receptionist_gateway(receptionist())
                    .await
                    .unwrap();
                f.register_thread_system("thread".into(), t.clone(), true)
                    .await
                    .unwrap();
                t.shutdown_internal().await.unwrap();
            })
        },
    );
}
#[test]
fn prefix_clone_can_register_another_prefix() {
    isolated("prefix_clone_can_register_another_prefix", || {
        run(async {
            let f = facade().await;
            let weak = Arc::downgrade(&f);
            f.register_prefix_handler(
                "/original/",
                Arc::new(Ref {
                    on_clone: Some(Box::new(move || {
                        weak.upgrade()
                            .unwrap()
                            .register_prefix_handler("/new/", Arc::new(Ref::plain(2)))
                            .unwrap();
                    })),
                    tag: 1,
                    retire: None,
                }),
            )
            .unwrap();
            assert_eq!(
                f.internal_get_actor(&ActorPath::placeholder("/original/x"))
                    .await
                    .unwrap()
                    .path(),
                "ref-1"
            );
            assert_eq!(
                f.internal_get_actor(&ActorPath::placeholder("/new/x"))
                    .await
                    .unwrap()
                    .path(),
                "ref-2"
            );
        })
    });
}
struct ConvertedPrefix(Callback);
impl From<ConvertedPrefix> for String {
    fn from(p: ConvertedPrefix) -> Self {
        (p.0)();
        "/converted/".into()
    }
}
#[test]
fn prefix_conversion_does_not_run_under_registry_lock() {
    isolated("prefix_conversion_does_not_run_under_registry_lock", || {
        run(async {
            let f = facade().await;
            let weak = Arc::downgrade(&f);
            f.register_prefix_handler(
                ConvertedPrefix(Box::new(move || {
                    weak.upgrade()
                        .unwrap()
                        .register_prefix_handler("/nested/", Arc::new(Ref::plain(2)))
                        .unwrap();
                })),
                Arc::new(Ref::plain(1)),
            )
            .unwrap();
            assert_eq!(
                f.internal_get_actor(&ActorPath::placeholder("/nested/a"))
                    .await
                    .unwrap()
                    .path(),
                "ref-2"
            );
        })
    });
}
#[test]
fn same_prefix_registration_replaces_stale_routing() {
    isolated("same_prefix_registration_replaces_stale_routing", || {
        run(async {
            let f = facade().await;
            f.register_prefix_handler("/entity-", Arc::new(Ref::plain(1)))
                .unwrap();
            let old = f
                .internal_get_actor(&ActorPath::placeholder("/entity-a"))
                .await
                .unwrap();
            f.register_prefix_handler("/entity-", Arc::new(Ref::plain(2)))
                .unwrap();
            assert_eq!(
                f.internal_get_actor(&ActorPath::placeholder("/entity-a"))
                    .await
                    .unwrap()
                    .path(),
                "ref-2"
            );
            assert_eq!(old.path(), "ref-1");
        })
    });
}

#[test]
fn reentrant_retirement_cannot_publish_an_older_gateway_after_a_newer_one() {
    isolated(
        "reentrant_retirement_cannot_publish_an_older_gateway_after_a_newer_one",
        || {
            run(async {
                let f = facade().await;
                let t = ThreadActorSystem::shared(Default::default());
                f.register_thread_system("thread".into(), t.clone(), true)
                    .await
                    .unwrap();
                let newest = receptionist();
                let check = newest.clone();
                let weak = Arc::downgrade(&f);
                f.register_receptionist_gateway(Arc::new(Receptionist {
                    retire: Some(Box::new(move || {
                        futures::executor::block_on(
                            weak.upgrade()
                                .unwrap()
                                .register_receptionist_gateway(newest.clone()),
                        )
                        .unwrap();
                    })),
                }))
                .await
                .unwrap();
                // Keep the retiring callback solely in the facade. A naive fix
                // that drops it before propagating the older update would let the
                // nested newer publication finish, then overwrite the engine again.
                t.set_receptionist_gateway(receptionist());
                f.register_receptionist_gateway(receptionist())
                    .await
                    .unwrap();
                assert!(Arc::ptr_eq(&f.receptionist_gateway().unwrap(), &check));
                assert!(Arc::ptr_eq(&t.receptionist_gateway().unwrap(), &check));
                let late = ThreadActorSystem::shared(Default::default());
                f.register_thread_system("late".into(), late.clone(), false)
                    .await
                    .unwrap();
                assert!(Arc::ptr_eq(&late.receptionist_gateway().unwrap(), &check));
                t.shutdown_internal().await.unwrap();
                late.shutdown_internal().await.unwrap();
            })
        },
    );
}
#[test]
fn gateway_retirement_finishes_every_capture_after_panics_and_keeps_committed_state() {
    isolated(
        "gateway_retirement_finishes_every_capture_after_panics_and_keeps_committed_state",
        || {
            run(async {
                use std::sync::atomic::{AtomicUsize, Ordering};
                let f = facade().await;
                let a = ThreadActorSystem::shared(Default::default());
                let b = ThreadActorSystem::shared(Default::default());
                f.register_thread_system("a".into(), a.clone(), true)
                    .await
                    .unwrap();
                f.register_thread_system("b".into(), b.clone(), false)
                    .await
                    .unwrap();
                f.register_receptionist_gateway(receptionist())
                    .await
                    .unwrap();
                let count = Arc::new(AtomicUsize::new(0));
                for t in [&a, &b] {
                    let count = count.clone();
                    let weak = Arc::downgrade(&f);
                    t.set_receptionist_gateway(Arc::new(Receptionist {
                        retire: Some(Box::new(move || {
                            assert_eq!(
                                weak.upgrade()
                                    .unwrap()
                                    .list_registered_systems()
                                    .unwrap()
                                    .len(),
                                2
                            );
                            count.fetch_add(1, Ordering::SeqCst);
                            panic!("old engine gateway destructor");
                        })),
                    }));
                }
                let newest = receptionist();
                let err = f
                    .register_receptionist_gateway(newest.clone())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("registration committed"));
                assert_eq!(count.load(Ordering::SeqCst), 2);
                for gw in [
                    f.receptionist_gateway(),
                    a.receptionist_gateway(),
                    b.receptionist_gateway(),
                ] {
                    assert!(Arc::ptr_eq(&gw.unwrap(), &newest));
                }
                f.register_receptionist_gateway(receptionist())
                    .await
                    .unwrap();
                a.shutdown_internal().await.unwrap();
                b.shutdown_internal().await.unwrap();
            })
        },
    );
}
#[test]
fn concurrent_gateway_updates_and_engine_registration_converge_to_one_committed_owner() {
    isolated(
        "concurrent_gateway_updates_and_engine_registration_converge_to_one_committed_owner",
        || {
            run(async {
                let f = facade().await;
                let engines: Vec<_> = (0..8)
                    .map(|_| ThreadActorSystem::shared(Default::default()))
                    .collect();
                let barrier = Arc::new(std::sync::Barrier::new(10));
                let mut joins = vec![];
                for (i, engine) in engines.iter().enumerate() {
                    let f = f.clone();
                    let barrier = barrier.clone();
                    let engine = engine.clone();
                    joins.push(std::thread::spawn(move || {
                        barrier.wait();
                        futures::executor::block_on(f.register_thread_system(
                            format!("engine-{i}"),
                            engine,
                            false,
                        ))
                        .unwrap();
                    }));
                }
                for _ in 0..2 {
                    let f = f.clone();
                    let barrier = barrier.clone();
                    joins.push(std::thread::spawn(move || {
                        barrier.wait();
                        for _ in 0..32 {
                            futures::executor::block_on(
                                f.register_receptionist_gateway(receptionist()),
                            )
                            .unwrap();
                        }
                    }));
                }
                for j in joins {
                    j.join().unwrap();
                }
                let current = f.receptionist_gateway().unwrap();
                for engine in engines {
                    assert!(Arc::ptr_eq(
                        &current,
                        &engine.receptionist_gateway().unwrap()
                    ));
                    engine.shutdown_internal().await.unwrap();
                }
            })
        },
    );
}
#[test]
fn replaced_thread_system_retires_user_captures_outside_system_registry() {
    isolated(
        "replaced_thread_system_retires_user_captures_outside_system_registry",
        || {
            run(async {
                use std::sync::atomic::{AtomicUsize, Ordering};
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                let count = Arc::new(AtomicUsize::new(0));
                let dropped = count.clone();
                let old = ThreadActorSystem::shared(Default::default());
                old.set_receptionist_gateway(Arc::new(Receptionist {
                    retire: Some(Box::new(move || {
                        assert_eq!(
                            weak.upgrade().unwrap().list_registered_systems().unwrap(),
                            ["engine"]
                        );
                        dropped.fetch_add(1, Ordering::SeqCst);
                    })),
                }));
                // Explicitly retire worker resources, then relinquish the last system
                // owner to the facade. Its fields still own the gateway capture.
                old.shutdown_internal().await.unwrap();
                f.register_thread_system("engine".into(), old, true)
                    .await
                    .unwrap();
                let new = ThreadActorSystem::shared(Default::default());
                f.register_thread_system("engine".into(), new.clone(), true)
                    .await
                    .unwrap();
                assert_eq!(count.load(Ordering::SeqCst), 1);
                new.shutdown_internal().await.unwrap();
            })
        },
    );
}
#[test]
fn prefix_replacement_and_removal_retire_outside_lock_and_preserve_fallback() {
    isolated(
        "prefix_replacement_and_removal_retire_outside_lock_and_preserve_fallback",
        || {
            run(async {
                use std::sync::atomic::{AtomicUsize, Ordering};
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                let count = Arc::new(AtomicUsize::new(0));
                let dropped = count.clone();
                f.register_prefix_handler("/entity/", Arc::new(Ref::plain(1)))
                    .unwrap();
                f.register_prefix_handler(
                    "/entity/deep/",
                    Arc::new(Ref {
                        tag: 2,
                        on_clone: None,
                        retire: Some(Box::new(move || {
                            let f = weak.upgrade().unwrap();
                            assert!(!f.unregister_prefix_handler("/missing/").unwrap());
                            dropped.fetch_add(1, Ordering::SeqCst);
                        })),
                    }),
                )
                .unwrap();
                let captured = f
                    .internal_get_actor(&ActorPath::placeholder("/entity/deep/a"))
                    .await
                    .unwrap();
                assert_eq!(captured.path(), "ref-2");
                assert!(f.unregister_prefix_handler("/entity/deep/").unwrap());
                assert_eq!(count.load(Ordering::SeqCst), 1);
                assert!(!f.unregister_prefix_handler("/entity/deep/").unwrap());
                assert_eq!(
                    f.internal_get_actor(&ActorPath::placeholder("/entity/deep/a"))
                        .await
                        .unwrap()
                        .path(),
                    "ref-1"
                );
                assert!(
                    f.internal_get_actor(&ActorPath::placeholder("/entity-other"))
                        .await
                        .is_none()
                );
                assert_eq!(captured.path(), "ref-2");
            })
        },
    );
}
#[test]
fn remote_and_prefix_disposal_panics_do_not_poison_committed_registrations() {
    isolated(
        "remote_and_prefix_disposal_panics_do_not_poison_committed_registrations",
        || {
            run(async {
                let f = facade().await;
                f.register_remote_gateway(Arc::new(Remote {
                    lookup: None,
                    retire: Some(Box::new(|| panic!("old remote"))),
                }))
                .await
                .unwrap();
                assert!(
                    f.register_remote_gateway(remote())
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("registration committed")
                );
                f.register_remote_gateway(remote()).await.unwrap();
                f.register_prefix_handler(
                    "/prefix/",
                    Arc::new(Ref {
                        tag: 1,
                        on_clone: None,
                        retire: Some(Box::new(|| panic!("old prefix"))),
                    }),
                )
                .unwrap();
                assert!(
                    f.register_prefix_handler("/prefix/", Arc::new(Ref::plain(2)))
                        .unwrap_err()
                        .to_string()
                        .contains("registration committed")
                );
                assert_eq!(
                    f.internal_get_actor(&ActorPath::placeholder("/prefix/a"))
                        .await
                        .unwrap()
                        .path(),
                    "ref-2"
                );
                assert!(f.unregister_prefix_handler("/prefix/").unwrap());
            })
        },
    );
}
#[test]
fn mirror_shadow_prefix_clone_can_reenter_the_registry() {
    isolated(
        "mirror_shadow_prefix_clone_can_reenter_the_registry",
        || {
            run(async {
                use parrot::mirror::{MirrorCopier, MirrorPolicy};
                use parrot_api::system::ActorSystem;
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                let (tx, rx) = tokio::sync::oneshot::channel();
                let sent = std::sync::Mutex::new(Some(tx));
                f.register_prefix_handler("/primary/", Arc::new(Ref::plain(1)))
                    .unwrap();
                f.register_prefix_handler(
                    "/shadow/",
                    Arc::new(Ref {
                        tag: 2,
                        retire: None,
                        on_clone: Some(Box::new(move || {
                            weak.upgrade()
                                .unwrap()
                                .register_prefix_handler("/from-shadow/", Arc::new(Ref::plain(3)))
                                .unwrap();
                            if let Some(tx) = sent.lock().unwrap().take() {
                                tx.send(()).unwrap();
                            }
                        })),
                    }),
                )
                .unwrap();
                f.register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
                    .unwrap();
                let handle = f
                    .register_mirror_policy(MirrorPolicy::new("/primary/", "/shadow/target"))
                    .unwrap();
                let primary = f
                    .get_actor(&ActorPath::placeholder("/primary/a"))
                    .await
                    .unwrap();
                assert_eq!(
                    *primary
                        .send(Box::new(7_u64))
                        .await
                        .unwrap()
                        .downcast::<u64>()
                        .unwrap(),
                    1
                );
                tokio::time::timeout(Duration::from_secs(3), rx)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    f.internal_get_actor(&ActorPath::placeholder("/from-shadow/a"))
                        .await
                        .unwrap()
                        .path(),
                    "ref-3"
                );
                handle.shutdown().await;
            })
        },
    );
}

type ResolveCallback = Box<dyn Fn(&str) -> Option<BoxedActorRef> + Send + Sync>;
struct Resolver {
    resolve: ResolveCallback,
    retire: Option<Callback>,
}
impl PrefixResolver for Resolver {
    fn resolve(&self, path: &str) -> Option<BoxedActorRef> {
        (self.resolve)(path)
    }
}
impl Drop for Resolver {
    fn drop(&mut self) {
        if let Some(callback) = &self.retire {
            callback();
        }
    }
}
fn resolver(tag: u64) -> Arc<dyn PrefixResolver> {
    Arc::new(Resolver {
        resolve: Box::new(move |_| Some(Box::new(Ref::plain(tag)))),
        retire: None,
    })
}

#[test]
fn prefix_resolution_and_last_owner_retirement_can_reenter() {
    isolated(
        "prefix_resolution_and_last_owner_retirement_can_reenter",
        || {
            run(async {
                use std::sync::atomic::{AtomicUsize, Ordering};
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                let retired = Arc::new(AtomicUsize::new(0));
                let count = retired.clone();
                let drop_weak = weak.clone();
                f.register_prefix_resolver(
                    "/entity/",
                    Arc::new(Resolver {
                        resolve: Box::new(move |path| {
                            assert_eq!(path, "/entity/a%2fb");
                            // Removal must not destroy the resolver during its callback.
                            assert!(
                                weak.upgrade()
                                    .unwrap()
                                    .unregister_prefix_handler("/entity/")
                                    .unwrap()
                            );
                            assert_eq!(retired.load(Ordering::SeqCst), 0);
                            Some(Box::new(Ref::plain(1)))
                        }),
                        retire: Some(Box::new(move || {
                            drop_weak
                                .upgrade()
                                .unwrap()
                                .register_prefix_resolver("/replacement/", resolver(2))
                                .unwrap();
                            count.fetch_add(1, Ordering::SeqCst);
                        })),
                    }),
                )
                .unwrap();
                assert_eq!(
                    f.internal_get_actor(&ActorPath::placeholder("/entity/a%2fb"))
                        .await
                        .unwrap()
                        .path(),
                    "ref-1"
                );
                assert_eq!(
                    f.internal_get_actor(&ActorPath::placeholder("/replacement/a"))
                        .await
                        .unwrap()
                        .path(),
                    "ref-2"
                );
                assert!(
                    f.internal_get_actor(&ActorPath::placeholder("/entity/a%2fb"))
                        .await
                        .is_none()
                );
            });
        },
    );
}

#[test]
fn resolver_replacement_shares_static_prefix_precedence_and_keeps_captured_refs() {
    isolated(
        "resolver_replacement_shares_static_prefix_precedence_and_keeps_captured_refs",
        || {
            run(async {
                let f = facade().await;
                f.register_prefix_handler("/entity/", Arc::new(Ref::plain(1)))
                    .unwrap();
                f.register_prefix_resolver("/entity/deep/", resolver(2))
                    .unwrap();
                let path = ActorPath::placeholder("/entity/deep/a");
                let captured = f.internal_get_actor(&path).await.unwrap();
                assert_eq!(captured.path(), "ref-2");
                f.register_prefix_handler("/entity/deep/", Arc::new(Ref::plain(3)))
                    .unwrap();
                assert_eq!(f.internal_get_actor(&path).await.unwrap().path(), "ref-3");
                f.register_prefix_resolver("/entity/deep/", resolver(4))
                    .unwrap();
                assert_eq!(f.internal_get_actor(&path).await.unwrap().path(), "ref-4");
                f.register_prefix_resolver(
                    "/entity/deep/",
                    Arc::new(Resolver {
                        resolve: Box::new(|_| None),
                        retire: None,
                    }),
                )
                .unwrap();
                assert!(
                    f.internal_get_actor(&path).await.is_none(),
                    "rejection must not reach the broader static handler"
                );
                assert!(f.unregister_prefix_handler("/entity/deep/").unwrap());
                assert_eq!(f.internal_get_actor(&path).await.unwrap().path(), "ref-1");
                assert_eq!(
                    *captured
                        .send(Box::new(()))
                        .await
                        .unwrap()
                        .downcast::<u64>()
                        .unwrap(),
                    2
                );
            });
        },
    );
}

#[test]
fn resolver_panic_and_retirement_panic_do_not_poison_registry() {
    isolated(
        "resolver_panic_and_retirement_panic_do_not_poison_registry",
        || {
            run(async {
                use futures::FutureExt;
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                f.register_prefix_resolver(
                    "/entity/",
                    Arc::new(Resolver {
                        resolve: Box::new(|_| panic!("resolver callback")),
                        retire: Some(Box::new(move || {
                            assert!(
                                !weak
                                    .upgrade()
                                    .unwrap()
                                    .unregister_prefix_handler("/absent/")
                                    .unwrap()
                            );
                            panic!("resolver retirement");
                        })),
                    }),
                )
                .unwrap();
                let path = ActorPath::placeholder("/entity/a");
                assert!(
                    std::panic::AssertUnwindSafe(f.internal_get_actor(&path))
                        .catch_unwind()
                        .await
                        .is_err()
                );
                let error = f
                    .register_prefix_resolver("/entity/", resolver(2))
                    .unwrap_err();
                assert!(error.to_string().contains("registration committed"));
                assert_eq!(f.internal_get_actor(&path).await.unwrap().path(), "ref-2");
                assert!(f.unregister_prefix_handler("/entity/").unwrap());
            });
        },
    );
}

#[test]
fn resolver_conversion_and_rejected_owner_retire_outside_lock() {
    isolated(
        "resolver_conversion_and_rejected_owner_retire_outside_lock",
        || {
            run(async {
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                let drop_weak = weak.clone();
                struct Prefix(Callback);
                impl From<Prefix> for String {
                    fn from(prefix: Prefix) -> Self {
                        (prefix.0)();
                        "/invalid".to_owned()
                    }
                }
                assert!(
                    f.register_prefix_resolver(
                        Prefix(Box::new(move || {
                            weak.upgrade()
                                .unwrap()
                                .register_prefix_resolver("/nested/", resolver(1))
                                .unwrap();
                        })),
                        Arc::new(Resolver {
                            resolve: Box::new(|_| unreachable!()),
                            retire: Some(Box::new(move || {
                                drop_weak
                                    .upgrade()
                                    .unwrap()
                                    .register_prefix_handler("/retired/", Arc::new(Ref::plain(2)))
                                    .unwrap();
                            })),
                        })
                    )
                    .is_err()
                );
                for (path, expected) in [("/nested/a", "ref-1"), ("/retired/a", "ref-2")] {
                    assert_eq!(
                        f.internal_get_actor(&ActorPath::placeholder(path))
                            .await
                            .unwrap()
                            .path(),
                        expected
                    );
                }
            });
        },
    );
}

#[test]
fn mirror_shadow_resolver_receives_exact_path_and_can_reenter() {
    isolated(
        "mirror_shadow_resolver_receives_exact_path_and_can_reenter",
        || {
            run(async {
                use parrot::mirror::{MirrorCopier, MirrorPolicy};
                use parrot_api::system::ActorSystem;
                let f = facade().await;
                let weak = Arc::downgrade(&f);
                let (tx, rx) = tokio::sync::oneshot::channel();
                let sent = std::sync::Mutex::new(Some(tx));
                f.register_prefix_handler("/primary/", Arc::new(Ref::plain(1)))
                    .unwrap();
                f.register_prefix_resolver(
                    "/shadow/",
                    Arc::new(Resolver {
                        resolve: Box::new(move |path| {
                            assert_eq!(path, "/shadow/exact%2fkey");
                            assert!(
                                weak.upgrade()
                                    .unwrap()
                                    .unregister_prefix_handler("/shadow/")
                                    .unwrap()
                            );
                            sent.lock().unwrap().take().unwrap().send(()).unwrap();
                            Some(Box::new(Ref::plain(2)))
                        }),
                        retire: None,
                    }),
                )
                .unwrap();
                f.register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
                    .unwrap();
                let handle = f
                    .register_mirror_policy(MirrorPolicy::new("/primary/", "/shadow/exact%2fkey"))
                    .unwrap();
                let reference = f
                    .get_actor(&ActorPath::placeholder("/primary/a"))
                    .await
                    .unwrap();
                assert_eq!(
                    *reference
                        .send(Box::new(7_u64))
                        .await
                        .unwrap()
                        .downcast::<u64>()
                        .unwrap(),
                    1
                );
                tokio::time::timeout(Duration::from_secs(3), rx)
                    .await
                    .unwrap()
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(3), async {
                    while handle.stats().admitted != 1 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                handle.shutdown().await;
                assert_eq!(handle.stats().admitted, 1);
            });
        },
    );
}

struct CopierRetirement(Callback);
impl Drop for CopierRetirement {
    fn drop(&mut self) {
        (self.0)();
    }
}
#[test]
fn mirror_copier_replacement_destructor_can_reenter() {
    isolated("mirror_copier_replacement_destructor_can_reenter", || {
        run(async {
            use parrot::mirror::MirrorCopier;
            let f = facade().await;
            let weak = Arc::downgrade(&f);
            let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let observed = called.clone();
            let capture = CopierRetirement(Box::new(move || {
                weak.upgrade()
                    .unwrap()
                    .register_mirror_copier(MirrorCopier::cloned::<u32>(|_| 4))
                    .unwrap();
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
            }));
            f.register_mirror_copier(MirrorCopier::cloned::<u64>(move |_| {
                let _ = &capture;
                8
            }))
            .unwrap();
            f.register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
                .unwrap();
            assert!(called.load(std::sync::atomic::Ordering::SeqCst));
            Arc::try_unwrap(f)
                .ok()
                .unwrap()
                .internal_shutdown()
                .await
                .unwrap();
        })
    });
}
#[test]
fn mirror_copier_replacement_panic_does_not_poison_registry() {
    isolated(
        "mirror_copier_replacement_panic_does_not_poison_registry",
        || {
            run(async {
                use parrot::mirror::MirrorCopier;
                let f = facade().await;
                let capture =
                    CopierRetirement(Box::new(|| panic!("copier capture disposal failed")));
                f.register_mirror_copier(MirrorCopier::cloned::<u64>(move |_| {
                    let _ = &capture;
                    8
                }))
                .unwrap();
                let replaced = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    f.register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))
                }));
                let healthy = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    f.register_mirror_copier(MirrorCopier::cloned::<u32>(|_| 4))
                }));
                // Even a poisoned old registry is disposed before assertions to avoid a
                // second unwind in facade Drop obscuring this behavioral failure.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(f)));
                assert!(
                    matches!(healthy, Ok(Ok(()))),
                    "copier retirement poisoned registry locks"
                );
                assert!(
                    matches!(replaced, Ok(Err(_))),
                    "committed retirement failure must be reported as an error"
                );
            })
        },
    );
}
