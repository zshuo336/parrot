use super::*;
use parrot_remote::roles::directory::ownership::OwnershipCommand;

struct Release(Arc<Events>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.work.add_permits(4096);
        self.0.stop.add_permits(4096);
    }
}
async fn install(
    node: &RunningNode,
    name: &str,
    members: &[NodeAddr],
    routes: &[NodeAddr],
    events: &Arc<Events>,
) -> ShardRouter {
    node.install_shards(
        ShardConfig::new(name, members.to_vec(), "router-test", routes.to_vec()),
        factory(events),
    )
    .await
    .unwrap()
}
async fn independent(actor: &dyn ActorRef, value: u64) {
    tokio::time::timeout(Duration::from_secs(2), echo(actor, value))
        .await
        .expect("an unrelated entity was blocked by another entity's work");
}
async fn admitted(actor: &dyn ActorRef, message: BoxedMessage) {
    tokio::time::timeout(Duration::from_secs(2), actor.deliver(message))
        .await
        .expect("family admission was blocked by an executing entity")
        .unwrap();
}
fn disposed(events: &Events) {
    assert_eq!(
        events.created.lock().unwrap().len(),
        events.dropped.load(Ordering::SeqCst)
    );
}

struct Forward {
    facade: std::sync::Weak<parrot::system::ParrotActorSystem>,
    target: String,
    events: Arc<Events>,
}
struct DisposalGate {
    entered: Notify,
    released: (Mutex<bool>, Condvar),
}
struct SlowInput(Arc<DisposalGate>);
impl Drop for SlowInput {
    fn drop(&mut self) {
        self.0.entered.notify_one();
        let (guard, timeout) = self
            .0
            .released
            .1
            .wait_timeout_while(self.0.released.0.lock().unwrap(), WAIT, |ready| !*ready)
            .unwrap();
        drop(guard);
        assert!(!timeout.timed_out(), "test owner abandoned input disposal");
    }
}
struct ReleaseInput(Arc<DisposalGate>);
impl Drop for ReleaseInput {
    fn drop(&mut self) {
        *self.0.released.0.lock().unwrap() = true;
        self.0.released.1.notify_all();
    }
}
impl Drop for Forward {
    fn drop(&mut self) {
        self.events.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Forward {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let facade = self.facade.upgrade().ok_or(ActorError::Stopped)?;
            let actor =
                ActorSystem::get_actor(facade.as_ref(), &ActorPath::placeholder(&self.target))
                    .await
                    .ok_or(ActorError::Stopped)?;
            actor.send(message).await
        })
    }
}

