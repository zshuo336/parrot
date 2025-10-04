//! M3 supervision executor: connects panic capture to supervisor decisions.
//!
//! Execution pipeline (POC `supervision` ported to the production system):
//!
//! ```text
//! actor panics (worker / processor catch)
//!   → system.on_child_panic(path, msg)          [panic hook]
//!   → supervise_entry: windowed decision        [state machine]
//!       Restart{max, within} → respawn via stored factory (window-limited;
//!                              over-budget → Escalate)
//!       Stop                  → remove entry, notify watchers(Panic)
//!       Escalate              → stop self + bubble up parent chain
//!       Resume                → not wired: a panicked actor cannot resume;
//!                              treated as Stop for panic deaths (documented)
//!   → notify_termination(path, reason)          [DeathWatch, High+Block]
//!   → Terminated{path, reason: DeathReason}     [public since M3]
//! ```
//!
//! Design choices vs POC:
//! - The restart factory is a typed `Fn() -> A` closure captured at
//!   `spawn_supervised` time (no `Arc<Mutex<Factory>>` compromise: the
//!   factory is `Fn`, immutable and freely shareable).
//! - Decisions run on the system's async runtime (single-threaded per
//!   system registry mutations stay Mutex-guarded).
//! - Escalation walks the parent chain (each supervisor gets a
//!   `ChildFailure` control message first; a supervisor without a strategy
//!   defaults to the system default strategy — semantic change #1: the
//!   declared default (Restart 3/10s) is now actually ENFORCED).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
#[allow(unused_imports)] // 测试模块需要 Duration
use std::time::{Duration, Instant};

use parrot_api::supervisor::DeathReason;

use crate::thread::config::SupervisorStrategy;
use crate::thread::system::ThreadActorSystem;

/// Windowed restart history for one supervised child.
pub(crate) struct SupervisionEntry {
    /// Strategy in force for this child.
    pub strategy: SupervisorStrategy,
    /// Restart timestamps inside the sliding window.
    pub restarts: Vec<Instant>,
    /// Typed respawn factory (produces a fresh actor instance).
    pub respawn: Arc<dyn Fn() -> crate::thread::system::ErasedSpawnBoxTyped + Send + Sync>,
}

/// Internal marker type: the erased spawn box re-exported for factory sigs.
pub use crate::thread::system::ErasedSpawnBoxTyped;

/// The decision produced by the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Respawn the child via its factory.
    Restart,
    /// Remove the child permanently.
    Stop,
    /// Stop the child AND escalate to the parent chain.
    Escalate(String),
}

/// Pure decision function: strategy × window history → decision.
///
/// Exposed crate-internally for unit tests (mirrors POC semantics).
pub(crate) fn decide(
    strategy: &SupervisorStrategy,
    restarts: &mut Vec<Instant>,
    now: Instant,
) -> Decision {
    match strategy {
        SupervisorStrategy::Stop => Decision::Stop,
        SupervisorStrategy::Resume => {
            // A panicked actor cannot "resume": its mailbox/processor state
            // is wedged. Treat as Stop for panic deaths. (Resume remains
            // meaningful for *error* deaths, wired in a later milestone.)
            Decision::Stop
        }
        SupervisorStrategy::Escalate => Decision::Escalate("strategy=Escalate".into()),
        SupervisorStrategy::Restart {
            max_retries,
            within,
        } => {
            // Sliding window: keep only restarts inside the window.
            restarts.retain(|t| now.duration_since(*t) < *within);
            if restarts.len() >= *max_retries {
                Decision::Escalate(format!(
                    "restart budget exhausted ({} in {:?})",
                    max_retries, within
                ))
            } else {
                restarts.push(now);
                Decision::Restart
            }
        }
    }
}

/// System-side supervision state (path → entry).
pub(crate) struct SupervisionRegistry {
    entries: Mutex<HashMap<String, SupervisionEntry>>,
}

