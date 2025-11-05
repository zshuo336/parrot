//! Durable Mirror orchestration for local or direct remote Rust shadows.
//! The Unix owner holds FileStateStore's exclusive lock. Every mutating request
//! is journaled first; losing a waiter never discards uncertain responsibility.
//! This is local cooperative ownership, not distributed leader election.
use super::{AppSupervisor, ObservedState, ReconcileAction, ReconcileReport};
use crate::{AppManifest, ApplicationLimits, EngineKind};
use parrot_remote::mirror_control::{
    MirrorControl, MirrorControlReply, MirrorPolicySpec, MirrorSessionId, MirrorSessionPhase,
};
use parrot_remote::RemoteActorSystem;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MirrorIntentPhase {
    Allocating,
    Applying,
    Applied,
    Closing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorIntent {
    pub owner: String,
    pub session: MirrorSessionId,
    /// Hash of the entire desired manifest, not a transport callback ID.
    pub desired: String,
    pub revision: u64,
    pub policies: Vec<MirrorPolicySpec>,
    pub phase: MirrorIntentPhase,
}
pub(super) fn validate_journal(
    entries: &BTreeMap<String, MirrorIntent>,
    limits: &ApplicationLimits,
) -> Result<(), String> {
    if entries.len() > 64 {
        return Err("at most 64 mirror node responsibilities".into());
    }
    limits.check_metadata(entries).map_err(|e| e.to_string())?;
    for (node, entry) in entries {
        limits.check_metadata(node).map_err(|e| e.to_string())?;
        if node.is_empty()
            || node.chars().any(char::is_control)
            || entry.session.sequence == 0
            || entry.revision != 1
            || entry.desired.len() != 64
            || !entry.desired.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid mirror journal identity or revision".into());
        }
        MirrorControl::Allocate {
            owner: entry.owner.clone(),
            session: entry.session,
        }
        .validate()?;
        MirrorControl::Apply {
            session: entry.session,
            revision: entry.revision,
            policies: entry.policies.clone(),
        }
        .validate()?;
    }
    Ok(())
}
fn desired_key(manifest: &AppManifest) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(manifest.to_toml().map_err(|e| e.to_string())?.as_bytes())
    ))
}
fn project(
    manifest: &AppManifest,
    observed: &ObservedState,
) -> Result<BTreeMap<String, Vec<MirrorPolicySpec>>, String> {
    let mut nodes = BTreeMap::<String, Vec<MirrorPolicySpec>>::new();
    for mirror in &manifest.mirrors {
        if !observed.links_known {
            return Err("mirror placement needs authenticated live-node observations".into());
        }
        let source = manifest.component(&mirror.source).ok_or("unknown source")?;
        let shadow = manifest.component(&mirror.shadow).ok_or("unknown shadow")?;
        if source.engine != EngineKind::Parrot || shadow.engine != EngineKind::Parrot {
            return Err(
                "remote mirror control currently requires Rust source and shadow nodes".into(),
            );
        }
        if !AppSupervisor::healthy_observation(source, &manifest.version, observed)
            || !AppSupervisor::healthy_observation(shadow, &manifest.version, observed)
        {
            return Err("mirror source or shadow lacks current healthy provenance".into());
        }
        let node = observed
            .component_nodes
            .get(&source.name)
            .ok_or("missing source node")?;
        let shadow_node = observed
            .component_nodes
            .get(&shadow.name)
            .ok_or("missing shadow node")?;
        let sources = source.instance_paths().map_err(|e| e.to_string())?;
        let shadows = shadow.instance_paths().map_err(|e| e.to_string())?;
        for (index, source_prefix) in sources.into_iter().enumerate() {
            let spec = MirrorPolicySpec {
                source_prefix,
                shadow_path: {
                    let path = &shadows[if shadows.len() == 1 { 0 } else { index }];
                    if shadow_node == node {
                        path.clone()
                    } else {
                        format!("parrot://{shadow_node}{path}")
                    }
                },
                capacity: mirror
                    .capacity
                    .try_into()
                    .map_err(|_| "mirror capacity exceeds wire range")?,
                max_message_bytes: mirror
                    .max_message_bytes
                    .try_into()
                    .map_err(|_| "mirror message budget exceeds wire range")?,
                max_pending_bytes: mirror
                    .max_pending_bytes
                    .try_into()
                    .map_err(|_| "mirror pending budget exceeds wire range")?,
                delivery_timeout_ms: mirror
                    .delivery_timeout_ms
                    .try_into()
                    .map_err(|_| "mirror deadline exceeds wire range")?,
            };
            nodes.entry(node.clone()).or_default().push(spec);
        }
    }
    Ok(nodes)
}

