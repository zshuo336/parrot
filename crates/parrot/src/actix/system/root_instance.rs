//! Initial dynamic root ownership survives admission, initialization and retirement.
use super::*;
use tokio::{sync::OwnedSemaphorePermit, time::Instant};

pub(in crate::actix) struct Creation {
    pub permit: Arc<OwnedSemaphorePermit>,
    pub deadline: Instant,
}

impl ActixActorSystem {
    /// Maximum admitted dynamic root and child creations, shared by clones and
    /// weak-system upgrades. Admission starts on first poll. Successful init
    /// releases the slot; failed or cancelled inputs retain it through disposal.
    /// Child requests reject with recoverable inputs when full instead of
    /// waiting on a slot that their parent's initialization may hold.
    /// Static, Local and factory creation use their existing budgets.
    pub const MAX_PENDING_INSTANCE_CREATIONS: usize = 1024;

    pub(in crate::actix) fn instance_creation_deadline(&self) -> Instant {
        let timeout = if self.operation_timeouts.actor_creation.is_zero() {
            self.settings.shutdown_timeout
        } else {
            self.operation_timeouts.actor_creation
        };
        Instant::now() + timeout
    }

    pub(super) fn spawn_root_instance<'a, A>(
        &'a self,
        actor: A,
        config: A::Config,
        strategy: Option<parrot_api::supervisor::SupervisorStrategyType>,
        path: Option<String>,
        cohort: Option<&'a crate::root_cohort::RootCohort>,
    ) -> impl std::future::Future<Output = Result<BoxedActorRef, SystemError>> + Send + 'a
    where
        A: ParrotActor<Context = ActixContext<ActixActor<A>>>
            + ActixEngineExt<Context = ActixContext<ActixActor<A>>>
            + Unpin
            + 'static,
    {
        let actor = OwnedPayload::new(actor, &self.runtime, &self.cleanup_slots);
        let config = OwnedPayload::new(config, &self.runtime, &self.cleanup_slots);
        let strategy = OwnedPayload::new(strategy, &self.runtime, &self.cleanup_slots);
        parrot_api::trace::capture(async move {
            let deadline = self.instance_creation_deadline();
            // Cleanup receipts can wait on user destructors. The public request
            // spends one deadline while their independent owners finish retirement.
            let creation = async move {
                let permit = tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline) => Err(SystemError::ActorError(
                        ActorError::TimeoutDetail("instance admission exceeded creation deadline".into()),
                    )),
                    permit = self.instance_admission.clone().acquire_owned() => {
                        permit.map(Arc::new).map_err(|_| SystemError::ShuttingDown)
                    },
                };
                let permit = match permit {
                    Ok(permit) => permit,
                    Err(error) => {
                        let (actor, config, strategy) =
                            tokio::join!(actor.retire(), config.retire(), strategy.retire());
                        actor
                            .and(config)
                            .and(strategy)
                            .map_err(SystemError::ActorError)?;
                        return Err(error);
                    }
                };
                let mut actor = Some(actor.with_reservation(&permit));
                let mut config = Some(config.with_reservation(&permit));
                let strategy = strategy.with_reservation(&permit);
                let cohort = cohort.or_else(|| self.root_cohorts.guardian());
                let strategy = if cohort.is_some() {
                    strategy.retire().await.map_err(SystemError::ActorError)?;
                    None
                } else {
                    strategy.take()
                };
                let mut lifetime_owner = None;
                let mut rejected = None;
                let mut ticket = None;
                let admit = |lifetime: Arc<crate::children::ChildLifetime>| {
                    lifetime_owner = Some(lifetime.clone());
                    // Recheck after arbitrary ignored-policy disposal and before
                    // any registry publication or user initialization can occur.
                    if Instant::now() >= deadline {
                        return Err(ActorError::TimeoutDetail(
                            "instance admission exceeded creation deadline".into(),
                        ));
                    }
                    match self.spawn_managed_with_creation(
                        actor.take().expect("one root actor").take(),
                        config.take().expect("one root config").take(),
                        None,
                        Some(lifetime.clone()),
                        path,
                        Some(Creation {
                            permit: permit.clone(),
                            deadline,
                        }),
                    ) {
                        Ok(pending) => {
                            let reference = pending.actor_ref.clone_boxed();
                            let control = pending.control.clone();
                            ticket = Some(pending);
                            Ok((
                                reference,
                                Arc::new(move || {
                                    control.request_stop(Instant::now() + control.timeout);
                                    let lifetime = lifetime.clone();
                                    Box::pin(async move { lifetime.wait().await })
                                        as BoxedFuture<'static, ActorResult<()>>
                                }) as crate::children::StopChild,
                            ))
                        }
                        Err(failed) => {
                            rejected = Some(failed);
                            Err(ActorError::Stopped)
                        }
                    }
                };
                let admitted = if let Some(cohort) = cohort {
                    cohort
                        .validate(&self.root_cohorts)
                        .and_then(|()| cohort.scope.spawn(admit))
                } else {
                    admit(crate::children::ChildLifetime::with_policy(
                        Weak::new(),
                        strategy,
                        self.policy_slots.clone(),
                    ))
                    .map(|_| ())
                };
                if let Err(error) = admitted {
                    // This independent owner keeps the slot until all three inputs
                    // retire, even when the public rejection future is cancelled.
                    return self
                        .root_cohorts
                        .retirement_runtime()
                        .spawn(parrot_api::trace::capture(async move {
                            let _permit = permit;
                            let (actor, config, rejected, policy) = tokio::join!(
                                async {
                                    match actor {
                                        Some(actor) => actor.retire().await,
                                        None => Ok(()),
                                    }
                                },
                                async {
                                    match config {
                                        Some(config) => config.retire().await,
                                        None => Ok(()),
                                    }
                                },
                                async {
                                    match rejected {
                                        Some(failed) => {
                                            failed
                                                .retired
                                                .await
                                                .map_err(SystemError::ActorError)?;
                                            Err(failed.error)
                                        }
                                        None => Ok(()),
                                    }
                                },
                                async {
                                    match lifetime_owner {
                                        Some(lifetime) => lifetime.dispose_policy().await,
                                        None => Ok(()),
                                    }
                                },
                            );
                            actor
                                .and(config)
                                .and(policy)
                                .map_err(SystemError::ActorError)?;
                            rejected?;
                            Err(SystemError::ActorError(error))
                        }))
                        .await
                        .map_err(|error| {
                            SystemError::ActorError(ActorError::InternalError(error.to_string()))
                        })?;
                }
                ticket
                    .expect("admitted root instance")
                    .wait()
                    .await
                    .map_err(SystemError::ActorError)
            };
            tokio::time::timeout_at(deadline, creation)
                .await
                .unwrap_or_else(|_| {
                    Err(SystemError::ActorError(ActorError::TimeoutDetail(
                        "instance creation exceeded creation deadline".into(),
                    )))
                })
        })
    }
}