pub(super) async fn run(
    node: &RunningNode,
    members: &[NodeAddr],
    routes: &[NodeAddr],
    authority: &OwnershipClusterClient,
) {
    let events = new_events();
    let _release = Release(events.clone());
    let router = install(node, "parallel-work", members, routes, &events).await;
    let blocked = reference(node, &router, "blocked").await;
    let healthy = reference(node, &router, "healthy").await;
    let blocked_trace = TraceContext::root();
    trace::scope(Some(blocked_trace.clone()), async {
        admitted(blocked.as_ref(), Box::new(NSlowEcho(701))).await;
        tokio::time::timeout(WAIT, events.entered.notified())
            .await
            .unwrap();
        // With engine mailbox capacity one, multiple queued messages must not
        // make the family host wait on the busy entity's runtime mailbox.
        admitted(blocked.as_ref(), Box::new(NEcho(702))).await;
        admitted(blocked.as_ref(), Box::new(NEcho(703))).await;
        assert!(matches!(
            blocked
                .send_with_timeout(Box::new(NEcho(704)), Some(Duration::from_millis(80)))
                .await,
            Err(ActorError::Timeout)
        ));
    })
    .await;
    let healthy_trace = TraceContext::root();
    trace::scope(
        Some(healthy_trace.clone()),
        independent(healthy.as_ref(), 705),
    )
    .await;
    let later_trace = TraceContext::root();
    trace::scope(
        Some(later_trace.clone()),
        independent(healthy.as_ref(), 706),
    )
    .await;
    assert!(!events
        .processed
        .lock()
        .unwrap()
        .iter()
        .any(|(key, _, _)| key == "blocked"));
    events.work.add_permits(1);
    router.passivate("blocked").await.unwrap();
    let processed = events.processed.lock().unwrap().clone();
    assert_eq!(
        processed
            .iter()
            .filter(|(key, _, _)| key == "blocked")
            .map(|(_, value, _)| *value)
            .collect::<Vec<_>>(),
        [701, 702, 703, 704]
    );
    for (key, value, context) in &processed {
        assert_eq!(
            context.as_ref().unwrap().trace_id(),
            if key == "blocked" {
                blocked_trace.trace_id()
            } else if *value == 706 {
                later_trace.trace_id()
            } else {
                healthy_trace.trace_id()
            }
        );
    }
    router.close().await.unwrap();
    disposed(&events);

    let events = new_events();
    let _release = Release(events.clone());
    let router = install(node, "parallel-init", members, routes, &events).await;
    let late = reference(node, &router, "late-init").await;
    late.deliver(Box::new(NEcho(711))).await.unwrap();
    tokio::time::timeout(WAIT, events.entered.notified())
        .await
        .unwrap();
    let healthy = reference(node, &router, "healthy").await;
    independent(healthy.as_ref(), 712).await;
    events.work.add_permits(1);
    router.passivate("late-init").await.unwrap();
    assert!(events
        .processed
        .lock()
        .unwrap()
        .iter()
        .any(|(key, value, _)| key == "late-init" && *value == 711));
    router.close().await.unwrap();
    disposed(&events);

    let events = new_events();
    let standard = factory(&events);
    let captured = events.clone();
    let facade = Arc::downgrade(&node.facade);
    let router = node
        .install_shards(
            ShardConfig::new(
                "parallel-calls",
                members.to_vec(),
                "router-test",
                routes.to_vec(),
            ),
            Arc::new(move |entity| {
                if entity.key == "caller" {
                    captured
                        .created
                        .lock()
                        .unwrap()
                        .push((entity.key, entity.generation, None));
                    Ok(Box::new(Forward {
                        facade: facade.clone(),
                        target: "/shards/parallel-calls/callee".into(),
                        events: captured.clone(),
                    }))
                } else {
                    standard(entity)
                }
            }),
        )
        .await
        .unwrap();
    let caller = reference(node, &router, "caller").await;
    independent(caller.as_ref(), 721).await;
    assert!(events
        .processed
        .lock()
        .unwrap()
        .iter()
        .any(|(key, value, _)| key == "callee" && *value == 721));
    router.close().await.unwrap();
    disposed(&events);

    let events = new_events();
    let _release = Release(events.clone());
    let router = install(node, "parallel-retire", members, routes, &events).await;
    let stopping = reference(node, &router, "blocked-stop").await;
    generation(stopping.as_ref()).await;
    let healthy = reference(node, &router, "healthy").await;
    let mut retirement = Box::pin(router.passivate("blocked-stop"));
    tokio::select! {
        _ = events.stopping.notified() => {},
        result = &mut retirement => panic!("retirement finished before hook release: {result:?}"),
    }
    independent(healthy.as_ref(), 731).await;
    drop(retirement);
    assert!(
        tokio::time::timeout(Duration::from_millis(80), router.update_members(vec![]))
            .await
            .is_err()
    );
    events.stop.add_permits(1);
    router.passivate("blocked-stop").await.unwrap();
    independent(healthy.as_ref(), 732).await;
    router.close().await.unwrap();
    disposed(&events);

    for rebalance in [false, true] {
        let events = new_events();
        let _release = Release(events.clone());
        let name = if rebalance {
            "parallel-rebalance"
        } else {
            "parallel-drain"
        };
        let router = install(node, name, members, routes, &events).await;
        let blocked = reference(node, &router, "blocked").await;
        let healthy = reference(node, &router, "healthy").await;
        generation(healthy.as_ref()).await;
        blocked.deliver(Box::new(NSlowEcho(741))).await.unwrap();
        tokio::time::timeout(WAIT, events.entered.notified())
            .await
            .unwrap();
        admitted(blocked.as_ref(), Box::new(NEcho(742))).await;
        let mut closing = Box::pin(async {
            if rebalance {
                router.update_members(vec![]).await
            } else {
                router.close().await
            }
        });
        tokio::select! {
            result = &mut closing => panic!("close finished while accepted work was blocked: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(2), async {
                while owner(authority, &router.path("healthy").unwrap()).await.unwrap().holder.is_some() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }) => result.expect("blocked work prevented a healthy sibling's retirement"),
        }
        assert!(owner(authority, &router.path("blocked").unwrap())
            .await
            .unwrap()
            .holder
            .is_some());
        events.work.add_permits(1);
        closing.await.unwrap();
        assert!(owner(authority, &router.path("blocked").unwrap())
            .await
            .unwrap()
            .holder
            .is_none());
        assert_eq!(
            events
                .processed
                .lock()
                .unwrap()
                .iter()
                .filter(|(key, _, _)| key == "blocked")
                .map(|(_, value, _)| *value)
                .collect::<Vec<_>>(),
            [741, 742]
        );
        router.close().await.unwrap();
        disposed(&events);
    }

    let events = new_events();
    let mut config = ShardConfig::new(
        "parallel-disposal",
        members.to_vec(),
        "router-test",
        routes.to_vec(),
    );
    config.max_entities = 1;
    let router = node.install_shards(config, factory(&events)).await.unwrap();
    let resource = router.path("held").unwrap();
    let claimed = authority
        .submit_and_wait(
            "concurrency-disposal-claim",
            &OwnershipCommand::Claim {
                resource: resource.clone(),
                expected: None,
            },
            WAIT,
        )
        .await
        .unwrap();
    let OwnershipStatus::Applied {
        outcome: OwnershipOutcome::Changed(held),
        ..
    } = claimed
    else {
        panic!("test owner failed to reserve the resource: {claimed:?}");
    };
    let gate = Arc::new(DisposalGate {
        entered: Notify::new(),
        released: (Mutex::new(false), Condvar::new()),
    });
    let release = ReleaseInput(gate.clone());
    let held_actor = reference(node, &router, "held").await;
    let mut rejected = Box::pin(held_actor.send(Box::new(SlowInput(gate.clone()))));
    tokio::select! {
        _ = gate.entered.notified() => {},
        result = &mut rejected => panic!("input disposal completed before release: {result:?}"),
    }
    let next = reference(node, &router, "next").await;
    let mut retiring = Box::pin(router.passivate("held"));
    tokio::select! {
        biased;
        result = &mut retiring => panic!("retirement finished during input disposal: {result:?}"),
        _ = async {
            // This response also orders the probe after the accepted control.
            assert!(matches!(
                next.send_with_timeout(Box::new(NEcho(751)), Some(Duration::from_millis(100))).await,
                Err(ActorError::MessageHandlingError(text)) if text.contains("entity capacity")
            ), "unfinished input disposal must retain the cell's activity reservation");
            assert!(matches!(
                held_actor.send_with_timeout(Box::new(NEcho(753)), Some(Duration::from_millis(100))).await,
                Err(ActorError::MessageHandlingError(text)) if text.contains("generation is retiring")
            ), "an idle but retiring worker must reject later work before admission");
        } => {},
    }
    drop(release);
    assert!(matches!(rejected.await,
        Err(ActorError::MessageHandlingError(text)) if text.contains("generation still held")));
    retiring.await.unwrap();
    independent(next.as_ref(), 752).await;
    assert_eq!(events.created.lock().unwrap().len(), 1);
    let released = authority
        .submit_and_wait(
            "concurrency-disposal-release",
            &OwnershipCommand::Release {
                resource,
                expected: held.generation,
            },
            WAIT,
        )
        .await
        .unwrap();
    assert!(matches!(released, OwnershipStatus::Applied {
        outcome: OwnershipOutcome::Changed(record), ..
    } if record.holder.is_none()));
    router.close().await.unwrap();
    disposed(&events);
}