impl SupervisionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn register(&self, path: &str, entry: SupervisionEntry) {
        self.entries.lock().unwrap().insert(path.to_string(), entry);
    }

    #[allow(dead_code)] // M3 状态面完整性保留
    pub(crate) fn unregister(&self, path: &str) {
        self.entries.lock().unwrap().remove(path);
    }

    #[allow(dead_code)] // M3 状态面完整性保留
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    #[allow(dead_code)] // M3 状态面完整性保留
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record a restart timestamp (called after a successful respawn).
    pub(crate) fn record_restart(&self, path: &str) {
        let mut g = self.entries.lock().unwrap();
        if let Some(e) = g.get_mut(path) {
            e.restarts.push(Instant::now());
        }
    }
}

impl ThreadActorSystem {
    /// M3: called when a supervised (or any) actor dies of a panic.
    ///
    /// Executes the supervision state machine: decide → act → notify.
    pub(crate) async fn on_child_panic(self: &Arc<Self>, path: &str, _panic_msg: String) {
        use tracing::{error, info};

        let (decision, respawn, strategy) = {
            let mut guard = self.supervision_registry.entries.lock().unwrap();
            match guard.get_mut(path) {
                None => {
                    // No supervision entry: default strategy enforcement
                    // (semantic change #1) — the system default applies.
                    let default_strategy = self.config().default_supervisor_strategy.clone();
                    let mut restarts = Vec::new();
                    let d = decide(&default_strategy, &mut restarts, Instant::now());
                    (d, None, default_strategy)
                }
                Some(entry) => {
                    let d = decide(&entry.strategy, &mut entry.restarts, Instant::now());
                    (d, Some(entry.respawn.clone()), entry.strategy.clone())
                }
            }
        };

        let _ = strategy;
        match decision {
            Decision::Restart => {
                if let Some(factory) = respawn {
                    info!("Supervision: restarting panicked actor {}", path);
                    // 1. Remove the dead actor's registry entry (best-effort;
                    //    the panicked mailbox may already be descheduled).
                    let _ = self.stop_actor_for_panic(path).await;
                    // 2. Respawn via the typed factory at the SAME path
                    //    (supervision tree shape is stable across restarts).
                    let new_payload = factory();
                    let strategy_for_respawn = self.config().default_supervisor_strategy.clone();
                    let spawned = new_payload
                        .spawn_at_path(
                            self.clone(),
                            path.to_string(),
                            crate::thread::config::ThreadActorConfig {
                                supervisor_strategy: Some(strategy_for_respawn),
                                ..Default::default()
                            },
                        )
                        .await;
                    match spawned {
                        Ok(r) => {
                            let _ = r;
                            self.supervision_registry.record_restart(path);
                        }
                        Err(e) => {
                            error!("Supervision: respawn of {} failed: {}", path, e);
                        }
                    }
                } else {
                    // No factory: cannot restart; treat as stop.
                    error!(
                        "Supervision: restart decided for {} but no factory registered; stopping",
                        path
                    );
                    let _ = self.stop_actor_for_panic(path).await;
                }
            }
            Decision::Stop => {
                info!("Supervision: stopping panicked actor {}", path);
                let _ = self.stop_actor_for_panic(path).await;
            }
            Decision::Escalate(msg) => {
                error!("Supervision: escalating failure of {}: {}", path, msg);
                // Stop the failed child, then bubble the failure up the
                // parent chain (each ancestor applies its own strategy).
                let _ = self.stop_actor_for_panic(path).await;
                self.escalate_to_parents(path, &msg).await;
            }
        }
    }

    /// Remove a panicked actor from the registry and notify watchers with
    /// the Panic death reason (M2 High+Block lane: never dropped).
    async fn stop_actor_for_panic(
        &self,
        path: &str,
    ) -> Result<(), crate::thread::error::SystemError> {
        // Remove registry entry + close mailbox + notify (Panic reason).
        // Reuses stop_actor's mechanics but with a Panic death reason.
        let entry = {
            let mut registry = self.registry.write().unwrap();
            registry.remove(path)
        };
        let entry = match entry {
            Some(e) => e,
            None => return Ok(()), // already gone
        };

        // The actor panicked; a graceful Stop control message would be
        // dropped by the wedged processor. Skip the lifecycle call.
        drop(entry.mailbox.get_processor());

        let results = (
            self.scheduler_group.shared_scheduler.deschedule(path),
            self.scheduler_group
                .dedicated_scheduler
                .deschedule(path)
                .await,
        );
        let _ = results;

        entry.mailbox.close().await;
        self.notify_termination_pub(path, DeathReason::Panic("actor panicked".into()))
            .await;
        Ok(())
    }

