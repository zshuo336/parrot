use parrot_api::{
    errors::ActorError,
    receptionist::{
        ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream,
        RegistrationScope,
    },
    types::ActorResult,
};
use parrot_config::receptionist::RegistrationLimits;
use parrot_remote::{acl::AclRules, receptionist::ReceptionistFacade};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
};

struct Hostile(AtomicUsize);
impl Wake for Hostile {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, SeqCst);
        panic!("service lifecycle observer panic");
    }
}

fn events(stream: &mut ReceptionistStream, registered: bool, expected: &[usize]) {
    let mut paths = Vec::new();
    loop {
        match stream.try_recv() {
            Ok(event) => {
                let (key, path, actual_registered) = match event {
                    ReceptionistEvent::Registered { key, remote_path } => (key, remote_path, true),
                    ReceptionistEvent::Unregistered { key, remote_path } => {
                        (key, remote_path, false)
                    }
                };
                assert_eq!(actual_registered, registered);
                assert_eq!(key.as_str(), "edge/retirement");
                paths.push(path);
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(error) => panic!("service event stream closed unexpectedly: {error}"),
        }
    }
    paths.sort();
    let wanted: Vec<_> = expected.iter().map(|id| format!("/actor/{id}")).collect();
    assert_eq!(paths, wanted);
}

async fn lifecycle(joined: bool, replacement: bool) {
    let facade = ReceptionistFacade::with_registration_limits(
        "notification-node",
        RegistrationLimits {
            max_registrations: 4,
            ..Default::default()
        },
    )
    .with_acl(AclRules::permissive());
    let key = ReceptionistKey::new("edge/retirement").unwrap();
    let old = Arc::new(RegistrationScope::default());
    let new = Arc::new(RegistrationScope::default());
    let mut stream = facade.subscribe(key.clone()).await.unwrap();
    for id in 0..4 {
        facade
            .register_scoped(key.clone(), format!("/actor/{id}"), old.clone())
            .await
            .unwrap();
    }
    events(&mut stream, true, &[0, 1, 2, 3]);
    assert!(facade
        .register_scoped(key.clone(), "/actor/4".into(), new.clone())
        .await
        .is_err());
    if replacement {
        for id in 0..2 {
            facade
                .register_scoped(key.clone(), format!("/actor/{id}"), new.clone())
                .await
                .unwrap();
        }
        events(&mut stream, true, &[0, 1]);
    }
    let observer = Arc::new(Hostile(AtomicUsize::new(0)));
    let waker = Waker::from(observer.clone());
    let mut waiting: Pin<Box<dyn Future<Output = ActorResult<()>> + '_>> = if joined {
        Box::pin(old.join())
    } else {
        Box::pin(async {
            old.closed().await;
            Ok(())
        })
    };
    assert!(waiting
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    let retired = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| old.retire()));
    assert!(
        retired.is_ok(),
        "observer interrupted actual service withdrawal"
    );
    assert_eq!(observer.0.load(SeqCst), 1);
    assert!(matches!(
        waiting.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Ready(Ok(()))
    ));
    old.join().await.unwrap();
    drop(waiting);
    events(
        &mut stream,
        false,
        if replacement { &[2, 3] } else { &[0, 1, 2, 3] },
    );
    let mut snapshot = facade.subscribe(key.clone()).await.unwrap();
    events(&mut snapshot, true, if replacement { &[0, 1] } else { &[] });
    assert!(matches!(
        facade
            .register_scoped(key.clone(), "/actor/0".into(), old.clone())
            .await,
        Err(ActorError::Stopped)
    ));
    assert!(matches!(
        facade
            .deregister_scoped(key.clone(), "/actor/0".into(), old.clone())
            .await,
        Err(ActorError::Stopped)
    ));
    for id in if replacement { 2..4 } else { 0..4 } {
        facade
            .register_scoped(key.clone(), format!("/actor/{id}"), new.clone())
            .await
            .unwrap();
    }
    events(
        &mut stream,
        true,
        if replacement { &[2, 3] } else { &[0, 1, 2, 3] },
    );
    new.retire();
    new.join().await.unwrap();
    events(&mut stream, false, &[0, 1, 2, 3]);
    assert_eq!(facade.subscribe(key).await.unwrap().len(), 0);
    assert_eq!(observer.0.load(SeqCst), 1);
}

#[tokio::test]
async fn closed_observer_panic_cannot_leak_services_or_registration_budget() {
    lifecycle(false, false).await;
}

#[tokio::test]
async fn join_observer_panic_keeps_actual_service_retirement_receipt_successful() {
    lifecycle(true, false).await;
}

#[tokio::test]
async fn throwing_old_generation_observer_preserves_replacement_services() {
    lifecycle(false, true).await;
}
