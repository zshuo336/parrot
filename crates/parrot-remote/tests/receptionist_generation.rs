use parrot_api::{
    errors::ActorError,
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey, RegistrationScope},
};
use parrot_remote::{acl::AclRules, receptionist::ReceptionistFacade};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn key() -> ReceptionistKey {
    ReceptionistKey::new("edge/generation").unwrap()
}
fn gateway() -> ReceptionistFacade {
    ReceptionistFacade::new("node").with_acl(AclRules::permissive())
}
async fn registered(gateway: &ReceptionistFacade, path: &str) {
    let mut stream = gateway.subscribe(key()).await.unwrap();
    assert_eq!(
        stream
            .try_recv()
            .expect("same-path replacement must remain registered"),
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: path.into()
        }
    );
    assert_eq!(
        stream.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
}
async fn empty(gateway: &ReceptionistFacade) {
    assert_eq!(
        gateway.subscribe(key()).await.unwrap().try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
}

#[tokio::test]
async fn old_owner_cannot_remove_same_path_replacement() {
    let gateway = gateway();
    let old = Arc::new(RegistrationScope::default());
    let new = Arc::new(RegistrationScope::default());
    gateway
        .register_scoped(key(), "/actor".into(), old.clone())
        .await
        .unwrap();
    gateway
        .register_scoped(key(), "/actor".into(), new.clone())
        .await
        .unwrap();
    gateway
        .deregister_scoped(key(), "/actor".into(), old.clone())
        .await
        .unwrap();
    registered(&gateway, "/actor").await;
    old.retire();
    old.join().await.unwrap();
    registered(&gateway, "/actor").await;
    new.retire();
    new.join().await.unwrap();
    empty(&gateway).await;
}

#[tokio::test]
async fn old_cleanup_cannot_remove_same_path_replacement() {
    let gateway = gateway();
    let old = Arc::new(RegistrationScope::default());
    let new = Arc::new(RegistrationScope::default());
    gateway
        .register_scoped(key(), "/actor".into(), old.clone())
        .await
        .unwrap();
    gateway
        .register_scoped(key(), "/actor".into(), new.clone())
        .await
        .unwrap();
    // Do not explicitly deregister old: that would remove its cleanup binding
    // and leave the exact-owner retirement condition untested.
    old.retire();
    old.join().await.unwrap();
    registered(&gateway, "/actor").await;
    new.retire();
    new.join().await.unwrap();
    empty(&gateway).await;
}

#[tokio::test]
async fn manual_republication_survives_previous_actor_retirement() {
    let gateway = gateway();
    let scope = Arc::new(RegistrationScope::default());
    gateway
        .register_scoped(key(), "/actor".into(), scope.clone())
        .await
        .unwrap();
    gateway.register(key(), "/actor".into()).await.unwrap();
    gateway
        .deregister_scoped(key(), "/actor".into(), scope.clone())
        .await
        .unwrap();
    scope.retire();
    scope.join().await.unwrap();
    registered(&gateway, "/actor").await;
    gateway.deregister(key(), "/actor".into()).await.unwrap();
    empty(&gateway).await;
}

#[tokio::test]
async fn retirement_fences_unpolled_calls_and_is_idempotent() {
    let gateway = gateway();
    let scope = Arc::new(RegistrationScope::default());
    gateway
        .register_scoped(key(), "/actor".into(), scope.clone())
        .await
        .unwrap();
    let register = gateway.register_scoped(key(), "/actor".into(), scope.clone());
    let deregister = gateway.deregister_scoped(key(), "/actor".into(), scope.clone());
    let mut events = gateway.subscribe(key()).await.unwrap();
    events.try_recv().unwrap();
    scope.close();
    scope.retire();
    scope.retire();
    scope.join().await.unwrap();
    scope.join().await.unwrap();
    assert!(matches!(register.await, Err(ActorError::Stopped)));
    assert!(matches!(deregister.await, Err(ActorError::Stopped)));
    assert_eq!(
        events.try_recv().unwrap(),
        ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: "/actor".into()
        }
    );
    assert_eq!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    empty(&gateway).await;
}

#[tokio::test]
async fn active_binding_limit_releases_capacity_after_deregistration() {
    let gateway = gateway();
    let scope = Arc::new(RegistrationScope::default());
    for n in 0..RegistrationScope::MAX_BINDINGS {
        gateway
            .register_scoped(key(), format!("/actor/{n}"), scope.clone())
            .await
            .unwrap();
    }
    // An existing binding is valid even at capacity.
    gateway
        .register_scoped(key(), "/actor/0".into(), scope.clone())
        .await
        .unwrap();
    assert!(gateway
        .register_scoped(key(), "/overflow".into(), scope.clone())
        .await
        .is_err());
    gateway
        .deregister_scoped(key(), "/actor/0".into(), scope.clone())
        .await
        .unwrap();
    gateway
        .register_scoped(key(), "/overflow".into(), scope.clone())
        .await
        .unwrap();
    scope.retire();
    scope.join().await.unwrap();
    empty(&gateway).await;
}

