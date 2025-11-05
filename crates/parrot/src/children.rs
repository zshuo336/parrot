//! Bounded generation-owned children, independent of public metadata and watches.
pub(crate) mod recovery;
use crate::error_receipt::ActorReceipt;
use futures::FutureExt;
use parrot_api::{
    context::ReadOnlyChildrenVec,
    errors::ActorError,
    supervisor::{DecisionFn, DefaultStrategy, SupervisionDecision, SupervisorStrategyType},
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};
use uuid::Uuid;

const MAX_CHILDREN: usize = 1024;
pub(crate) type Escalate = Arc<dyn Fn(Arc<ActorError>) + Send + Sync>;
pub(crate) type StopChild = Arc<dyn Fn() -> BoxedFuture<'static, ActorResult<()>> + Send + Sync>;
pub(crate) struct ChildTerminated(pub BoxedActorRef);
pub(crate) trait ChildRecipient: Send + Sync {
    fn deliver(&self, event: ChildTerminated) -> BoxedFuture<'_, ActorResult<()>>;
}
type RecoverChild = Arc<dyn Fn(Arc<recovery::Recovery>) + Send + Sync>;
struct Child {
    lifetime: Arc<ChildLifetime>,
    actor: BoxedActorRef,
    stop: StopChild,
    // Keep capacity reserved through termination notification, too.
    permit: OwnedSemaphorePermit,
}
pub(crate) struct ChildScope {
    closed: AtomicBool,
    changed: Notify,
    children: Mutex<HashMap<Uuid, Child>>,
    // Failures during this scope's retirement outlive member removal. Earlier
    // independent child failures do not poison a later parent stop operation.
    retirement_failure: Mutex<Option<ActorReceipt>>,
    slots: Arc<Semaphore>,
    recipient: OnceLock<Arc<dyn ChildRecipient>>,
    escalator: OnceLock<Escalate>,
    timeout: Duration,
    policy: crate::supervision_policy::PolicyStore,
    group_active: AtomicBool,
    // Separate from the children/topology lock: parent stop must cancel an
    // uncommitted epoch synchronously, before asynchronous child joins run.
    active_recovery: Mutex<Option<Weak<recovery::Recovery>>>,
    #[cfg(test)]
    recovery_registration_probe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    group_restarts: Mutex<Vec<tokio::time::Instant>>,
}
impl std::fmt::Debug for ChildScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildScope")
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
impl ChildScope {
    pub(crate) fn new(timeout: Duration, decision_slots: Arc<Semaphore>) -> Arc<Self> {
        Arc::new(Self {
            closed: AtomicBool::new(false),
            changed: Notify::new(),
            children: Mutex::new(HashMap::new()),
            retirement_failure: Mutex::new(None),
            slots: Arc::new(Semaphore::new(MAX_CHILDREN)),
            recipient: OnceLock::new(),
            escalator: OnceLock::new(),
            timeout,
            policy: crate::supervision_policy::PolicyStore::new(
                Some(SupervisorStrategyType::Default(
                    DefaultStrategy::StopOnFailure,
                )),
                decision_slots,
            ),
            group_active: AtomicBool::new(false),
            active_recovery: Mutex::new(None),
            #[cfg(test)]
            recovery_registration_probe: Mutex::new(None),
            group_restarts: Mutex::new(Vec::new()),
        })
    }
    pub(crate) fn set_strategy(&self, strategy: SupervisorStrategyType) {
        self.policy.set(Some(strategy));
    }
    pub(crate) fn bind_escalator(&self, escalate: Escalate) {
        assert!(
            self.escalator.set(escalate).is_ok(),
            "parent escalation endpoint installed twice"
        );
    }
    pub(crate) fn bind(&self, recipient: Arc<dyn ChildRecipient>) {
        assert!(
            self.recipient.set(recipient).is_ok(),
            "child recipient installed twice"
        );
    }
    pub(crate) fn view(&self) -> ReadOnlyChildrenVec {
        let snapshot = self
            .children
            .lock()
            .unwrap()
            .values()
            .map(|child| child.actor.clone_boxed())
            .collect();
        ReadOnlyChildrenVec::new(Arc::new(RwLock::new(snapshot)))
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    pub(crate) fn close(&self) {
        {
            let active = self.active_recovery.lock().unwrap();
            self.closed.store(true, Ordering::Release);
            if let Some(group) = active.as_ref().and_then(Weak::upgrade) {
                // abort serializes with the last publication acknowledgement.
                // A committed epoch belongs to the live generation and ignores
                // this abort; an uncommitted one cannot activate after close.
                group.abort("parent stop cancelled cohort recovery");
            }
        }
        self.changed.notify_waiters();
    }
    /// The synchronous engine publication and insertion form one admission.
    /// close() never takes the children lock, so registry locks cannot invert it.
    pub(crate) fn spawn(
        self: &Arc<Self>,
        spawn: impl FnOnce(Arc<ChildLifetime>) -> ActorResult<(BoxedActorRef, StopChild)>,
    ) -> ActorResult<BoxedActorRef> {
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| ActorError::InternalError("child capacity exhausted (1024 live children or pending termination notifications)".into()))?;
        let mut children = self.children.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            return Err(ActorError::Stopped);
        }
        if self.group_active.load(Ordering::Acquire) {
            return Err(ActorError::InternalError(
                "child group is handling a failure; retry after recovery".into(),
            ));
        }
        let lifetime = ChildLifetime::new(Arc::downgrade(self));
        let created =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| spawn(lifetime.clone())));
        let (actor, stop) = match created {
            Ok(result) => result?,
            Err(panic) => {
                drop(children);
                std::panic::resume_unwind(panic);
            }
        };
        children.insert(
            lifetime.id,
            Child {
                lifetime,
                actor: actor.clone_boxed(),
                stop,
                permit,
            },
        );
        Ok(actor)
    }
    pub(crate) async fn join(&self) -> ActorResult<()> {
        self.close();
        // Admission holds this lock until a submitted child has an owned stop.
        let stops: Vec<_> = self
            .children
            .lock()
            .unwrap()
            .values()
            .map(|child| child.stop.clone())
            .collect();
        let results = futures::future::join_all(stops.into_iter().map(|stop| stop())).await;
        let disposal = self.policy.dispose().await;
        for result in results {
            result?;
        }
        disposal?;
        self.retirement_failure
            .lock()
            .unwrap()
            .clone()
            .map_or(Ok(()), |error| Err(error.into_error()))
    }
    async fn notify(&self, child: Child) {
        let _permit = child.permit;
        let closed = self.changed.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if let Some(recipient) = self.recipient.get() {
            tokio::select! {
                biased;
                _ = closed => {},
                result = tokio::time::timeout(self.timeout, recipient.deliver(ChildTerminated(child.actor))) => {
                    if !matches!(result, Ok(Ok(()))) { tracing::warn!("child termination notification could not be delivered before deadline"); }
                }
            }
        }
    }
}
#[derive(Clone)]
struct PolicyChoice {
    decision: SupervisionDecision,
    all: bool,
    budget: Option<(u32, Duration)>,
    failure_context: Option<parrot_api::trace::TraceContext>,
}
#[derive(Clone)]
struct LiveDecision {
    id: Uuid,
    result:
        futures::future::Shared<BoxedFuture<'static, Result<Option<PolicyChoice>, ActorReceipt>>>,
    deadline: tokio::time::Instant,
}
/// Completed explicitly by the lifecycle owner after actual disposal. ActorRef
/// retention, user stop deadlines, and obsolete generation Drop are not receipts.
pub(crate) struct ChildLifetime {
    pub(crate) id: Uuid,
    recovery: Mutex<Option<Arc<recovery::Recovery>>>,
    stopping: AtomicBool,
    recover: OnceLock<RecoverChild>,
    parent: Weak<ChildScope>,
    done: watch::Sender<Option<Result<(), ActorReceipt>>>,
    // A child remains a parent-owned member until its creation bridge has
    // returned and released its captures, even if the live actor stops first.
    publication: watch::Sender<Option<Result<(), ActorReceipt>>>,
    policy: crate::supervision_policy::PolicyStore,
    admission: OnceLock<Arc<crate::thread::mailbox::admission::ManagedAdmission>>,
    restarts: Mutex<Vec<tokio::time::Instant>>,
    // Message and child-escalation decisions may overlap while a parent stage is paused.
    live_decision: [Mutex<Option<LiveDecision>>; 2],
}
impl ChildLifetime {
    pub(crate) fn new(parent: Weak<ChildScope>) -> Arc<Self> {
        let slots = parent
            .upgrade()
            .map(|scope| scope.policy.slots.clone())
            .unwrap_or_else(|| Arc::new(Semaphore::new(1)));
        Self::with_policy(parent, None, slots)
    }
    pub(crate) fn with_policy(
        parent: Weak<ChildScope>,
        strategy: Option<SupervisorStrategyType>,
        slots: Arc<Semaphore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: Uuid::new_v4(),
            recovery: Mutex::new(None),
            stopping: AtomicBool::new(false),
            recover: OnceLock::new(),
            parent,
            policy: crate::supervision_policy::PolicyStore::new(strategy, slots),
            admission: OnceLock::new(),
            restarts: Mutex::new(Vec::new()),
            live_decision: [Mutex::new(None), Mutex::new(None)],
            done: watch::channel(None).0,
            publication: watch::channel(Some(Ok(()))).0,
        })
    }
    pub(crate) fn bind_recovery(&self, recover: RecoverChild) {
        assert!(
            self.recover.set(recover).is_ok(),
            "logical recovery endpoint installed twice"
        );
    }
    pub(crate) fn recovery(&self) -> Option<Arc<recovery::Recovery>> {
        self.recovery
            .lock()
            .unwrap()
            .clone()
            .filter(|group| !group.is_committed())
    }
    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
    pub(crate) fn cancel_recovery(&self) -> bool {
        self.stopping.store(true, Ordering::Release);
        let assigned = self.recovery.lock().unwrap().clone();
        if let Some(group) = assigned {
            group.abort("explicit member stop cancelled cohort recovery");
            return group.is_committed();
        }
        false
    }
    pub(crate) fn set_strategy(&self, strategy: Option<SupervisorStrategyType>) {
        self.policy.set(strategy);
    }
    async fn evaluate_decision(
        &self,
        error: Arc<ActorError>,
        slots: Arc<Semaphore>,
    ) -> ActorResult<Option<PolicyChoice>> {
        let parent = self.parent.upgrade();
        let strategy = self.policy.get().or_else(|| {
            parent
                .as_ref()
                .map(|parent| parent.policy.get().unwrap_or_default())
        });
        let Some(strategy) = strategy else {
            return Ok(None);
        };
        let all = matches!(strategy, SupervisorStrategyType::OneForAll(_));
        let budget = match &strategy {
            SupervisorStrategyType::OneForOne(strategy) => {
                Some((strategy.max_restarts, strategy.within))
            }
            SupervisorStrategyType::OneForAll(strategy) => {
                Some((strategy.max_restarts, strategy.within))
            }
            SupervisorStrategyType::Default(_) => None,
        };
        let permit = slots
            .acquire_owned()
            .await
            .map_err(|_| ActorError::Stopped)?;
        let context = parrot_api::trace::TraceContext::current();
        let decision = tokio::task::spawn_blocking(move || {
            parrot_api::trace::in_scope(context, || {
                let _permit = permit;
                // No registry/topology/strategy lock is held while invoking user code.
                match strategy {
                    SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure) => {
                        SupervisionDecision::Stop
                    }
                    SupervisorStrategyType::Default(DefaultStrategy::RestartOnFailure) => {
                        SupervisionDecision::Restart
                    }
                    SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure) => {
                        SupervisionDecision::Resume
                    }
                    SupervisorStrategyType::Default(DefaultStrategy::EscalateFailure) => {
                        SupervisionDecision::Escalate
                    }
                    SupervisorStrategyType::OneForOne(strategy) => strategy.decider.decide(&error),
                    SupervisorStrategyType::OneForAll(strategy) => strategy.decider.decide(&error),
                }
            })
        })
        .await
        .map_err(|error| {
            ActorError::InternalError(format!("supervision decider failed: {error}"))
        })?;
        Ok(Some(PolicyChoice {
            decision,
            all,
            budget,
            failure_context: parrot_api::trace::TraceContext::current(),
        }))
    }
    pub(crate) fn decision_deadline(&self) -> Option<tokio::time::Instant> {
        self.live_decision
            .iter()
            .filter_map(|slot| {
                slot.lock()
                    .unwrap()
                    .as_ref()
                    .map(|pending| pending.deadline)
            })
            .min()
    }
    /// Retain callback ownership across cancellation of a message future. Only
    /// a Resume consumes the receipt here; retirement consumes every other result.
    pub(crate) async fn resume_message(
        self: &Arc<Self>,
        error: Arc<ActorError>,
        deadline: tokio::time::Instant,
        fallback_resume: bool,
        runtime: &tokio::runtime::Handle,
    ) -> bool {
        self.resume_decision(
            &self.live_decision[0],
            error,
            deadline,
            fallback_resume,
            runtime,
        )
        .await
    }
    /// A paused parent message keeps its own receipt. A child failure must not
    /// reuse or consume that decision, even when both callbacks are pending.
    pub(crate) async fn resume_escalation(
        self: &Arc<Self>,
        error: Arc<ActorError>,
        deadline: tokio::time::Instant,
        runtime: &tokio::runtime::Handle,
    ) -> bool {
        self.start_escalation(error, deadline, false, runtime).await
    }
    /// Publish the real callback receipt before returning from the child's
    /// escalation hook. Parent retirement joins that child, so policy disposal
    /// cannot overtake a merely queued decision owner.
    pub(crate) fn start_escalation(
        self: &Arc<Self>,
        error: Arc<ActorError>,
        deadline: tokio::time::Instant,
        fallback_resume: bool,
        runtime: &tokio::runtime::Handle,
    ) -> BoxedFuture<'static, bool> {
        let receipt = self.start_decision(&self.live_decision[1], error, deadline, runtime);
        let owner = self.clone();
        Box::pin(async move { owner.await_decision(receipt, fallback_resume).await })
    }
    async fn resume_decision(
        self: &Arc<Self>,
        slot: &Mutex<Option<LiveDecision>>,
        error: Arc<ActorError>,
        deadline: tokio::time::Instant,
        fallback_resume: bool,
        runtime: &tokio::runtime::Handle,
    ) -> bool {
        let receipt = self.start_decision(slot, error, deadline, runtime);
        self.await_decision(receipt, fallback_resume).await
    }
    fn start_decision(
        self: &Arc<Self>,
        slot: &Mutex<Option<LiveDecision>>,
        error: Arc<ActorError>,
        deadline: tokio::time::Instant,
        runtime: &tokio::runtime::Handle,
    ) -> LiveDecision {
        let mut pending = slot.lock().unwrap();
        if let Some(pending) = pending.as_ref() {
            pending.clone()
        } else {
            let owner = self.clone();
            let (send, receive) = tokio::sync::oneshot::channel();
            runtime.spawn(parrot_api::trace::capture(async move {
                let result = owner
                    .evaluate_decision(error, owner.policy.slots.clone())
                    .await;
                let _ = send.send(result.map_err(ActorReceipt::from_error));
            }));
            let result = async move {
                receive.await.unwrap_or_else(|_| {
                    Err(ActorReceipt::from_error(ActorError::InternalError(
                        "supervision decision owner exited".into(),
                    )))
                })
            }
            .boxed()
            .shared();
            let receipt = LiveDecision {
                id: Uuid::new_v4(),
                result,
                deadline,
            };
            *pending = Some(receipt.clone());
            receipt
        }
    }
    async fn await_decision(&self, receipt: LiveDecision, fallback_resume: bool) -> bool {
        let result = tokio::time::timeout_at(receipt.deadline, receipt.result).await;
        let resume = match result {
            Ok(Ok(Some(choice))) => choice.decision == SupervisionDecision::Resume,
            Ok(Ok(None)) => fallback_resume,
            _ => false,
        };
        if resume
            && tokio::time::Instant::now() < receipt.deadline
            && !self.is_stopping()
            && self.recovery().is_none()
            && !self
                .parent
                .upgrade()
                .is_some_and(|parent| parent.closed.load(Ordering::Acquire))
        {
            self.clear_decision(receipt.id);
            true
        } else {
            false
        }
    }
    /// The lifecycle owner must retain this future through timeout: a blocked
    /// user decider still owns resources and must not be detached at retirement.
    pub(crate) async fn failure_policy(
        &self,
        error: Arc<ActorError>,
        slots: Arc<Semaphore>,
        deadline: tokio::time::Instant,
    ) -> ActorResult<Option<FailurePolicy>> {
        let pending = if let Some((choice, pending_deadline)) = self.join_live_decision().await? {
            if tokio::time::Instant::now() >= pending_deadline {
                return Ok(Some(FailurePolicy::stop()));
            }
            Some(choice)
        } else {
            None
        };
        let parent = self.parent.upgrade();
        if parent
            .as_ref()
            .is_some_and(|parent| parent.closed.load(Ordering::Acquire))
        {
            return Ok(Some(FailurePolicy::stop()));
        }
        if let Some(recovery) = self.recovery() {
            return Ok(Some(FailurePolicy {
                decision: SupervisionDecision::Restart,
                group: None,
                failure_context: recovery.failure_context.clone(),
                recovery: Some(recovery),
            }));
        }
        let choice = if let Some(choice) = pending {
            choice
        } else {
            self.evaluate_decision(error, slots).await?
        };
        let Some(PolicyChoice {
            mut decision,
            all,
            budget,
            failure_context,
        }) = choice
        else {
            return Ok(None);
        };
        if parent
            .as_ref()
            .is_some_and(|parent| parent.closed.load(Ordering::Acquire))
        {
            return Ok(Some(FailurePolicy::stop()));
        }
        if let Some(recovery) = self.recovery() {
            return Ok(Some(FailurePolicy {
                decision: SupervisionDecision::Restart,
                group: None,
                failure_context: recovery.failure_context.clone(),
                recovery: Some(recovery),
            }));
        }
        if all && decision == SupervisionDecision::Restart {
            if let Some(parent) = &parent {
                let children = parent.children.lock().unwrap();
                if let Some(recovery) = self.recovery() {
                    return Ok(Some(FailurePolicy {
                        decision,
                        group: None,
                        failure_context: recovery.failure_context.clone(),
                        recovery: Some(recovery),
                    }));
                }
                if parent.group_active.load(Ordering::Acquire) {
                    return Ok(Some(FailurePolicy::stop()));
                }
                if !children.contains_key(&self.id) {
                    return Ok(Some(FailurePolicy::stop()));
                }
                let eligible = children.values().all(|child| {
                    child.lifetime.recover.get().is_some() && !child.lifetime.is_stopping()
                });
                let now = tokio::time::Instant::now();
                let allowed = eligible
                    && now < deadline
                    && budget.is_none_or(|(maximum, within)| {
                        let mut history = parent.group_restarts.lock().unwrap();
                        history
                            .retain(|previous| now.saturating_duration_since(*previous) < within);
                        if history.len() >= maximum as usize {
                            false
                        } else {
                            history.push(now);
                            true
                        }
                    });
                if allowed {
                    #[cfg(test)]
                    {
                        let probe = parent.recovery_registration_probe.lock().unwrap().clone();
                        if let Some(probe) = probe {
                            probe();
                        }
                    }

                    // Pair registration with close without taking topology locks
                    // from close. A decision already in flight cannot install a
                    // fresh epoch after the parent has synchronously stopped.
                    let mut active = parent.active_recovery.lock().unwrap();
                    if parent.closed.load(Ordering::Acquire) {
                        return Ok(Some(FailurePolicy::stop()));
                    }
                    let members: Vec<_> = children
                        .values()
                        .map(|child| child.lifetime.clone())
                        .collect();
                    let weak_members: Vec<_> = members.iter().map(Arc::downgrade).collect();
                    let scope = Arc::downgrade(parent);
                    let id = Uuid::new_v4();
                    let recovery = parrot_api::trace::in_scope(failure_context, || {
                        recovery::Recovery::new(
                            id,
                            members.iter().map(|member| member.id),
                            deadline,
                            Box::new(move |_| {
                                if let Some(scope) = scope.upgrade() {
                                    let _children = scope.children.lock().unwrap();
                                    for member in weak_members.iter().filter_map(Weak::upgrade) {
                                        let mut assigned = member.recovery.lock().unwrap();
                                        if assigned
                                            .as_ref()
                                            .is_some_and(|assigned| assigned.id == id)
                                        {
                                            assigned.take();
                                        }
                                    }
                                    scope.group_active.store(false, Ordering::Release);
                                }
                            }),
                        )
                    });
                    *active = Some(Arc::downgrade(&recovery));
                    parent.group_active.store(true, Ordering::Release);
                    for member in &members {
                        *member.recovery.lock().unwrap() = Some(recovery.clone());
                    }
                    drop(active);
                    drop(children);
                    for member in &members {
                        member.recover.get().expect("all endpoints checked")(recovery.clone());
                    }
                    return Ok(Some(FailurePolicy {
                        decision,
                        group: None,
                        failure_context: recovery.failure_context.clone(),
                        recovery: Some(recovery),
                    }));
                }
                // A mixed instance/factory cohort cannot be reconstructed.
                // Retire it coherently instead of rebuilding only some members.
                decision = SupervisionDecision::Stop;
            } else {
                decision = SupervisionDecision::Stop;
            }
        } else if decision == SupervisionDecision::Restart
            && let Some((maximum, within)) = budget
        {
            let now = tokio::time::Instant::now();
            let mut restarts = self.restarts.lock().unwrap();
            restarts.retain(|previous| now.saturating_duration_since(*previous) < within);
            if restarts.len() >= maximum as usize {
                decision = SupervisionDecision::Stop;
            } else {
                restarts.push(now);
            }
        }
        let group = if all
            && decision != SupervisionDecision::Resume
            && let Some(parent) = parent
        {
            let children = parent.children.lock().unwrap();
            if parent.group_active.swap(true, Ordering::AcqRel) {
                None
            } else {
                Some(FailureGroup {
                    scope: parent.clone(),
                    siblings: children
                        .iter()
                        .filter(|(id, _)| **id != self.id)
                        .map(|(_, child)| child.stop.clone())
                        .collect(),
                })
            }
        } else {
            None
        };
        Ok(Some(FailurePolicy {
            decision,
            group,
            recovery: None,
            failure_context,
        }))
    }
    pub(crate) fn escalate(&self, error: Arc<ActorError>) -> bool {
        if let Some(parent) = self.parent.upgrade()
            && !parent.closed.load(Ordering::Acquire)
            && let Some(escalate) = parent.escalator.get()
        {
            escalate(error);
            true
        } else {
            false
        }
    }
    fn clear_decision(&self, id: Uuid) {
        for slot in &self.live_decision {
            let mut pending = slot.lock().unwrap();
            if pending.as_ref().is_some_and(|pending| pending.id == id) {
                pending.take();
                break;
            }
        }
    }
    async fn join_live_decision(
        &self,
    ) -> ActorResult<Option<(Option<PolicyChoice>, tokio::time::Instant)>> {
        // A non-resumable escalation is the cause that interrupted the parent
        // stage. The interrupted message receipt remains owned for disposal.
        let pending = self
            .live_decision
            .iter()
            .rev()
            .find_map(|slot| slot.lock().unwrap().clone());
        self.join_decision(pending).await
    }
    async fn join_decision(
        &self,
        pending: Option<LiveDecision>,
    ) -> ActorResult<Option<(Option<PolicyChoice>, tokio::time::Instant)>> {
        // Keep the stored receipt until actual completion, including cancellation
        // of any waiter. Clearing is scoped to the invocation's unique identity.
        match pending {
            None => Ok(None),
            Some(pending) => {
                let result = pending.result.await;
                self.clear_decision(pending.id);
                result
                    .map(|choice| Some((choice, pending.deadline)))
                    .map_err(ActorReceipt::into_error)
            }
        }
    }
    /// Join every interrupted decision before retiring a generation or policy.
    pub(crate) async fn settle_live_decision(&self) -> ActorResult<()> {
        let waits = self.live_decision.iter().map(|slot| {
            let pending = slot.lock().unwrap().clone();
            self.join_decision(pending)
        });
        futures::future::join_all(waits)
            .await
            .into_iter()
            .try_for_each(|result| result.map(|_| ()))
    }
    pub(crate) async fn dispose_policy(&self) -> ActorResult<()> {
        // Close admission before waiting on other lifecycle work. Existing
        // callbacks keep their leases, but no new callback can start.
        let admission = self.admission.get().map(|policy| policy.close());
        let decision = self.settle_live_decision().await;
        let policy = self.policy.dispose().await;
        let admission = match admission {
            Some(admission) => admission.await.map_err(ActorError::Panic),
            None => Ok(()),
        };
        admission.and(policy).and(decision)
    }
    /// Called by the publication owner before config/mailbox/scheduler clones.
    /// All generations reuse the same logical actor's policy; only the final
    /// lifecycle owner calls dispose_policy, including rejected construction.
    pub(crate) fn bind_admission(
        &self,
        config: &mut crate::thread::config::ThreadActorConfig,
        runtime: &tokio::runtime::Handle,
        slots: &Arc<Semaphore>,
    ) {
        if let Some(policy) = &config.message_admission {
            let managed = self.admission.get_or_init(|| {
                crate::thread::mailbox::admission::ManagedAdmission::new(
                    policy.clone(),
                    runtime,
                    slots,
                )
            });
            config.message_admission = Some(managed.clone());
        }
    }
    pub(crate) async fn wait(&self) -> ActorResult<()> {
        let mut receiver = self.done.subscribe();
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result.map_err(ActorReceipt::into_error);
            }
            receiver.changed().await.map_err(|_| ActorError::Stopped)?;
        }
    }
    pub(crate) fn publish_reference(&self, actor: BoxedActorRef) {
        if let Some(parent) = self.parent.upgrade() {
            let mut children = parent.children.lock().unwrap();
            if let Some(child) = children.get_mut(&self.id) {
                child.actor = actor;
            }
        }
    }
    pub(crate) fn hold_publication(&self) {
        self.publication.send_replace(None);
    }
    pub(crate) fn release_publication(&self, result: Result<(), ActorReceipt>) {
        self.publication.send_replace(Some(result));
    }
    pub(crate) fn finish(
        self: &Arc<Self>,
        result: Result<(), ActorReceipt>,
    ) -> BoxedFuture<'static, ()> {
        let mut publication = self.publication.subscribe();
        if let Some(published) = publication.borrow().clone() {
            return self.finish_actual(published.and(result));
        }
        let lifetime = self.clone();
        Box::pin(async move {
            let published = loop {
                if let Some(result) = publication.borrow_and_update().clone() {
                    break result;
                }
                if publication.changed().await.is_err() {
                    break Err(ActorReceipt::from_error(ActorError::InternalError(
                        "child creation owner lost its retirement receipt".into(),
                    )));
                }
            };
            lifetime.finish_actual(published.and(result)).await;
        })
    }
    fn finish_actual(&self, result: Result<(), ActorReceipt>) -> BoxedFuture<'static, ()> {
        let parent = self.parent.upgrade();
        let child = parent.as_ref().and_then(|scope| {
            let mut children = scope.children.lock().unwrap();
            let child = children.remove(&self.id);
            if child.is_some()
                && scope.closed.load(Ordering::Acquire)
                && let Err(error) = &result
            {
                // Publish failure while holding the membership lock. A join
                // that sees no member must already be able to see its outcome.
                scope
                    .retirement_failure
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| error.clone());
            }
            child
        });
        if let Some(recovery) = self.recovery() {
            recovery.abort("member terminated during cohort recovery");
            recovery.retired_after_abort(self.id);
        }
        self.done.send_replace(Some(result));
        Box::pin(async move {
            if let (Some(parent), Some(child)) = (parent, child) {
                parent.notify(child).await;
            }
        })
    }
}

