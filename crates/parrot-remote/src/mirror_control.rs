//! Negotiated, generation-owned remote mirror sessions. Opening a session never
//! installs a policy. An old apply cannot resurrect a retired session, even when
//! its owner name has been reused. Retirement receipts await workers and encoders.
use crate::mirror::{WireMirrorGroup, WireMirrorOptions, WireMirrorPolicy, WireMirrorRegistry};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorSessionId {
    pub incarnation: [u8; 16],
    pub sequence: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorPolicySpec {
    pub source_prefix: String,
    pub shadow_path: String,
    pub capacity: u32,
    pub max_message_bytes: u32,
    pub max_pending_bytes: u32,
    pub delivery_timeout_ms: u32,
}
impl MirrorPolicySpec {
    pub fn new(source: impl Into<String>, shadow: impl Into<String>) -> Self {
        Self {
            source_prefix: source.into(),
            shadow_path: shadow.into(),
            capacity: 256,
            max_message_bytes: 1024 * 1024,
            max_pending_bytes: 8 * 1024 * 1024,
            delivery_timeout_ms: 100,
        }
    }
    fn expand(&self) -> Result<(WireMirrorPolicy, WireMirrorOptions), String> {
        let policy = WireMirrorPolicy::new(&self.source_prefix, &self.shadow_path);
        let options = WireMirrorOptions {
            capacity: self.capacity as usize,
            max_message_bytes: self.max_message_bytes as usize,
            max_pending_bytes: self.max_pending_bytes as usize,
            delivery_timeout: Duration::from_millis(self.delivery_timeout_ms.into()),
        };
        policy.validate().map_err(|e| e.to_string())?;
        options.validate().map_err(|e| e.to_string())?;
        Ok((policy, options))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MirrorControl {
    Open {
        owner: String,
    },
    Apply {
        session: MirrorSessionId,
        revision: u64,
        policies: Vec<MirrorPolicySpec>,
    },
    Status {
        session: MirrorSessionId,
    },
    Close {
        session: MirrorSessionId,
    },
    /// Read the next allocation slot without allocating resources.
    Candidate,
    /// Compare-and-create a specific slot. Retired slots can never reopen.
    Allocate {
        owner: String,
        session: MirrorSessionId,
    },
}
impl MirrorControl {
    /// Reject a reply for another operation or generation. Transport callback
    /// correlation alone cannot prove that a policy revision was applied.
    pub(crate) fn accepts(&self, reply: &MirrorControlReply) -> bool {
        match (self, reply) {
            (Self::Candidate, MirrorControlReply::Candidate { session }) => session.sequence != 0,
            (
                Self::Allocate { owner, session },
                MirrorControlReply::Session {
                    owner: actual,
                    session: id,
                    ..
                },
            ) => owner == actual && session == id,
            (
                Self::Open { owner },
                MirrorControlReply::Session {
                    session,
                    owner: actual,
                    ..
                },
            ) => owner == actual && session.sequence != 0,
            (
                Self::Apply {
                    session,
                    revision,
                    policies,
                },
                MirrorControlReply::Session {
                    session: actual,
                    revision: applied,
                    policies: installed,
                    phase: MirrorSessionPhase::Active,
                    ..
                },
            ) => session == actual && revision == applied && policies == installed,
            (
                Self::Status { session },
                reply @ (MirrorControlReply::Session { .. } | MirrorControlReply::Retired { .. }),
            ) => *session == reply.session(),
            (Self::Close { session }, MirrorControlReply::Retired { session: actual }) => {
                session == actual
            }
            _ => false,
        }
    }
    pub(crate) fn required_capabilities(&self) -> u32 {
        crate::handshake::caps::MIRROR_CONTROL
            | if matches!(self, Self::Apply {policies,..} if policies.iter().any(|p| crate::mirror::remote_shadow(&p.shadow_path).is_some()))
            {
                crate::handshake::caps::MIRROR_SHADOW | crate::handshake::caps::TELL_COMPLETION
            } else {
                0
            }
            | if matches!(self, Self::Candidate | Self::Allocate { .. }) {
                crate::handshake::caps::MIRROR_ALLOCATION
            } else {
                0
            }
    }
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Open { owner } | Self::Allocate { owner, .. }
                if owner.is_empty() || owner.len() > 128 || owner.chars().any(char::is_control) =>
            {
                Err("mirror owner must contain 1..128 bytes without control characters".into())
            }
            Self::Apply {
                revision, policies, ..
            } => {
                if *revision == 0 || policies.is_empty() || policies.len() > 64 {
                    return Err(
                        "mirror apply requires a positive revision and 1..64 policies".into(),
                    );
                }
                let mut sources = std::collections::BTreeSet::new();
                for policy in policies {
                    policy.expand()?;
                    if !sources.insert(&policy.source_prefix) {
                        return Err("duplicate mirror source".into());
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MirrorSessionPhase {
    Empty,
    Active,
    Superseded,
    Closing,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MirrorControlReply {
    Session {
        session: MirrorSessionId,
        owner: String,
        revision: u64,
        phase: MirrorSessionPhase,
        policies: Vec<MirrorPolicySpec>,
    },
    Retired {
        session: MirrorSessionId,
    },
    Candidate {
        session: MirrorSessionId,
    },
}
impl MirrorControlReply {
    pub fn session(&self) -> MirrorSessionId {
        match self {
            Self::Session { session, .. }
            | Self::Retired { session }
            | Self::Candidate { session } => *session,
        }
    }
}
struct Session {
    owner: String,
    revision: u64,
    policies: Vec<MirrorPolicySpec>,
    group: Option<Arc<WireMirrorGroup>>,
    // Retain every replaced group until it has really retired; Close must also
    // join old workers/encoders, not merely the latest registry generation.
    retiring: Vec<Arc<WireMirrorGroup>>,
    closing: bool,
}
struct State {
    open: bool,
    issued: u64,
    sessions: BTreeMap<u64, Session>,
}
pub(crate) struct MirrorControlStore {
    incarnation: [u8; 16],
    state: Mutex<State>,
}
impl MirrorControlStore {
    pub(crate) fn new() -> Self {
        Self {
            incarnation: *uuid::Uuid::new_v4().as_bytes(),
            state: Mutex::new(State {
                open: true,
                issued: 0,
                sessions: BTreeMap::new(),
            }),
        }
    }
    fn snapshot(&self, sequence: u64, value: &Session) -> MirrorControlReply {
        let phase = if value.closing {
            MirrorSessionPhase::Closing
        } else if let Some(group) = &value.group {
            if group.is_current() {
                MirrorSessionPhase::Active
            } else {
                MirrorSessionPhase::Superseded
            }
        } else {
            MirrorSessionPhase::Empty
        };
        MirrorControlReply::Session {
            session: MirrorSessionId {
                incarnation: self.incarnation,
                sequence,
            },
            owner: value.owner.clone(),
            revision: value.revision,
            phase,
            policies: value.policies.clone(),
        }
    }
    fn check(&self, state: &State, session: MirrorSessionId) -> Result<(), String> {
        if session.incarnation != self.incarnation
            || session.sequence == 0
            || session.sequence > state.issued
        {
            Err("unknown mirror session incarnation or sequence".into())
        } else {
            Ok(())
        }
    }
    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.open = false;
        for session in state.sessions.values_mut() {
            session.closing = true;
            for group in session.group.iter().chain(&session.retiring) {
                group.cancel();
            }
        }
    }
    pub(crate) async fn execute(
        &self,
        registry: &WireMirrorRegistry,
        command: MirrorControl,
    ) -> Result<MirrorControlReply, String> {
        command.validate()?;
        let (id, groups) = {
            let mut state = self.state.lock().unwrap();
            if !state.open {
                return Err("mirror control is shut down".into());
            }
            match command {
                MirrorControl::Candidate => {
                    return Ok(MirrorControlReply::Candidate {
                        session: MirrorSessionId {
                            incarnation: self.incarnation,
                            sequence: state
                                .issued
                                .checked_add(1)
                                .ok_or("mirror session sequence exhausted")?,
                        },
                    });
                }
                MirrorControl::Allocate { owner, session } => {
                    if session.incarnation != self.incarnation || session.sequence == 0 {
                        return Err("unknown mirror allocation incarnation or sequence".into());
                    }
                    if let Some(value) = state.sessions.get(&session.sequence) {
                        if value.owner != owner {
                            return Err("mirror allocation slot belongs to another owner".into());
                        }
                        return Ok(self.snapshot(session.sequence, value));
                    }
                    if state.issued.checked_add(1) != Some(session.sequence) {
                        return Err("mirror allocation slot is retired or not next".into());
                    }
                    if state.sessions.len() >= 64 {
                        return Err("at most 64 live mirror sessions are allowed".into());
                    }
                    let value = Session {
                        owner,
                        revision: 0,
                        policies: vec![],
                        group: None,
                        retiring: vec![],
                        closing: false,
                    };
                    let reply = self.snapshot(session.sequence, &value);
                    state.sessions.insert(session.sequence, value);
                    state.issued = session.sequence;
                    return Ok(reply);
                }
                MirrorControl::Open { owner } => {
                    if let Some((&sequence, session)) = state
                        .sessions
                        .iter()
                        .find(|(_, session)| session.owner == owner)
                    {
                        return Ok(self.snapshot(sequence, session));
                    }
                    if state.sessions.len() >= 64 {
                        return Err("at most 64 live mirror sessions are allowed".into());
                    }
                    let sequence = state
                        .issued
                        .checked_add(1)
                        .ok_or("mirror session sequence exhausted")?;
                    let value = Session {
                        owner,
                        revision: 0,
                        policies: vec![],
                        group: None,
                        retiring: vec![],
                        closing: false,
                    };
                    let reply = self.snapshot(sequence, &value);
                    state.sessions.insert(sequence, value);
                    state.issued = sequence;
                    return Ok(reply);
                }
                MirrorControl::Status { session } => {
                    self.check(&state, session)?;
                    return Ok(state
                        .sessions
                        .get(&session.sequence)
                        .map(|value| self.snapshot(session.sequence, value))
                        .unwrap_or(MirrorControlReply::Retired { session }));
                }
                MirrorControl::Apply {
                    session,
                    revision,
                    policies,
                } => {
                    self.check(&state, session)?;
                    let value = state
                        .sessions
                        .get_mut(&session.sequence)
                        .ok_or("mirror session is retired")?;
                    if value.closing {
                        return Err("mirror session is closing".into());
                    }
                    if revision == value.revision {
                        if policies != value.policies
                            || !value.group.as_ref().is_some_and(|group| group.is_current())
                        {
                            return Err("mirror revision conflict or superseded policy".into());
                        }
                        return Ok(self.snapshot(session.sequence, value));
                    }
                    if value.revision.checked_add(1) != Some(revision) {
                        return Err("mirror revision must advance exactly once".into());
                    }
                    value.retiring.retain(|group| !group.is_finished());
                    let expanded = policies
                        .iter()
                        .map(MirrorPolicySpec::expand)
                        .collect::<Result<Vec<_>, _>>()?;
                    let group = registry
                        .replace_group(value.group.as_deref(), expanded)
                        .map_err(|e| e.to_string())?;
                    if let Some(old) = value.group.replace(Arc::new(group)) {
                        value.retiring.push(old);
                    }
                    value.revision = revision;
                    value.policies = policies;
                    return Ok(self.snapshot(session.sequence, value));
                }
                MirrorControl::Close { session } => {
                    self.check(&state, session)?;
                    let Some(value) = state.sessions.get_mut(&session.sequence) else {
                        return Ok(MirrorControlReply::Retired { session });
                    };
                    value.closing = true;
                    let groups: Vec<_> =
                        value.group.iter().chain(&value.retiring).cloned().collect();
                    for group in &groups {
                        group.cancel();
                    }
                    (session, groups)
                }
            }
        };
        // A cancelled requester leaves Closing ownership in the store. A retry
        // joins the same groups. No mutex is held while awaiting user cleanup.
        tokio::time::timeout(Duration::from_secs(10), async {
            for group in &groups {
                group.close().await;
            }
        })
        .await
        .map_err(|_| "mirror retirement timed out; retry close".to_string())?;
        let removed = self.state.lock().unwrap().sessions.remove(&id.sequence);
        drop(removed);
        Ok(MirrorControlReply::Retired { session: id })
    }
}
impl Drop for MirrorControlStore {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingress::LocalLookup;
    use parrot_api::types::BoxedMessage;
    #[test]
    fn allocation_requires_its_own_negotiated_capability_and_exact_receipt() {
        let session = MirrorSessionId {
            incarnation: [1; 16],
            sequence: 1,
        };
        let command = MirrorControl::Allocate {
            owner: "app".into(),
            session,
        };
        assert_ne!(
            command.required_capabilities(),
            crate::handshake::caps::MIRROR_CONTROL
        );
        assert_eq!(
            MirrorControl::Candidate.required_capabilities(),
            command.required_capabilities()
        );
        let reply = MirrorControlReply::Session {
            session,
            owner: "foreign".into(),
            revision: 0,
            phase: MirrorSessionPhase::Empty,
            policies: vec![],
        };
        assert!(!command.accepts(&reply));
        assert!(
            !MirrorControl::Status { session }.accepts(&MirrorControlReply::Candidate { session })
        );
        assert!(!MirrorControl::Close { session }.accepts(&reply));
        assert!(!MirrorControl::Candidate.accepts(&MirrorControlReply::Retired { session }));
    }
    #[tokio::test]
    async fn candidate_reads_consume_no_capacity_and_allocation_is_exactly_fenced() {
        let local: Arc<dyn LocalLookup> = Arc::new(Empty);
        let registry = WireMirrorRegistry::new(&local);
        let store = MirrorControlStore::new();
        let candidate = store
            .execute(&registry, MirrorControl::Candidate)
            .await
            .unwrap();
        for _ in 0..80 {
            assert_eq!(
                store
                    .execute(&registry, MirrorControl::Candidate)
                    .await
                    .unwrap(),
                candidate
            );
        }
        let session = candidate.session();
        let mut invalid = session;
        invalid.sequence += 1;
        assert!(store
            .execute(
                &registry,
                MirrorControl::Allocate {
                    owner: "app".into(),
                    session: invalid
                }
            )
            .await
            .is_err());
        invalid = session;
        invalid.incarnation[0] ^= 1;
        assert!(store
            .execute(
                &registry,
                MirrorControl::Allocate {
                    owner: "app".into(),
                    session: invalid
                }
            )
            .await
            .is_err());
        let allocate = MirrorControl::Allocate {
            owner: "app".into(),
            session,
        };
        store.execute(&registry, allocate.clone()).await.unwrap();
        assert!(store
            .execute(
                &registry,
                MirrorControl::Allocate {
                    owner: "foreign".into(),
                    session
                }
            )
            .await
            .is_err());
        store
            .execute(&registry, MirrorControl::Close { session })
            .await
            .unwrap();
        assert!(store.execute(&registry, allocate).await.is_err());
        assert_eq!(store.state.lock().unwrap().issued, 1);
        assert!(store.state.lock().unwrap().sessions.is_empty());
        registry.close();
        registry.wait_closed().await;
    }
    struct Empty;
    #[async_trait::async_trait]
    impl LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
            None
        }
    }
    struct Gate {
        entered: tokio::sync::Notify,
        release: Mutex<bool>,
        cv: std::sync::Condvar,
    }
    struct EncodeGate(Arc<Gate>);
    parrot_api::message::inventory::submit! {
        parrot_api::message::CodecRegistration {
            type_key: "bin:mirror-control-gated-encoder#v1", type_id: std::any::TypeId::of::<EncodeGate>(),
            encode: |message| {
                let gate=&message.downcast_ref::<EncodeGate>().unwrap().0;
                gate.entered.notify_one();
                let (release,expired)=gate.cv.wait_timeout_while(gate.release.lock().unwrap(),Duration::from_secs(5),|ready| !*ready).unwrap();
                assert!(*release && !expired.timed_out(),"encoder wall-clock watchdog");
                Ok(vec![1])
            },
            decode: |_| Ok(Box::new(()) as BoxedMessage),
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_close_joins_retired_generation_encoders_before_receipt() {
        let local: Arc<dyn LocalLookup> = Arc::new(Empty);
        let registry = Arc::new(WireMirrorRegistry::new(&local));
        let store = Arc::new(MirrorControlStore::new());
        let session = store
            .execute(
                &registry,
                MirrorControl::Open {
                    owner: "app".into(),
                },
            )
            .await
            .unwrap()
            .session();
        store
            .execute(
                &registry,
                MirrorControl::Apply {
                    session,
                    revision: 1,
                    policies: vec![MirrorPolicySpec::new("/source", "/shadow")],
                },
            )
            .await
            .unwrap();
        let handle = store.state.lock().unwrap().sessions[&session.sequence]
            .group
            .as_ref()
            .unwrap()
            .handle_for("/source")
            .unwrap();
        let gate = Arc::new(Gate {
            entered: Default::default(),
            release: Mutex::new(false),
            cv: Default::default(),
        });
        let message: BoxedMessage = Box::new(EncodeGate(gate.clone()));
        let encoder = std::thread::spawn(move || handle.offer_local("/source", &message));
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        store
            .execute(
                &registry,
                MirrorControl::Apply {
                    session,
                    revision: 2,
                    policies: vec![MirrorPolicySpec::new("/source", "/next")],
                },
            )
            .await
            .unwrap();
        // Retire the current group first. Otherwise a pending close may merely
        // be waiting for its worker, even if ownership of the blocked encoder's
        // replaced group was accidentally discarded.
        let current = store.state.lock().unwrap().sessions[&session.sequence]
            .group
            .clone()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), current.close())
            .await
            .unwrap();
        {
            let close = store.execute(&registry, MirrorControl::Close { session });
            tokio::pin!(close);
            assert!(
                futures::poll!(&mut close).is_pending(),
                "receipt must wait for the replaced generation's encoder"
            );
            // Dropping the polled waiter must leave ownership in the store.
        }
        assert!(
            matches!(store.execute(&registry,MirrorControl::Open{owner:"app".into()}).await.unwrap(),MirrorControlReply::Session{session:id,phase:MirrorSessionPhase::Closing,..} if id==session)
        );
        assert!(store
            .execute(
                &registry,
                MirrorControl::Apply {
                    session,
                    revision: 3,
                    policies: vec![MirrorPolicySpec::new("/source", "/next")]
                }
            )
            .await
            .is_err());
        *gate.release.lock().unwrap() = true;
        gate.cv.notify_all();
        tokio::task::spawn_blocking(move || encoder.join().unwrap())
            .await
            .unwrap();
        assert_eq!(
            store
                .execute(&registry, MirrorControl::Close { session })
                .await
                .unwrap(),
            MirrorControlReply::Retired { session }
        );
        registry.close();
        registry.wait_closed().await;
    }
    #[tokio::test]
    async fn exhausted_worker_budget_keeps_previous_revision_and_group() {
        let local: Arc<dyn LocalLookup> = Arc::new(Empty);
        let registry = WireMirrorRegistry::new(&local);
        let store = MirrorControlStore::new();
        let session = store
            .execute(
                &registry,
                MirrorControl::Open {
                    owner: "app".into(),
                },
            )
            .await
            .unwrap()
            .session();
        let command = MirrorControl::Apply {
            session,
            revision: 1,
            policies: vec![MirrorPolicySpec::new("/source", "/shadow")],
        };
        let before = store.execute(&registry, command.clone()).await.unwrap();
        let handle = store.state.lock().unwrap().sessions[&session.sequence]
            .group
            .as_ref()
            .unwrap()
            .handle_for("/source")
            .unwrap();
        for index in 0..63 {
            registry
                .register(
                    WireMirrorPolicy::new(format!("/foreign-{index}"), "/other"),
                    Default::default(),
                )
                .unwrap();
        }
        assert!(store
            .execute(
                &registry,
                MirrorControl::Apply {
                    session,
                    revision: 2,
                    policies: vec![MirrorPolicySpec::new("/source", "/next")]
                }
            )
            .await
            .is_err());
        assert_eq!(store.execute(&registry, command).await.unwrap(), before);
        // Equal metadata is insufficient: cancellation can fence admission
        // before the worker publishes its terminal state.
        registry.offer("/source", "bin:budget-preserved#v1", &[1]);
        assert_eq!(
            handle.stats().enqueued,
            1,
            "failed replacement must leave the previous policy accepting work"
        );
        store.close();
        registry.close();
        registry.wait_closed().await;
    }
}
