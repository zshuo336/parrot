// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot_api::{address::RetirementStatus, errors::ActorError, runtime::LoadBalancingStrategy};
use std::sync::mpsc;
use tokio::sync::oneshot;

struct BlockOnDrop {
    entered: Option<oneshot::Sender<()>>,
    release: mpsc::Receiver<()>,
    retired: Arc<AtomicUsize>,
}

impl Drop for BlockOnDrop {
    fn drop(&mut self) {
        let _ = self.entered.take().unwrap().send(());
        let _ = self.release.recv_timeout(LIMIT);
        self.retired.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn shared_typed_host_exit_waits_for_generation_io_after_stop_waiter_cancellation() {
    for strategy in [
        LoadBalancingStrategy::Random,
        LoadBalancingStrategy::LeastLoaded,
    ] {
        check_retirement(strategy);
    }
}

fn check_retirement(strategy: LoadBalancingStrategy) {
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let host = runtime();
    let observer = runtime();
    let exits = Arc::new(AtomicUsize::new(0));
    let (system, actor, context) = host.block_on(async {
        let mut config = config();
        config.runtime_config.io_threads = Some(1);
        config.runtime_config.scheduler_config.load_balancing = strategy;
        let system = ThreadActorSystem::with_runtime_config(
            ThreadActorSystemConfig {
                shared_burst_workers_max: 0,
                ..Default::default()
            },
            config.runtime_config,
        )
        .await
        .unwrap();
        let actor = bounded(system.spawn_typed::<_, TypedRequest>(TypedProbe, "/placed"))
            .await
            .unwrap();
        let TypedReply::Context(context) = bounded(actor.ask(TypedRequest::Context)).await.unwrap()
        else {
            panic!("missing typed context");
        };
        let (entered, busy) = oneshot::channel();
        let (release, wait) = mpsc::channel();
        bounded(actor.tell(TypedRequest::Block(entered, wait)))
            .await
            .unwrap();
        let actor_thread = bounded(busy).await.unwrap();
        let tls = exits.clone();
        let io_thread = bounded(context.io_runtime().unwrap().run_io_typed(async move {
            EXIT.with(|slot| *slot.borrow_mut() = Some(ThreadExit(tls)));
            tcp().await
        }))
        .await
        .unwrap();
        assert_ne!(actor_thread, io_thread);
        release.send(()).unwrap();
        bounded(actor.ask(TypedRequest::Tcp)).await.unwrap();
        (system, actor, context)
    });

    let drops = Arc::new(AtomicUsize::new(0));
    let (entered, disposing) = oneshot::channel();
    let (release, wait) = mpsc::channel();
    let guard = BlockOnDrop {
        entered: Some(entered),
        release: wait,
        retired: drops.clone(),
    };
    let (started, running) = oneshot::channel();
    let work = observer.spawn(context.io_runtime().unwrap().run_io_typed(async move {
        let _guard = guard;
        started.send(()).unwrap();
        std::future::pending::<ActorResult<()>>().await
    }));
    observer.block_on(bounded(running)).unwrap();
    drop(host);
    observer.block_on(async {
        bounded(disposing).await.unwrap();
        {
            let mut first = Box::pin(actor.stop());
            assert!(futures::poll!(&mut first).is_pending());
        }
        let mut second = Box::pin(actor.stop());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut second)
                .await
                .is_err(),
            "second stop completed before the generation's I/O input was disposed"
        );
        assert!(matches!(
            actor.retirement_status(),
            RetirementStatus::Pending
        ));
        assert_eq!(system.actor_count(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        let error = bounded(second).await.unwrap_err();
        assert!(
            error.to_string().contains("JoinError::Cancelled"),
            "{error}"
        );
        assert!(matches!(
            bounded(work).await.unwrap(),
            Err(ActorError::Stopped)
        ));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(system.actor_count(), 0);
        assert!(matches!(
            actor.retirement_status(),
            RetirementStatus::Retired(Ok(()))
        ));
        assert!(context.io_runtime().is_none());
        let result = bounded(system.shutdown_internal()).await;
        assert!(
            matches!(&result, Err(parrot::thread::error::SystemError::ShutdownError(message))
            if message.contains("shared task unexpectedly cancelled")),
            "{result:?}"
        );
        assert_eq!(exits.load(Ordering::SeqCst), 1);
    });
}