/// One failed child owns the sibling operation. Other concurrent failures
/// retire independently instead of awaiting a group that is waiting on them.
pub(crate) struct FailureGroup {
    scope: Arc<ChildScope>,
    siblings: Vec<StopChild>,
}
impl FailureGroup {
    pub(crate) async fn stop(&self) -> ActorResult<()> {
        let results = futures::future::join_all(self.siblings.iter().map(|stop| stop())).await;
        for result in results {
            result?;
        }
        Ok(())
    }
}
impl Drop for FailureGroup {
    fn drop(&mut self) {
        self.scope.group_active.store(false, Ordering::Release);
    }
}
pub(crate) struct FailurePolicy {
    pub(crate) decision: SupervisionDecision,
    pub(crate) group: Option<FailureGroup>,
    pub(crate) recovery: Option<Arc<recovery::Recovery>>,
    pub(crate) failure_context: Option<parrot_api::trace::TraceContext>,
}
impl FailurePolicy {
    fn stop() -> Self {
        Self {
            decision: SupervisionDecision::Stop,
            group: None,
            recovery: None,
            failure_context: None,
        }
    }
    pub(crate) async fn stop_siblings(&self) -> ActorResult<()> {
        match &self.group {
            Some(group) => group.stop().await,
            None => Ok(()),
        }
    }
}
#[cfg(test)]
mod live_owner_tests {
    use super::*;

