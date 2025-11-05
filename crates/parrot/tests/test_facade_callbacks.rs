//! Public-facade callback and retirement contracts. Bounded child processes turn
//! synchronous deadlocks into failures without abandoning blocked test threads.
use parrot::system::{ParrotActorSystem, ReceptionistGateway, RemoteGateway};
use parrot::thread::system::ThreadActorSystem;
use parrot_api::{
    address::{ActorPath, ActorRef},
    receptionist::{ReceptionistKey, ReceptionistStream},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::sync::Arc;
use std::time::Duration;

type Callback = Box<dyn Fn() + Send + Sync>;
fn isolated(name: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_FACADE_CASE").ok().as_deref() == Some(name) {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_FACADE_CASE", name)
        .spawn()
        .unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child contract failed: {name}: {status}");
            return;
        }
        if std::time::Instant::now() >= end {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("facade callback deadlocked: {name}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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
        Ok(tokio::sync::mpsc::channel(1).1)
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
