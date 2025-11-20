use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Request;
impl parrot_api::message::Message for Request {
    type Result = ();
}
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("sync outcome recursive payload");
    }
}
#[derive(Debug)]
struct Reply {
    drops: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
    recursive: bool,
}
impl std::fmt::Display for Reply {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("unexpected reply diagnostic")
    }
}
impl std::error::Error for Reply {}
impl Drop for Reply {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(Payload(self.payloads.clone()));
        }
        panic!("sync outcome destructor");
    }
}
struct Business {
    result: Option<ActorResult<BoxedMessage>>,
    asynchronous: bool,
}
impl Drop for Business {
    fn drop(&mut self) {
        let _ = crate::owned_payload::PublicationInput::new(self.result.take()).retire();
    }
}
impl ParrotActor for Business {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        self.asynchronous
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("sync handler was bypassed");
    }
}
impl parrot_api::actor::ActixEngineExt for Business {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
        _: EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        self.result.take()
    }
}
async fn unpolled(success: bool, recursive: bool) {
    let drops = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    let reply = Reply {
        drops: drops.clone(),
        payloads: payloads.clone(),
        recursive,
    };
    let result = if success {
        Ok(Box::new(reply) as BoxedMessage)
    } else {
        Err(ActorError::Other(anyhow::Error::new(reply)))
    };
    let mut actor = ActixActor::new(Business {
        result: Some(result),
        asynchronous: false,
    });
    let mut ctx = Context::new();
    actor.ctx = Some(ActixContext::new(
        ctx.address(),
        ActorPath::for_test("/sync-cancellation"),
    ));
    let message = ActixMessageWrapper {
        envelope: parrot_api::message::MessageEnvelope::new(Request, None, None),
    };
    let future = actor.handle_ready(message, &mut ctx);
    assert!(
        actor.inner.as_ref().unwrap().result.is_none(),
        "real sync handler did not run"
    );
    let escaped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future)))
        .err()
        .map(crate::owned_payload::panic_message);
    tokio::time::timeout(Duration::from_secs(3), async {
        while drops.load(Ordering::SeqCst) != 1
            || payloads.load(Ordering::SeqCst) != usize::from(recursive)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        escaped.is_none(),
        "unpolled real sync response escaped: {escaped:?}"
    );
    assert!(actor.control.lifetime.decision_deadline().is_none());
    assert_eq!(
        actor.control.cleanup_failure.lock().unwrap().is_some(),
        success
    );
}
#[actix::test]
async fn unpolled_sync_success_panic() {
    unpolled(true, false).await;
}
#[actix::test]
async fn unpolled_sync_success_recursive() {
    unpolled(true, true).await;
}
#[actix::test]
async fn unpolled_sync_error_panic() {
    unpolled(false, false).await;
}
#[actix::test]
async fn unpolled_sync_error_recursive() {
    unpolled(false, true).await;
}

#[actix::test]
async fn rejected_and_unpolled_dispatch_retire_input() {
    for admission in 0..5 {
        let drops = Arc::new(AtomicUsize::new(0));
        let payloads = Arc::new(AtomicUsize::new(0));
        let mut actor = ActixActor::new(Business {
            result: None,
            asynchronous: admission == 4,
        });
        let mut ctx = Context::new();
        if admission != 0 {
            actor.ctx = Some(ActixContext::new(
                ctx.address(),
                ActorPath::for_test("/input"),
            ));
        }
        if admission == 1 {
            actor.inner = None;
        }
        if admission == 2 {
            actor
                .control
                .request_stop(tokio::time::Instant::now() + Duration::from_secs(1));
        }
        let message = ActixMessageWrapper {
            envelope: parrot_api::message::MessageEnvelope::from_boxed(
                Box::new(Reply {
                    drops: drops.clone(),
                    payloads: payloads.clone(),
                    recursive: true,
                }),
                None,
                Default::default(),
            ),
        };
        // Admission 3 waits in the real Handler's pause future without polling
        // it; the other branches reject in handle_ready before user dispatch.
        let escaped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if admission == 3 {
                drop(actix::Handler::handle(&mut actor, message, &mut ctx));
            } else {
                drop(actor.handle_ready(message, &mut ctx));
            }
        }))
        .err()
        .map(crate::owned_payload::panic_message);
        assert!(escaped.is_none(), "admission {admission}: {escaped:?}");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(payloads.load(Ordering::SeqCst), 1);
    }
}
