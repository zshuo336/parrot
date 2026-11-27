// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot_api::sender::ActorSenderOwner;
use parrot_config::OrderedReceiverState;

/// Each real TCP driver advertises its own protocol version. Both deliver to
/// the same live production ingress, including its receiver identity and grants.
async fn attach(
    source: &RemoteActorSystem,
    receiver: &Arc<RemoteActorSystem>,
    capabilities: u32,
) -> (
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    let mut peer = Raw::connect_with_caps(source, capabilities).await;
    let stop = peer.shutdown.clone();
    let ingress = receiver.ingress.clone();
    let task = tokio::spawn(async move {
        while let Some((frame, back, from)) = peer.incoming.recv().await {
            ingress.dispatch(frame, &back, &from).await;
        }
        drop(peer.handle);
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let links = source.links_snapshot().await;
            if links.last().is_some_and(|(node, sender, _)| {
                node == "wire-source" && sender.peer_capabilities() == capabilities
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    (stop, task)
}

async fn switch_first_hop(disconnect_original: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (target, mut received) = state();
    let receiver = start(
        RemoteConfig::tcp("wire-source", None)
            .with_insecure_development()
            .with_ordered_receiver_state(OrderedReceiverState {
                directory: dir.path().into(),
                realm: "first-hop".into(),
            }),
        Arc::new(Target(target)),
    )
    .await;
    let source = start(
        RemoteConfig::tcp("switching-source", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Empty),
    )
    .await;
    let old = caps::BIN
        | caps::ORDERED_TELL
        | caps::TELL_COMPLETION
        | caps::ORDERED_ADMISSION
        | caps::ORDERED_ACTOR
        | caps::ORDERED_RECOVERY;
    let (stop_modern, modern_task) = attach(&source, &receiver, old | caps::ORDERED_SENDER).await;
    let reference = source.remote_ref("parrot://wire-source/target").unwrap();
    let owner = ActorSenderOwner::new("/user/writer");
    let actor = owner.sender();
    actor.scope(strict(&reference, 1)).await;
    seen(&mut received, 1).await;
    let (stop_old, old_task) = attach(&source, &receiver, old).await;
    let timeout = Duration::from_secs(3);
    let wait_for_old_link = || async {
        tokio::time::timeout(timeout, async {
            loop {
                let links = source.links_snapshot().await;
                if links.len() == 1 && links[0].1.peer_capabilities() == old {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    };
    if disconnect_original {
        stop_modern.send_replace(true);
        wait_for_old_link().await;
    }
    assert!(actor
        .in_scope(|| reference.deliver(Box::new(Work(2))))
        .await
        .is_err());
    assert!(actor
        .in_scope(|| reference.deliver_ordered(Box::new(Work(3)), Default::default()))
        .await
        .is_err());
    let close = || actor.in_scope(|| reference.close_ordered_session(timeout));
    if disconnect_original {
        let error = close().await.unwrap_err();
        assert!(error.to_string().contains("not remotable"), "{error}");
        assert!(actor
            .in_scope(|| reference.deliver(Box::new(Work(8))))
            .await
            .is_err());
        let (restored, restored_task) =
            attach(&source, &receiver, old | caps::ORDERED_SENDER).await;
        close()
            .await
            .expect("reconnected sender-capable route can retire the original grant");
        restored.send_replace(true);
        wait_for_old_link().await;
        restored_task.await.unwrap();
    } else {
        close()
            .await
            .expect("original sender-capable connection still owns this grant");
    }
    assert!(received.try_recv().is_err());
    actor
        .in_scope(|| reference.deliver(Box::new(Work(4))))
        .await
        .unwrap();
    seen(&mut received, 4).await;
    assert!(actor
        .in_scope(|| reference.deliver_ordered(Box::new(Work(5)), Default::default()))
        .await
        .is_err());
    actor
        .in_scope(|| reference.deliver(Box::new(Work(6))))
        .await
        .unwrap();
    seen(&mut received, 6).await;
    stop_old.send_replace(true);
    stop_modern.send_replace(true);
    source.shutdown().await.unwrap();
    receiver.shutdown().await.unwrap();
    old_task.await.unwrap();
    modern_task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actor_close_uses_live_grant_when_selected_link_only_supports_node_recovery() {
    switch_first_hop(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnected_actor_grant_survives_capability_loss_until_compatible_reconnect() {
    switch_first_hop(true).await;
}