#[tokio::test]
async fn cleanup_panics_do_not_skip_other_bindings_or_poison_receipt() {
    let scope = RegistrationScope::default();
    let count = Arc::new(AtomicUsize::new(0));
    for n in 0..4 {
        let count = count.clone();
        scope
            .publish(
                (1, key(), n.to_string()),
                move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    assert_ne!(n, 1, "cleanup panic probe");
                    Ok(())
                },
                || (),
            )
            .unwrap();
    }
    scope.retire();
    assert_eq!(count.load(Ordering::SeqCst), 4);
    assert!(matches!(scope.join().await, Err(ActorError::Panic(_))));
    assert!(matches!(scope.join().await, Err(ActorError::Panic(_))));
    assert!(matches!(scope.with_live(|| ()), Err(ActorError::Stopped)));
}

#[tokio::test]
async fn cleanup_can_reenter_scope_and_registry() {
    let gateway = Arc::new(gateway());
    let scope = Arc::new(RegistrationScope::default());
    gateway
        .register_scoped(key(), "/actor".into(), scope.clone())
        .await
        .unwrap();
    let weak = Arc::downgrade(&scope);
    let table = gateway.clone();
    scope
        .publish(
            (0, key(), "reentry".into()),
            move || {
                assert!(matches!(
                    weak.upgrade().unwrap().with_live(|| ()),
                    Err(ActorError::Stopped)
                ));
                futures::executor::block_on(table.subscribe(key())).unwrap();
                Ok(())
            },
            || (),
        )
        .unwrap();
    scope.retire();
    scope.join().await.unwrap();
    empty(&gateway).await;
}

#[tokio::test]
async fn cleanup_isolates_panicking_callback_and_error_destructors() {
    struct Bomb;
    impl Drop for Bomb {
        fn drop(&mut self) {
            panic!("cleanup capture drop panic");
        }
    }
    #[derive(Debug)]
    struct BadError;
    impl std::fmt::Display for BadError {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("cleanup error format panic");
        }
    }
    impl std::error::Error for BadError {}
    impl Drop for BadError {
        fn drop(&mut self) {
            panic!("cleanup error drop panic");
        }
    }
    let scope = RegistrationScope::default();
    let bomb = Bomb;
    scope
        .publish(
            (1, key(), "capture".into()),
            move || {
                std::hint::black_box(&bomb);
                panic!("cleanup callback panic");
            },
            || (),
        )
        .unwrap();
    scope
        .publish(
            (1, key(), "error".into()),
            || Err(ActorError::Other(BadError.into())),
            || (),
        )
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let retired = count.clone();
    scope
        .publish(
            (1, key(), "last".into()),
            move || {
                retired.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || (),
        )
        .unwrap();
    scope.retire();
    assert!(matches!(scope.join().await, Err(ActorError::Panic(_))));
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retirement_subscriber_wakes_outside_scope_and_table_locks() {
    struct Reentrant {
        gateway: Arc<ReceptionistFacade>,
        scope: std::sync::Weak<RegistrationScope>,
        calls: AtomicUsize,
        panic: bool,
    }
    impl std::task::Wake for Reentrant {
        fn wake(self: Arc<Self>) {
            assert!(matches!(
                self.scope.upgrade().unwrap().with_live(|| ()),
                Err(ActorError::Stopped)
            ));
            futures::executor::block_on(empty(&self.gateway));
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "retirement subscriber wake panic");
        }
    }
    for panic in [false, true] {
        let gateway = Arc::new(gateway());
        let scope = Arc::new(RegistrationScope::default());
        gateway
            .register_scoped(key(), "/actor".into(), scope.clone())
            .await
            .unwrap();
        let mut stream = gateway.subscribe(key()).await.unwrap();
        stream.try_recv().unwrap();
        let callback = Arc::new(Reentrant {
            gateway: gateway.clone(),
            scope: Arc::downgrade(&scope),
            calls: AtomicUsize::new(0),
            panic,
        });
        let waker = std::task::Waker::from(callback.clone());
        assert!(stream
            .poll_recv(&mut std::task::Context::from_waker(&waker))
            .is_pending());
        scope.retire();
        let receipt = scope.join().await;
        if panic {
            assert!(matches!(receipt, Err(ActorError::Panic(_))));
        } else {
            receipt.unwrap();
        }
        assert_eq!(callback.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            stream.try_recv(),
            Ok(ReceptionistEvent::Unregistered { .. })
        ));
        empty(&gateway).await;
    }
}