    /// Public-reason variant of notify_termination (panics carry Panic).
    async fn notify_termination_pub(&self, path: &str, reason: DeathReason) {
        self.notify_termination(path, reason).await;
    }

    /// Escalate a failure to the failed actor's parent chain.
    ///
    /// Each ancestor receives a `ChildFailure` control message; an ancestor
    /// that itself dies during handling escalates further (walk stops at the
    /// root or when an ancestor's strategy absorbs the failure).
    async fn escalate_to_parents(&self, path: &str, msg: &str) {
        // Determine the parent chain from the path convention
        // ("/user/parent/child" → "/user/parent"). Path-based parenting is
        // the thread engine's convention (contexts hold parent refs, but the
        // supervision walk only needs the chain order).
        let mut current = path.to_string();
        #[allow(clippy::never_loop)] // break 均为终止条件（哨兵路径保护）
        while let Some(pos) = current.rfind('/') {
            // Don't strip past "/user"
            if current == "/user" || current.ends_with("/user") {
                break;
            }
            current.truncate(pos);
            if current.is_empty() || current == "/" {
                break;
            }

            // Deliver a ChildFailure control message to the ancestor.
            let failure = crate::thread::envelope::ControlMessage::ChildFailure {
                path: path.to_string(),
                reason: msg.to_string(),
            };
            let mailbox = self.get_mailbox(&current);
            if let Some(mailbox) = mailbox {
                let _ = mailbox
                    .push_with_priority(
                        Box::new(failure),
                        crate::thread::config::BackpressureStrategy::Block,
                        true,
                    )
                    .await;
            }
            // Only the immediate parent gets the control message; further
            // escalation happens if THAT parent panics handling it.
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restart_strategy(max: usize, window: Duration) -> SupervisorStrategy {
        SupervisorStrategy::Restart {
            max_retries: max,
            within: window,
        }
    }

    #[test]
    fn decision_restart_under_budget() {
        let mut restarts = vec![];
        let d = decide(
            &restart_strategy(3, Duration::from_secs(10)),
            &mut restarts,
            Instant::now(),
        );
        assert_eq!(d, Decision::Restart);
        assert_eq!(restarts.len(), 1);
    }

    #[test]
    fn decision_escalate_when_window_exhausted() {
        let mut restarts = vec![Instant::now(); 3];
        let d = decide(
            &restart_strategy(3, Duration::from_secs(10)),
            &mut restarts,
            Instant::now(),
        );
        assert!(matches!(d, Decision::Escalate(_)));
    }

    #[test]
    fn decision_window_slides() {
        // Old restarts fall out of the window → budget available again.
        let old = Instant::now() - Duration::from_secs(11);
        let mut restarts = vec![old; 5];
        let d = decide(
            &restart_strategy(3, Duration::from_secs(10)),
            &mut restarts,
            Instant::now(),
        );
        assert_eq!(d, Decision::Restart);
        assert_eq!(restarts.len(), 1, "stale restarts pruned");
    }

    #[test]
    fn decision_stop_strategy_stops() {
        let mut restarts = vec![];
        let d = decide(&SupervisorStrategy::Stop, &mut restarts, Instant::now());
        assert_eq!(d, Decision::Stop);
    }

    #[test]
    fn decision_escalate_strategy_escalates() {
        let mut restarts = vec![];
        let d = decide(&SupervisorStrategy::Escalate, &mut restarts, Instant::now());
        assert!(matches!(d, Decision::Escalate(_)));
    }

    #[test]
    fn decision_resume_maps_to_stop_for_panics() {
        let mut restarts = vec![];
        let d = decide(&SupervisorStrategy::Resume, &mut restarts, Instant::now());
        assert_eq!(d, Decision::Stop);
    }
}
