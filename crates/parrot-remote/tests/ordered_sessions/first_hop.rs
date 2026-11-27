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

async fn upgrade_shared_sender(actor_paths: bool) {
    let (target, mut received) = state();
    let receiver = start(
        RemoteConfig::tcp("wire-source", None).with_insecure_development(),
        Arc::new(Target(target.clone())),
    )
    .await;
    let source = start(
        RemoteConfig::tcp("upgrading-source", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Empty),
    )
    .await;
    let base = caps::BIN | caps::ORDERED_TELL | caps::TELL_COMPLETION | caps::ORDERED_ADMISSION;
    let old = if actor_paths {
        base | caps::ORDERED_ACTOR
    } else {
        base
    };
    let (stop_old, old_task) = attach(&source, &receiver, old).await;
    let target_ref = source.remote_ref("parrot://wire-source/target").unwrap();
    let writer = ActorSenderOwner::new("/user/writer");
    let sibling = ActorSenderOwner::new("/user/sibling");
    let timeout = Duration::from_secs(3);
    writer
        .sender()
        .in_scope(|| target_ref.deliver(Box::new(Work(701))))
        .await
        .unwrap();
    tokio::time::timeout(timeout, target.entered.notified())
        .await
        .unwrap();
    sibling
        .sender()
        .in_scope(|| target_ref.deliver(Box::new(Work(2))))
        .await
        .unwrap();
    let (stop_new, new_task) = attach(
        &source,
        &receiver,
        base | caps::ORDERED_ACTOR | caps::ORDERED_SENDER,
    )
    .await;
    let upgrade = || {
        writer
            .sender()
            .in_scope(|| target_ref.deliver_ordered(Box::new(Work(3)), Default::default()))
    };
    let release = Release(target.clone());
    assert!(
        upgrade().await.is_err(),
        "private session overtook a live shared delivery"
    );
    writer
        .sender()
        .in_scope(|| target_ref.close_ordered_session(timeout))
        .await
        .unwrap();
    assert!(
        upgrade().await.is_err(),
        "one actor closed a ticket shared with another actor"
    );
    writer
        .sender()
        .in_scope(|| target_ref.deliver(Box::new(Work(8))))
        .await
        .unwrap();
    target.resume.notify_one();
    seen(&mut received, 701).await;
    seen(&mut received, 2).await;
    seen(&mut received, 8).await;
    writer
        .sender()
        .in_scope(|| target_ref.deliver(Box::new(Work(700))))
        .await
        .unwrap();
    tokio::time::timeout(timeout, target.entered.notified())
        .await
        .unwrap();
    {
        let close = target_ref.close_ordered_session(timeout);
        tokio::pin!(close);
        tokio::select! {
            early = &mut close => panic!("shared worker still owns a delivery: {early:?}"),
            result = tokio::time::timeout(timeout, target.dropping.notified()) => result.unwrap(),
        }
        // Cancelling the caller cannot remove the fence while Drop is blocked.
    }
    let error = upgrade().await.unwrap_err();
    assert!(
        error.to_string().contains("ordered sender scope"),
        "{error}"
    );
    release.release();
    target_ref.close_ordered_session(timeout).await.unwrap();
    assert!(target.dropped.load(Ordering::SeqCst));
    upgrade()
        .await
        .expect("a retired shared ticket must allow promotion");
    seen(&mut received, 3).await;
    // Node-originated and private deliveries can coexist on a fully capable connection.
    strict(&target_ref, 4).await;
    seen(&mut received, 4).await;
    writer.sender().scope(strict(&target_ref, 5)).await;
    seen(&mut received, 5).await;
    stop_new.send_replace(true);
    stop_old.send_replace(true);
    source.shutdown().await.unwrap();
    receiver.shutdown().await.unwrap();
    new_task.await.unwrap();
    old_task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_wide_shared_ticket_fences_sender_upgrade() {
    upgrade_shared_sender(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn destination_actor_shared_ticket_fences_sender_upgrade() {
    upgrade_shared_sender(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_direct_link_keeps_actor_fifo_and_sibling_progress_through_tls_hub() {
    use parrot_remote::topology::{TopologyConfig, TopologyMode};
    let pki = Pki::new();
    let config = |name| {
        RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap())).with_mtls(pki.node(name), [])
    };
    let hub = start(
        config("fifo-hub").with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let (target_state, mut received) = state();
    let target = start(
        config("fifo-end").with_ordered_relays(["fifo-hub".into()]),
        Arc::new(Target(target_state.clone())),
    )
    .await;
    let source = start(
        config("fifo-source").with_topology(TopologyConfig {
            mode: TopologyMode::Hybrid,
            ..Default::default()
        }),
        Arc::new(Empty),
    )
    .await;
    let hub_addr = NodeAddr::tcp("fifo-hub", hub.local_addr().unwrap());
    target.connect(&hub_addr).await.unwrap();
    source
        .connect(&NodeAddr::tcp("fifo-end", target.local_addr().unwrap()))
        .await
        .unwrap();
    let direct = source.links_snapshot().await.pop().unwrap().1;
    assert_eq!(direct.node_id(), "fifo-end");
    let reference = source.remote_ref("parrot://fifo-end/target").unwrap();
    let writer = ActorSenderOwner::new("/user/writer");
    let sibling = ActorSenderOwner::new("/user/sibling");
    let timeout = Duration::from_secs(4);
    writer.sender().scope(strict(&reference, 701)).await;
    tokio::time::timeout(timeout, target_state.entered.notified())
        .await
        .unwrap();
    source.connect(&hub_addr).await.unwrap();
    direct.close();
    tokio::time::timeout(timeout, async {
        loop {
            let peers = source.links_snapshot().await;
            if peers.len() == 1 && peers[0].0 == "fifo-hub" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    writer.sender().scope(strict(&reference, 2)).await;
    sibling.sender().scope(strict(&reference, 3)).await;
    seen(&mut received, 3).await;
    target_state.resume.notify_one();
    seen(&mut received, 701).await;
    seen(&mut received, 2).await;
    assert_eq!(source.links_snapshot().await.len(), 1);
    for owner in [&writer, &sibling] {
        owner
            .sender()
            .in_scope(|| reference.close_ordered_session(timeout))
            .await
            .unwrap();
    }
    for system in [&source, &target, &hub] {
        system.shutdown().await.unwrap();
    }
    assert!(received.try_recv().is_err());
}
