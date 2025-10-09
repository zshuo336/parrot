//! A destructor panic must not strand the rest of a managed typed mailbox.
use parrot::thread::system::ThreadActorSystem;
use parrot_api::{
    message::Message,
    types::{ActorResult, BoxedFuture},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct PanicPayload(Arc<AtomicUsize>);
impl Drop for PanicPayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("payload destructor failed");
    }
}
struct CleanupQueue(tokio::sync::mpsc::UnboundedSender<&'static str>);
impl Drop for CleanupQueue {
    fn drop(&mut self) {
        let _ = self.0.send("actor_drop");
    }
}
enum CleanupCommand {
    Hold,
    Bomb(PanicPayload),
    Tracked(Payload),
}
impl Message for CleanupCommand {
    type Result = ();
}
impl parrot_api::typed::ParrotTypedDispatch for CleanupQueue {
    type Msg = CleanupCommand;
    type Reply = ();
    fn dispatch(&mut self, message: CleanupCommand) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            match message {
                CleanupCommand::Hold => {
                    let _ = self.0.send("held");
                    std::future::pending().await
                }
                CleanupCommand::Bomb(payload) => {
                    drop(payload);
                    Ok(())
                }
                CleanupCommand::Tracked(payload) => {
                    drop(payload);
                    Ok(())
                }
            }
        })
    }
}
impl parrot_api::typed::ParrotMsgVariant<CleanupQueue> for CleanupCommand {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}
#[tokio::test]
async fn panicking_queued_destructor_does_not_strand_following_payloads() {
    let system = ThreadActorSystem::shared(Default::default());
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let actor = system
        .spawn_typed::<CleanupQueue, CleanupCommand>(CleanupQueue(events), "/cleanup")
        .await
        .unwrap();
    actor.tell(CleanupCommand::Hold).await.unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap(),
        "held"
    );
    let dropped = Arc::new(AtomicUsize::new(0));
    actor
        .tell(CleanupCommand::Bomb(PanicPayload(dropped.clone())))
        .await
        .unwrap();
    actor
        .tell(CleanupCommand::Tracked(Payload(dropped.clone())))
        .await
        .unwrap();
    let result = actor.stop().await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("payload destructor failed")
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap(),
        "actor_drop"
    );
    system.shutdown_internal().await.unwrap();
}
