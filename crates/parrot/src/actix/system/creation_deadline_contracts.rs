//! Hold the actual creation coordinator until cleanup and its deadline are ready.
use super::*;
use parrot_api::actor::{ActorState, EmptyConfig};

struct Probe {
    initialized: Arc<AtomicUsize>,
    disposed: Arc<AtomicUsize>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.disposed.fetch_add(1, Ordering::SeqCst);
    }
}
impl parrot_api::actor::ActixEngineExt for Probe {}
impl ParrotActor for Probe {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}

#[actix::test]
async fn ready_config_cannot_bypass_deadline_on_stalled_creation_executor() {
    let mut system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap();
    // Inject the coordinator's executor, so this test stalls its actual owner.
    // Actor execution and system shutdown retain their real independent owners.
    system.runtime = tokio::runtime::Handle::current();
    let initialized = Arc::new(AtomicUsize::new(0));
    let disposed = Arc::new(AtomicUsize::new(0));
    let ticket = system
        .spawn_managed(
            Probe {
                initialized: initialized.clone(),
                disposed: disposed.clone(),
            },
            EmptyConfig,
            None,
            None,
        )
        .unwrap_or_else(|_| panic!("instance admission failed"));
    let reference = ticket.actor_ref.clone_boxed();
    let creation = ticket.wait();
    tokio::pin!(creation);
    assert!(futures::poll!(creation.as_mut()).is_pending());
    // No yield lets the coordinator run before this sleep. EmptyConfig has
    // immediate disposal, so both branches are ready at the first select poll.
    std::thread::sleep(Duration::from_millis(200));
    let result = creation.await;
    assert!(
        matches!(result, Err(ActorError::TimeoutDetail(_))),
        "ready cleanup result bypassed the elapsed creation deadline: {result:?}"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while matches!(
            reference.retirement_status(),
            parrot_api::address::RetirementStatus::Pending
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(initialized.load(Ordering::SeqCst), 0);
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    assert!(reference.send(Box::new(())).await.is_err());
    system.clone().shutdown().await.unwrap();
    assert_eq!(system.status().resources.thread_count, 0);
}
