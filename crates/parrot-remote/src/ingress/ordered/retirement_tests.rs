// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use crate::{registry::CallbackRegistry, LocalLookup};

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}

async fn open() -> (Arc<Ingress>, SourceKey, Control) {
    let ingress = Arc::new(Ingress::new(
        Arc::new(Empty),
        Arc::new(CallbackRegistry::default()),
    ));
    let mut control = Control {
        source: "writer".into(),
        incarnation: ordered::identity(),
        stream: ordered::identity(),
        ticket: [0; 16],
        operation: Operation::OpenActor,
    };
    let key = SourceKey {
        node: control.source.clone(),
        actor: Some("parrot://end/target".into()),
    };
    control.ticket = ingress
        .open_ordered(key.clone(), control.clone())
        .await
        .unwrap()
        .ticket;
    control.operation = Operation::CloseActor;
    (ingress, key, control)
}

async fn check_other_identity(ingress: &Ingress, key: &SourceKey, original: &Control) {
    let mut changed = original.clone();
    changed.incarnation = ordered::identity();
    let mut other_stream = original.clone();
    other_stream.stream = ordered::identity();
    for stale in [changed, other_stream] {
        assert_eq!(
            ingress.close_ordered_lease(key.clone(), stale).await,
            Err(ErrCode::Forbidden),
            "known ticket received a false retirement proof"
        );
    }
}

#[tokio::test]
async fn challenged_ticket_requires_original_identity_for_retirement() {
    let (ingress, key, control) = open().await;
    check_other_identity(&ingress, &key, &control).await;
    let retry = Control {
        operation: Operation::OpenActor,
        ticket: [0; 16],
        ..control.clone()
    };
    let challenge = ingress.open_ordered(key.clone(), retry).await.unwrap();
    assert!(!challenge.active);
    assert_eq!(challenge.ticket, control.ticket);
    let ack = ingress
        .close_ordered_lease(key.clone(), control.clone())
        .await
        .unwrap();
    assert!(!ack.active);
    assert_eq!(ack.ticket, control.ticket);
    assert!(!ingress.ordered.lock().unwrap().sources.contains_key(&key));
}

#[tokio::test]
async fn active_and_closing_ticket_require_original_identity_until_cleanup() {
    let (ingress, key, control) = open().await;
    let mut commit = control.clone();
    commit.operation = Operation::CommitActor;
    assert!(
        ingress
            .commit_ordered(key.clone(), commit)
            .await
            .unwrap()
            .active
    );
    let lease = ingress.ordered.lock().unwrap().sources[&key]
        .active
        .clone()
        .unwrap();
    check_other_identity(&ingress, &key, &control).await;
    assert!(!lease.cancel.is_cancelled());
    let lookup = lease.lookups.read().await;
    {
        let close = ingress.close_ordered_lease(key.clone(), control.clone());
        tokio::pin!(close);
        tokio::select! {
            early = &mut close => panic!("retired during an admitted lookup: {early:?}"),
            _ = lease.cancel.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("close never cancelled the worker"),
        }
        tokio::time::timeout(
            Duration::from_secs(3),
            check_other_identity(&ingress, &key, &control),
        )
        .await
        .expect("a mismatched close waited for the original owner");
    }
    drop(lookup);
    let ack = tokio::time::timeout(
        Duration::from_secs(3),
        ingress.close_ordered_lease(key.clone(), control.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(lease.is_finished());
    assert!(!ack.active);
    assert_eq!(ack.ticket, control.ticket);
    assert!(
        !ingress
            .close_ordered_lease(key, control)
            .await
            .unwrap()
            .active
    );
}