    fn scope_with_child() -> (Arc<ChildScope>, Arc<ChildLifetime>) {
        let scope = ChildScope::new(Duration::from_secs(1), Arc::new(Semaphore::new(1)));
        let mut child = None;
        scope
            .spawn(|lifetime| {
                child = Some(lifetime.clone());
                let reference = parrot_api::address::ActorPath::placeholder("/scope-unit/child")
                    .target()
                    .clone_boxed();
                Ok((
                    reference,
                    Arc::new(move || {
                        let lifetime = lifetime.clone();
                        Box::pin(async move { lifetime.wait().await })
                    }),
                ))
            })
            .unwrap();
        (scope, child.unwrap())
    }

    #[tokio::test]
    async fn retirement_result_survives_cancelled_and_repeated_scope_joins() {
        for failed in [false, true] {
            let (scope, child) = scope_with_child();
            let mut cancelled = Box::pin(scope.join());
            assert!(futures::poll!(&mut cancelled).is_pending());
            drop(cancelled);
            child
                .finish(if failed {
                    Err(ActorReceipt::from_error(ActorError::InternalError(
                        "descendant resource disposal failed".into(),
                    )))
                } else {
                    Ok(())
                })
                .await;
            assert!(scope.view().is_empty());
            let (first, second) = tokio::join!(scope.join(), scope.join());
            for result in [first, second, scope.join().await] {
                assert_eq!(result.is_err(), failed);
                if let Err(error) = result {
                    assert!(
                        error
                            .to_string()
                            .contains("descendant resource disposal failed")
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn independent_retirement_before_scope_close_is_not_replayed() {
        let (scope, child) = scope_with_child();
        child
            .finish(Err(ActorReceipt::from_error(ActorError::InternalError(
                "independent child cleanup failed".into(),
            ))))
            .await;
        assert!(child.wait().await.is_err());
        assert!(scope.view().is_empty());
        scope.join().await.unwrap();
        scope.join().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retiring_execution_runtime_does_not_abandon_pending_decider() {
        let entered = Arc::new(Notify::new());
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let release = Release(gate.clone());
        let signal = entered.clone();
        let blocked = gate.clone();
        let lifetime = ChildLifetime::with_policy(
            Weak::new(),
            Some(SupervisorStrategyType::OneForOne(
                parrot_api::supervisor::OneForOneStrategy {
                    max_restarts: 0,
                    within: Duration::from_secs(60),
                    decider: parrot_api::supervisor::BasicDecisionFn::new(move |_| {
                        signal.notify_one();
                        let mut open = blocked.0.lock().unwrap();
                        while !*open {
                            open = blocked.1.wait(open).unwrap();
                        }
                        SupervisionDecision::Resume
                    }),
                },
            )),
            Arc::new(Semaphore::new(1)),
        );
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let owner = lifetime.clone();
        let system_runtime = tokio::runtime::Handle::current();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async { tokio::select! {
                _ = stopped => {},
                _ = owner.resume_message(Arc::new(ActorError::Panic("failure".into())),tokio::time::Instant::now()+Duration::from_secs(10),false,&system_runtime) => panic!("decision completed before gate opened"),
            }});
            rt.shutdown_background();
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        stop.send(()).unwrap();
        thread.join().unwrap();
        let mut disposal = Box::pin(lifetime.dispose_policy());
        let premature = tokio::time::timeout(Duration::from_millis(50), &mut disposal).await;
        drop(release);
        assert!(
            premature.is_err(),
            "callback still owns its error and policy; receipt must stay pending: {premature:?}"
        );
        disposal.await.unwrap();
    }
}

#[cfg(test)]
mod interrupted_decision_tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_failure_policy_or_disposal_waiter_keeps_the_real_decision_receipt() {
        for escalating in [false, true] {
            for disposing in [false, true] {
                for panics in [false, true] {
                    let entered = Arc::new(Notify::new());
                    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
                    struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
                    impl Drop for Release {
                        fn drop(&mut self) {
                            *self.0.0.lock().unwrap() = true;
                            self.0.1.notify_all();
                        }
                    }
                    let release = Release(gate.clone());
                    let blocked = gate.clone();
                    let signal = entered.clone();
                    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let finished = completed.clone();
                    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let called = calls.clone();
                    let lifetime = ChildLifetime::with_policy(
                        Weak::new(),
                        Some(SupervisorStrategyType::OneForOne(
                            parrot_api::supervisor::OneForOneStrategy {
                                max_restarts: 1,
                                within: Duration::from_secs(30),
                                decider: parrot_api::supervisor::BasicDecisionFn::new(move |_| {
                                    called.fetch_add(1, Ordering::SeqCst);
                                    signal.notify_one();
                                    let mut open = blocked.0.lock().unwrap();
                                    while !*open {
                                        open = blocked.1.wait(open).unwrap();
                                    }
                                    finished.fetch_add(1, Ordering::SeqCst);
                                    assert!(!panics, "pending decider panic");
                                    SupervisionDecision::Restart
                                }),
                            },
                        )),
                        Arc::new(Semaphore::new(2)),
                    );
                    let error = Arc::new(ActorError::MessageHandlingError(
                        "old failed message".into(),
                    ));
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                    let runtime = tokio::runtime::Handle::current();
                    let mut execution: BoxedFuture<'_, bool> = if escalating {
                        Box::pin(lifetime.resume_escalation(error.clone(), deadline, &runtime))
                    } else {
                        Box::pin(lifetime.resume_message(error.clone(), deadline, false, &runtime))
                    };
                    assert!(futures::poll!(&mut execution).is_pending());
                    tokio::time::timeout(Duration::from_secs(2), entered.notified())
                        .await
                        .unwrap();
                    drop(execution);
                    let mut interrupted: BoxedFuture<'_, ActorResult<()>> = if disposing {
                        Box::pin(lifetime.dispose_policy())
                    } else {
                        Box::pin(async {
                            lifetime
                                .failure_policy(error, Arc::new(Semaphore::new(2)), deadline)
                                .await
                                .map(|_| ())
                        })
                    };
                    assert!(futures::poll!(&mut interrupted).is_pending());
                    drop(interrupted);
                    let mut final_owner = Box::pin(lifetime.dispose_policy());
                    let premature =
                        tokio::time::timeout(Duration::from_millis(30), &mut final_owner).await;
                    assert_eq!(completed.load(Ordering::SeqCst), 0);
                    drop(release);
                    assert!(
                        premature.is_err(),
                        "cancelled waiter lost the only retirement receipt: {premature:?}"
                    );
                    let result = final_owner.await;
                    assert_eq!(result.is_err(), panics);
                    if let Err(error) = result {
                        assert!(
                            error.to_string().contains("pending decider panic"),
                            "{error}"
                        );
                    }
                    assert_eq!(completed.load(Ordering::SeqCst), 1);
                    assert_eq!(calls.load(Ordering::SeqCst), 1);
                }
            }
        }
    }
}

#[cfg(test)]
mod unpolled_escalation_tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unpolled_escalation_waiter_already_owns_callback_retirement_receipt() {
        for panics in [false, true] {
            let entered = Arc::new(Notify::new());
            let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
            impl Drop for Release {
                fn drop(&mut self) {
                    *self.0.0.lock().unwrap() = true;
                    self.0.1.notify_all();
                }
            }
            let release = Release(gate.clone());
            let blocked = gate.clone();
            let signal = entered.clone();
            let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let finished = completed.clone();
            let lifetime = ChildLifetime::with_policy(
                Weak::new(),
                Some(SupervisorStrategyType::OneForOne(
                    parrot_api::supervisor::OneForOneStrategy {
                        max_restarts: 0,
                        within: Duration::from_secs(30),
                        decider: parrot_api::supervisor::BasicDecisionFn::new(move |_| {
                            signal.notify_one();
                            let (open, timed) = blocked
                                .1
                                .wait_timeout_while(
                                    blocked.0.lock().unwrap(),
                                    Duration::from_secs(8),
                                    |open| !*open,
                                )
                                .unwrap();
                            assert!(!timed.timed_out() || *open, "test gate not released");
                            finished.fetch_add(1, Ordering::SeqCst);
                            assert!(!panics, "unpolled decision panic");
                            SupervisionDecision::Resume
                        }),
                    },
                )),
                Arc::new(Semaphore::new(2)),
            );
            let waiter = lifetime.start_escalation(
                Arc::new(ActorError::MessageHandlingError("child".into())),
                tokio::time::Instant::now() + Duration::from_secs(3),
                false,
                &tokio::runtime::Handle::current(),
            );
            // A hook has returned, but its runtime waiter has never been polled.
            // Cancelling that waiter must neither forget nor delay receipt registration.
            drop(waiter);
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            let mut disposing = Box::pin(lifetime.dispose_policy());
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut disposing)
                    .await
                    .is_err()
            );
            assert_eq!(completed.load(Ordering::SeqCst), 0);
            drop(release);
            let result = disposing.await;
            assert_eq!(result.is_err(), panics);
            if let Err(error) = result {
                assert!(
                    error.to_string().contains("unpolled decision panic"),
                    "{error}"
                );
            }
            assert_eq!(completed.load(Ordering::SeqCst), 1);
        }
    }
}