async fn isolated(
    shutdown_timeout_ms: u64,
) -> (
    tempfile::TempDir,
    NetworkDirectory,
    RunningNode,
    Vec<NodeAddr>,
) {
    let data = tempfile::tempdir().unwrap();
    let directory = NetworkDirectory::start_with_timing(
        data.path(),
        "full-capacity",
        RemoteConfig::mem("capacity-authority"),
        vec![],
        Arc::new(Empty),
        DirectoryTiming {
            election_ms: 100,
            heartbeat_ms: 20,
            rpc_timeout_ms: 500,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(WAIT, async {
        while directory.handle().observation().role != Role::Leader {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut settings = NodeSettings::from_environment(|key| match key {
        "PARROT_NODE_ID" => Some("capacity-host".into()),
        "PARROT_SCHEME" => Some("mem".into()),
        "PARROT_ACTORS" => Some("".into()),
        _ => None,
    })
    .unwrap();
    settings.resolved.thread.default_mailbox_capacity = 1;
    settings.resolved.thread.shutdown_timeout_ms = shutdown_timeout_ms;
    let node = RunningNode::start(&settings).await.unwrap();
    let routes = vec![NodeAddr::mem("capacity-authority")];
    (data, directory, node, routes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit isolated validation of the full 4096-entity capacity"]
async fn full_entity_capacity_and_control_reservation() {
    let (_data, mut directory, node, routes) = isolated(30000).await;
    let mut config = ShardConfig::new(
        "full",
        vec![NodeAddr::mem("capacity-host")],
        "full-capacity",
        routes.clone(),
    );
    config.max_entities = 4096;
    let events = new_events();
    let router = node.install_shards(config, factory(&events)).await.unwrap();
    let started = std::time::Instant::now();
    for index in 0..4096 {
        let actor = reference(&node, &router, &format!("key-{index}")).await;
        assert!(generation(actor.as_ref()).await > 0);
        if (index + 1) % 512 == 0 {
            eprintln!(
                "activated {} entities in {:?}",
                index + 1,
                started.elapsed()
            );
        }
    }
    assert_eq!(events.created.lock().unwrap().len(), 4096);
    let overflow = reference(&node, &router, "one-too-many").await;
    assert!(matches!(overflow.send(Box::new(NEcho(1))).await,
        Err(ActorError::MessageHandlingError(text)) if text.contains("entity capacity")));
    // Control must still be admitted at the exact business capacity. The real
    // runtime parent limit cannot strand otherwise healthy ownership records.
    // Durable authority work grows with retained history. This is a correctness
    // cleanup bound, not a latency SLO; report the actual elapsed time below.
    let closing = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(600), router.close())
        .await
        .unwrap()
        .unwrap();
    eprintln!("closed 4096 entities in {:?}", closing.elapsed());
    disposed(&events);
    let authority =
        OwnershipClusterClient::new(node.remote.clone(), "full-capacity", routes).unwrap();
    for key in ["key-0", "key-4095"] {
        assert!(owner(&authority, &router.path(key).unwrap())
            .await
            .unwrap()
            .holder
            .is_none());
    }
    node.shutdown().await.unwrap();
    directory.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit isolated validation of repeated generation retirement"]
async fn repeated_generations_do_not_retain_child_notifications() {
    // Keep notification timeouts longer than this run: a pass must come from
    // retiring the real worker, not from waiting for old notifications to expire.
    let (_data, mut directory, node, routes) = isolated(600000).await;
    let mut config = ShardConfig::new(
        "cycles",
        vec![NodeAddr::mem("capacity-host")],
        "full-capacity",
        routes.clone(),
    );
    config.max_entities = 1;
    let events = new_events();
    let router = node.install_shards(config, factory(&events)).await.unwrap();
    let actor = reference(&node, &router, "same").await;
    let started = std::time::Instant::now();
    let mut previous = 0;
    for index in 0..1030 {
        let current = generation(actor.as_ref()).await;
        assert!(
            current > previous,
            "reactivation reused the previous generation"
        );
        previous = current;
        router.passivate("same").await.unwrap();
        assert_eq!(events.created.lock().unwrap().len(), index + 1);
        assert_eq!(events.dropped.load(Ordering::SeqCst), index + 1);
        if (index + 1) % 128 == 0 {
            eprintln!(
                "retired {} generations in {:?}",
                index + 1,
                started.elapsed()
            );
        }
    }
    let authority =
        OwnershipClusterClient::new(node.remote.clone(), "full-capacity", routes).unwrap();
    assert!(owner(&authority, &router.path("same").unwrap())
        .await
        .unwrap()
        .holder
        .is_none());
    tokio::time::timeout(Duration::from_secs(30), router.close())
        .await
        .unwrap()
        .unwrap();
    disposed(&events);
    node.shutdown().await.unwrap();
    directory.close().await.unwrap();
}