/// Owns the local durable store and executes real remote control requests. Drop
/// does not imply remote retirement: reopen the same directory to resume. Hosts
/// must keep node identities authenticated and drive health invalidation from
/// real link/deployment generation events. No internal polling timer is created.
#[cfg(unix)]
pub struct MirrorSupervisor {
    inner: AppSupervisor,
}
#[cfg(unix)]
impl MirrorSupervisor {
    pub fn open(
        directory: impl AsRef<std::path::Path>,
        manifest: AppManifest,
        clock: Arc<dyn super::Clock>,
        limits: ApplicationLimits,
    ) -> Result<Self, String> {
        let store = Arc::new(super::FileStateStore::open(directory, limits)?);
        Ok(Self {
            inner: AppSupervisor::with_executor(manifest, store, clock, limits, true)?,
        })
    }
    pub fn desired(&self) -> &AppManifest {
        self.inner.desired()
    }
    pub fn revision(&self) -> u64 {
        self.inner.revision()
    }
    pub fn responsibilities(&self) -> &BTreeMap<String, MirrorIntent> {
        &self.inner.state.mirrors
    }
    pub fn submit(&mut self, manifest: AppManifest) -> Result<(), String> {
        self.inner.submit(manifest)
    }
    pub fn upgrade_generation(&self, component: &str) -> Option<u64> {
        self.inner.upgrade_generation(component)
    }
    pub fn finish_upgrade(&mut self, component: &str, generation: u64) -> Result<bool, String> {
        self.inner.finish_upgrade(component, generation)
    }
    fn save(&mut self, node: &str, entry: Option<MirrorIntent>) -> Result<(), String> {
        let mut journal = self.inner.state.mirrors.clone();
        if let Some(entry) = entry {
            journal.insert(node.into(), entry);
        } else {
            journal.remove(node);
        }
        self.inner.commit_mirrors(journal)
    }
    fn wait(&self, reason: String) -> ReconcileReport {
        ReconcileReport {
            actions: vec![ReconcileAction::Wait {
                comp: self.desired().name.clone(),
                reason,
            }],
            converged: false,
        }
    }
    /// Execute bounded progress. Component changes are withheld until obsolete
    /// mirror sessions have fully retired. Convergence requires a fresh remote
    /// Active receipt with exact owner, session, revision and policy contents.
    pub async fn reconcile_once(
        &mut self,
        observed: &ObservedState,
        remote: &Arc<RemoteActorSystem>,
    ) -> Result<ReconcileReport, String> {
        self.inner.require_current()?;
        let components = self.inner.reconcile_components(observed);
        let desired = desired_key(self.desired())?;
        let projection = if components.converged {
            project(self.desired(), observed)
        } else {
            Ok(BTreeMap::new())
        };
        let wanted = projection.as_ref().cloned().unwrap_or_default();
        let mut ready = true;
        let existing: Vec<_> = self.inner.state.mirrors.keys().cloned().collect();
        for node in existing {
            ready &= self
                .progress(&node, wanted.get(&node), &desired, remote)
                .await?;
        }
        // Finish all old responsibility before opening a new placement. A lost
        // source link is not proof that its old mirror workers have stopped.
        if self.inner.state.mirrors.iter().any(|(node, entry)| {
            entry.desired != desired
                || wanted.get(node) != Some(&entry.policies)
                || entry.phase == MirrorIntentPhase::Closing
        }) {
            return Ok(self.wait("waiting for mirror retirement receipt".into()));
        }
        if !components.converged {
            return Ok(components);
        }
        if let Err(reason) = projection {
            return Ok(self.wait(reason));
        }
        for (node, policies) in &wanted {
            if self.inner.state.mirrors.contains_key(node) {
                continue;
            }
            let reply = remote
                .control_mirror(node, MirrorControl::Candidate)
                .await
                .map_err(|e| e.to_string())?;
            let MirrorControlReply::Candidate { session } = reply else {
                return Err("invalid allocation candidate".into());
            };
            let owner = format!(
                "parrot-app:{:x}",
                Sha256::digest(self.desired().name.as_bytes())
            );
            // This commit precedes the first request capable of creating a session.
            self.save(
                node,
                Some(MirrorIntent {
                    owner,
                    session,
                    desired: desired.clone(),
                    revision: 1,
                    policies: policies.clone(),
                    phase: MirrorIntentPhase::Allocating,
                }),
            )?;
            ready = false;
        }
        if ready && self.inner.state.mirrors.len() == wanted.len() {
            Ok(components)
        } else {
            Ok(self.wait("waiting for current mirror policy receipt".into()))
        }
    }
    async fn progress(
        &mut self,
        node: &str,
        wanted: Option<&Vec<MirrorPolicySpec>>,
        desired: &str,
        remote: &Arc<RemoteActorSystem>,
    ) -> Result<bool, String> {
        let mut entry = self.inner.state.mirrors[node].clone();
        let current = entry.desired == desired && wanted == Some(&entry.policies);
        if entry.phase == MirrorIntentPhase::Allocating {
            let result = remote
                .control_mirror(
                    node,
                    MirrorControl::Allocate {
                        owner: entry.owner.clone(),
                        session: entry.session,
                    },
                )
                .await;
            let reply = match result {
                Ok(reply) => reply,
                Err(error) => {
                    // A competing allocator may have consumed this exact slot.
                    // Only a terminal proof for this slot permits forgetting it.
                    match remote
                        .control_mirror(
                            node,
                            MirrorControl::Status {
                                session: entry.session,
                            },
                        )
                        .await
                    {
                        Ok(MirrorControlReply::Retired { .. }) => {
                            self.save(node, None)?;
                            return Ok(false);
                        }
                        Ok(MirrorControlReply::Session { owner, .. }) if owner != entry.owner => {
                            self.save(node, None)?;
                            return Ok(false);
                        }
                        _ => return Err(error.to_string()),
                    }
                }
            };
            match reply {
                MirrorControlReply::Session {
                    session,
                    owner,
                    revision: 0,
                    phase: MirrorSessionPhase::Empty,
                    policies,
                } if session == entry.session && owner == entry.owner && policies.is_empty() => {
                    entry.phase = if current {
                        MirrorIntentPhase::Applying
                    } else {
                        MirrorIntentPhase::Closing
                    };
                    self.save(node, Some(entry))?;
                    return Ok(false);
                }
                // Persistence failure after an allocation reply can only leave
                // an empty allocation; arbitrary pre-existing state is not ours.
                _ => return Err("allocation receipt does not match empty owned session".into()),
            }
        }
        if !current && entry.phase != MirrorIntentPhase::Closing {
            entry.phase = MirrorIntentPhase::Closing;
            self.save(node, Some(entry.clone()))?;
        }
        if entry.phase == MirrorIntentPhase::Closing {
            let reply = remote
                .control_mirror(
                    node,
                    MirrorControl::Close {
                        session: entry.session,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            if reply
                != (MirrorControlReply::Retired {
                    session: entry.session,
                })
            {
                return Err("invalid mirror retirement receipt".into());
            }
            self.save(node, None)?;
            return Ok(false);
        }
        if entry.phase == MirrorIntentPhase::Applying {
            let reply = remote
                .control_mirror(
                    node,
                    MirrorControl::Apply {
                        session: entry.session,
                        revision: entry.revision,
                        policies: entry.policies.clone(),
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            if !matches_active(&entry, &reply) {
                return Err("invalid mirror apply receipt".into());
            }
            entry.phase = MirrorIntentPhase::Applied;
            self.save(node, Some(entry))?;
            return Ok(false);
        }
        let reply = remote
            .control_mirror(
                node,
                MirrorControl::Status {
                    session: entry.session,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        if matches_active(&entry, &reply) {
            return Ok(current);
        }
        // Retired/superseded observations never prove convergence. Persist
        // cleanup before returning, including external policy interference.
        entry.phase = MirrorIntentPhase::Closing;
        self.save(node, Some(entry))?;
        Ok(false)
    }
}
fn matches_active(entry: &MirrorIntent, reply: &MirrorControlReply) -> bool {
    matches!(reply, MirrorControlReply::Session { session, owner, revision, phase: MirrorSessionPhase::Active, policies }
        if *session==entry.session && owner==&entry.owner && *revision==entry.revision && policies==&entry.policies)
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::{DesiredStateRecord, DesiredStateStore, MemStateStore, SystemClock};
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    struct FaultStore {
        memory: MemStateStore,
        fail: AtomicBool,
    }
    impl DesiredStateStore for FaultStore {
        fn latest(&self) -> Result<Option<DesiredStateRecord>, String> {
            self.memory.latest()
        }
        fn commit(&self, expected: Option<u64>, state: &DesiredStateRecord) -> Result<(), String> {
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err("injected receipt persistence failure".into());
            }
            self.memory.commit(expected, state)
        }
    }
    struct Empty;
    #[async_trait::async_trait]
    impl parrot_remote::LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
            None
        }
    }
    fn manifest() -> AppManifest {
        AppManifest::from_toml("name='receipt-test'\nversion='1.0.0'\n[[components]]\nname='source'\nengine='parrot'\nartifact={Props={factory='test'}}\n[[components]]\nname='shadow'\nengine='parrot'\nartifact={Props={factory='test'}}\n[[mirrors]]\nsource='source'\nshadow='shadow'\n").unwrap()
    }
    fn controller(store: Arc<FaultStore>) -> MirrorSupervisor {
        MirrorSupervisor {
            inner: AppSupervisor::with_executor(
                manifest(),
                store,
                Arc::new(SystemClock),
                ApplicationLimits::default(),
                true,
            )
            .unwrap(),
        }
    }
    async fn receipt_failure(phase: MirrorIntentPhase) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let node = RemoteActorSystem::new(
            parrot_remote::RemoteConfig::mem(format!("receipt-node-{id}")),
            Arc::new(Empty),
        )
        .unwrap();
        let client = RemoteActorSystem::new(
            parrot_remote::RemoteConfig::mem(format!("receipt-client-{id}")),
            Arc::new(Empty),
        )
        .unwrap();
        node.start().await.unwrap();
        client.start().await.unwrap();
        client
            .connect(&parrot_remote::NodeAddr::mem(&node.config.node_id))
            .await
            .unwrap();
        let store = Arc::new(FaultStore {
            memory: MemStateStore::default(),
            fail: AtomicBool::new(false),
        });
        let mut c = controller(store.clone());
        let key = desired_key(c.desired()).unwrap();
        let session = client
            .control_mirror(&node.config.node_id, MirrorControl::Candidate)
            .await
            .unwrap()
            .session();
        let policies = vec![MirrorPolicySpec::new("/user/source", "/user/shadow")];
        let entry = MirrorIntent {
            owner: "receipt-owner".into(),
            session,
            desired: key.clone(),
            revision: 1,
            policies: policies.clone(),
            phase: MirrorIntentPhase::Allocating,
        };
        c.save(&node.config.node_id, Some(entry)).unwrap();
        if phase != MirrorIntentPhase::Allocating {
            c.progress(&node.config.node_id, Some(&policies), &key, &client)
                .await
                .unwrap();
        }
        if phase == MirrorIntentPhase::Closing {
            c.progress(&node.config.node_id, Some(&policies), &key, &client)
                .await
                .unwrap();
            let mut entry = c.responsibilities()[&node.config.node_id].clone();
            entry.phase = MirrorIntentPhase::Closing;
            c.save(&node.config.node_id, Some(entry)).unwrap();
        }
        store.fail.store(true, Ordering::SeqCst);
        assert!(c
            .progress(&node.config.node_id, Some(&policies), &key, &client)
            .await
            .is_err());
        let reply = client
            .control_mirror(&node.config.node_id, MirrorControl::Status { session })
            .await
            .unwrap();
        match phase {
            MirrorIntentPhase::Allocating => assert!(matches!(
                reply,
                MirrorControlReply::Session {
                    phase: MirrorSessionPhase::Empty,
                    ..
                }
            )),
            MirrorIntentPhase::Applying => assert!(matches!(
                reply,
                MirrorControlReply::Session {
                    phase: MirrorSessionPhase::Active,
                    ..
                }
            )),
            MirrorIntentPhase::Closing => {
                assert_eq!(reply, MirrorControlReply::Retired { session })
            }
            _ => unreachable!(),
        }
        // The old durable phase still owns the side effect even though its
        // acknowledgement failed to commit. Recovery retries the same identity.
        assert_eq!(
            store.latest().unwrap().unwrap().mirrors[&node.config.node_id].phase,
            phase
        );
        drop(c);
        let mut c = controller(store.clone());
        c.progress(&node.config.node_id, Some(&policies), &key, &client)
            .await
            .unwrap();
        if phase == MirrorIntentPhase::Closing {
            assert!(c.responsibilities().is_empty());
        } else {
            assert_ne!(c.responsibilities()[&node.config.node_id].phase, phase);
        }
        client.shutdown().await.unwrap();
        node.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn allocation_receipt_storage_failure_keeps_original_slot() {
        receipt_failure(MirrorIntentPhase::Allocating).await;
    }
    #[tokio::test]
    async fn application_receipt_storage_failure_keeps_original_revision() {
        receipt_failure(MirrorIntentPhase::Applying).await;
    }
    #[tokio::test]
    async fn retirement_receipt_storage_failure_keeps_cleanup_responsibility() {
        receipt_failure(MirrorIntentPhase::Closing).await;
    }
    #[test]
    fn journal_resource_and_identity_validation_precedes_persistence() {
        let mut entry = MirrorIntent {
            owner: "owner".into(),
            session: MirrorSessionId {
                incarnation: [1; 16],
                sequence: 1,
            },
            desired: "a".repeat(64),
            revision: 1,
            policies: vec![MirrorPolicySpec::new("/source", "/shadow")],
            phase: MirrorIntentPhase::Applied,
        };
        let limits = ApplicationLimits::default();
        assert!(
            validate_journal(&BTreeMap::from([("node".into(), entry.clone())]), &limits).is_ok()
        );
        let tiny = ApplicationLimits {
            max_metadata_bytes: 64,
            ..limits
        };
        assert!(
            validate_journal(&BTreeMap::from([("node".into(), entry.clone())]), &tiny).is_err()
        );
        entry.session.sequence = 0;
        assert!(validate_journal(&BTreeMap::from([("node".into(), entry)]), &limits).is_err());
    }
}