#[cfg(test)]
mod source_receipt_contracts {
    use super::*;
    use std::{error::Error, fmt, sync::atomic::AtomicUsize};
    struct Business(Arc<AtomicUsize>);
    impl fmt::Debug for Business {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("ChildBusiness")
        }
    }
    impl fmt::Display for Business {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("retirement must not format an unknown business error");
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("retirement must not traverse unknown business errors");
        }
    }
    fn failure(identity: &Arc<AtomicUsize>) -> ActorReceipt {
        ActorReceipt::from_error(ActorError::Other(anyhow::Error::new(Business(
            identity.clone(),
        ))))
    }
    fn original(result: ActorResult<()>, identity: &Arc<AtomicUsize>) {
        let error = result.expect_err("retirement failure must remain observable");
        let mut current = Some(&error as &(dyn Error + 'static));
        for _ in 0..32 {
            let Some(error) = current else { break };
            if let Some(error) = error.downcast_ref::<Business>() {
                assert!(Arc::ptr_eq(&error.0, identity));
                assert_eq!(identity.load(Ordering::SeqCst), 0);
                return;
            }
            current = error.source();
        }
        panic!("the original business source was replaced");
    }
    fn child() -> (Arc<ChildScope>, Arc<ChildLifetime>) {
        let scope = ChildScope::new(Duration::from_secs(1), Arc::new(Semaphore::new(1)));
        let mut owned = None;
        scope
            .spawn(|lifetime| {
                owned = Some(lifetime.clone());
                let actor = parrot_api::address::ActorPath::placeholder("/source-state/child")
                    .target()
                    .clone_boxed();
                Ok((
                    actor,
                    Arc::new(move || {
                        let lifetime = lifetime.clone();
                        Box::pin(async move { lifetime.wait().await })
                    }),
                ))
            })
            .unwrap();
        (scope, owned.unwrap())
    }
    #[tokio::test]
    async fn first_scope_wait_preserves_source_before_any_repeated_join() {
        let (scope, child) = child();
        let identity = Arc::new(AtomicUsize::new(0));
        let mut first = Box::pin(scope.join());
        assert!(futures::poll!(&mut first).is_pending());
        child.finish(Err(failure(&identity))).await;
        original(first.await, &identity);
    }
    #[tokio::test]
    async fn removed_child_scope_receipt_preserves_original_source() {
        let (scope, child) = child();
        let identity = Arc::new(AtomicUsize::new(0));
        scope.close();
        child.finish(Err(failure(&identity))).await;
        assert!(scope.view().is_empty());
        original(scope.join().await, &identity);
        original(scope.join().await, &identity);
    }
    #[tokio::test]
    async fn cancelled_lifetime_waiter_cannot_consume_source_receipt() {
        let (_, child) = child();
        let identity = Arc::new(AtomicUsize::new(0));
        let mut cancelled = Box::pin(child.wait());
        assert!(futures::poll!(&mut cancelled).is_pending());
        drop(cancelled);
        child.finish(Err(failure(&identity))).await;
        original(child.wait().await, &identity);
        original(child.wait().await, &identity);
    }
    #[tokio::test]
    async fn publication_failure_preserves_source_when_actor_retirement_also_fails() {
        let (scope, child) = child();
        let publication = Arc::new(AtomicUsize::new(0));
        let actor = Arc::new(AtomicUsize::new(0));
        child.hold_publication();
        let mut first = Box::pin(scope.join());
        assert!(futures::poll!(&mut first).is_pending());
        let mut finish = child.finish(Err(failure(&actor)));
        assert!(futures::poll!(&mut finish).is_pending());
        assert!(!scope.view().is_empty());
        child.release_publication(Err(failure(&publication)));
        finish.await;
        original(first.await, &publication);
        original(scope.join().await, &publication);
        assert_eq!(actor.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn successful_publication_does_not_replace_actor_retirement_source() {
        let (_, child) = child();
        let identity = Arc::new(AtomicUsize::new(0));
        child.hold_publication();
        let mut finish = child.finish(Err(failure(&identity)));
        assert!(futures::poll!(&mut finish).is_pending());
        child.release_publication(Ok(()));
        finish.await;
        original(child.wait().await, &identity);
    }
}

#[cfg(test)]
mod parent_close_recovery_contracts {
    use super::recovery::{Recovery, Step};
    use super::*;
    use std::sync::atomic::AtomicUsize;
    fn scope_members() -> (Arc<ChildScope>, Vec<Arc<ChildLifetime>>, Arc<Semaphore>) {
        let slots = Arc::new(Semaphore::new(2));
        let scope = ChildScope::new(Duration::from_secs(3), slots.clone());
        scope.set_strategy(SupervisorStrategyType::OneForAll(
            parrot_api::supervisor::OneForAllStrategy {
                max_restarts: 1,
                within: Duration::from_secs(30),
                decider: parrot_api::supervisor::BasicDecisionFn::new(|_| {
                    SupervisionDecision::Restart
                }),
            },
        ));
        let mut members = Vec::new();
        for id in 0..2 {
            scope
                .spawn(|lifetime| {
                    lifetime.bind_recovery(Arc::new(|_| {}));
                    members.push(lifetime.clone());
                    let actor =
                        parrot_api::address::ActorPath::placeholder(format!("/close-epoch/{id}"))
                            .target()
                            .clone_boxed();
                    Ok((
                        actor,
                        Arc::new(move || {
                            let lifetime = lifetime.clone();
                            Box::pin(async move { lifetime.wait().await })
                        }),
                    ))
                })
                .unwrap();
        }
        (scope, members, slots)
    }
    async fn fixture() -> (Arc<ChildScope>, Vec<Arc<ChildLifetime>>, Arc<Recovery>) {
        let (scope, members, slots) = scope_members();
        let policy = members[0]
            .failure_policy(
                Arc::new(ActorError::InternalError("trigger".into())),
                slots,
                tokio::time::Instant::now() + Duration::from_secs(3),
            )
            .await
            .unwrap()
            .unwrap();
        (scope, members, policy.recovery.unwrap())
    }
    async fn barrier(group: &Arc<Recovery>, members: &[Arc<ChildLifetime>], step: Step) {
        let (first, second) = tokio::join!(
            group.arrive(members[0].id, step),
            group.arrive(members[1].id, step)
        );
        first.unwrap();
        second.unwrap();
    }
    async fn close_before_commit(phase: usize) {
        let (scope, members, group) = fixture().await;
        let activated = Arc::new(AtomicUsize::new(0));
        let activation = activated.clone();
        group.on_activation(Box::new(move || {
            activation.fetch_add(1, Ordering::SeqCst);
        }));
        if phase >= 1 {
            barrier(&group, &members, Step::Retired).await;
        }
        if phase >= 2 {
            barrier(&group, &members, Step::Prepared).await;
        }
        let mut published = Box::pin(group.arrive(members[0].id, Step::Published));
        if phase == 3 {
            assert!(futures::poll!(&mut published).is_pending());
        }
        // This is the actual synchronous close boundary used by parent stop.
        // No child stop/join or executor yield may be needed to cancel the epoch.
        scope.close();
        let cancelled = group.control.is_cancelled();
        if !cancelled {
            group.abort("test cleanup after missing parent cancellation");
        }
        if phase == 3 {
            assert!(published.await.is_err());
        }
        assert!(group.activation().await.is_err());
        assert!(scope.group_active.load(Ordering::Acquire));
        members[0].finish(Ok(())).await;
        assert!(
            scope.group_active.load(Ordering::Acquire),
            "abort is not a disposal receipt"
        );
        members[1].finish(Ok(())).await;
        assert!(!scope.group_active.load(Ordering::Acquire));
        assert!(scope.view().is_empty());
        scope.join().await.unwrap();
        assert_eq!(activated.load(Ordering::SeqCst), 0);
        assert!(
            cancelled,
            "parent close must synchronously cancel its uncommitted recovery"
        );
    }
    #[tokio::test]
    async fn close_before_retirement() {
        close_before_commit(0).await;
    }
    #[tokio::test]
    async fn close_during_preparation() {
        close_before_commit(1).await;
    }
    #[tokio::test]
    async fn close_before_publication() {
        close_before_commit(2).await;
    }
    #[tokio::test]
    async fn close_before_last_publication() {
        close_before_commit(3).await;
    }
    #[tokio::test]
    async fn close_after_commit_does_not_retroactively_abort_generation() {
        let (scope, members, group) = fixture().await;
        for step in [Step::Retired, Step::Prepared, Step::Published] {
            barrier(&group, &members, step).await;
        }
        scope.close();
        group.activation().await.unwrap();
        assert!(group.is_committed());
        assert!(!group.control.is_cancelled());
        for member in members {
            member.finish(Ok(())).await;
        }
        scope.join().await.unwrap();
    }
    #[tokio::test]
    async fn closed_scope_does_not_start_a_new_recovery() {
        let (scope, members, group) = fixture().await;
        for step in [Step::Retired, Step::Prepared, Step::Published] {
            barrier(&group, &members, step).await;
        }
        scope.close();
        let policy = members[0]
            .failure_policy(
                Arc::new(ActorError::InternalError("later".into())),
                Arc::new(Semaphore::new(2)),
                tokio::time::Instant::now() + Duration::from_secs(3),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(policy.decision, SupervisionDecision::Stop);
        assert!(policy.recovery.is_none());
        for member in members {
            member.finish(Ok(())).await;
        }
        scope.join().await.unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parent_close_wins_against_a_decision_waiting_to_register_its_epoch() {
        let (scope, members, slots) = scope_members();
        let entered = Arc::new(Notify::new());
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let release = Release(gate.clone());
        let blocked = gate.clone();
        let signal = entered.clone();
        *scope.recovery_registration_probe.lock().unwrap() = Some(Arc::new(move || {
            signal.notify_one();
            let (open, timeout) = blocked
                .1
                .wait_timeout_while(blocked.0.lock().unwrap(), Duration::from_secs(5), |open| {
                    !*open
                })
                .unwrap();
            assert!(
                !timeout.timed_out() || *open,
                "registration probe not released"
            );
        }));
        let member = members[0].clone();
        let task = tokio::spawn(async move {
            member
                .failure_policy(
                    Arc::new(ActorError::InternalError("racing decision".into())),
                    slots,
                    tokio::time::Instant::now() + Duration::from_secs(3),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        scope.close();
        drop(release);
        let policy = task.await.unwrap().unwrap().unwrap();
        let prevented = policy.decision == SupervisionDecision::Stop && policy.recovery.is_none();
        if let Some(group) = policy.recovery {
            group.abort("test cleanup after late epoch installation");
        }
        for member in members {
            member.finish(Ok(())).await;
        }
        scope.join().await.unwrap();
        assert!(
            prevented,
            "a decision must not register a recovery after parent close completes"
        );
    }
}
