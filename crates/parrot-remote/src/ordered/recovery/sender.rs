// SPDX-License-Identifier: AGPL-3.0-only
use super::{error, regular, Access, Identity, ReceiverOwner};
use parrot_api::{errors::ActorError, types::ActorResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

const MAGIC: &[u8; 8] = b"POSND01\0";
const LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ticket {
    pub source: String,
    pub incarnation: Identity,
    pub stream: Identity,
    pub receiver: Identity,
    pub epoch: Option<u64>,
    pub ticket: Identity,
    pub actor: bool,
    pub destination: String,
    pub path: String,
}

impl Ticket {
    fn same_ticket(&self, other: &Self) -> bool {
        self.receiver == other.receiver && self.ticket == other.ticket
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    namespace: Identity,
    tickets: Vec<Ticket>,
}

pub(crate) struct Journal {
    owner: Mutex<Option<Arc<ReceiverOwner>>>,
    capacity: usize,
    // A write retains its guard and owner even if its awaiting caller is cancelled.
    state: Arc<tokio::sync::Mutex<Snapshot>>,
    pending: tokio::sync::watch::Sender<Vec<Ticket>>,
    uncertain: Arc<AtomicBool>,
}

impl Journal {
    pub fn open(
        owner: &Arc<ReceiverOwner>,
        enabled: bool,
        capacity: usize,
    ) -> Result<Option<Self>, String> {
        let path = owner.directory.join("sender.state");
        let saved = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Snapshot {
                namespace: owner.namespace,
                tickets: Vec::new(),
            },
            Err(e) => return Err(error(e)),
            Ok(_) => {
                let mut bytes = Vec::new();
                regular(&path, Access::Read)?
                    .take((LIMIT + 41) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(error)?;
                if bytes.len() < 40
                    || bytes.len() > LIMIT + 40
                    || &bytes[..8] != MAGIC
                    || Sha256::digest(&bytes[40..])[..] != bytes[8..40]
                {
                    return Err("corrupt ordered sender state".into());
                }
                serde_json::from_slice(&bytes[40..]).map_err(error)?
            }
        };
        if saved.namespace != owner.namespace || saved.tickets.len() > capacity {
            return Err("ordered sender state binding or capacity differs".into());
        }
        if !enabled {
            if saved.tickets.is_empty() {
                return Ok(None);
            }
            return Err("outstanding ordered tickets require persist_sender_sessions".into());
        }
        Ok(Some(Self {
            owner: Mutex::new(Some(owner.clone())),
            capacity,
            pending: tokio::sync::watch::channel(saved.tickets.clone()).0,
            uncertain: Default::default(),
            state: Arc::new(tokio::sync::Mutex::new(saved)),
        }))
    }

    pub fn pending(&self) -> Vec<Ticket> {
        self.pending.borrow().clone()
    }

    pub fn close(&self) {
        self.owner.lock().unwrap().take();
    }

    // Actor incarnations change at restart. A previous node-scoped ticket may
    // also have been shared by actors when a peer negotiated an older protocol.
    pub async fn wait_for_source(
        &self,
        source: &str,
        deadline: tokio::time::Instant,
    ) -> ActorResult<()> {
        let mut pending = self.pending.subscribe();
        tokio::time::timeout_at(
            deadline,
            pending.wait_for(|tickets| {
                !tickets
                    .iter()
                    .any(|t| t.source == source || !t.source.contains('@') || !source.contains('@'))
            }),
        )
        .await
        .map_err(|_| ActorError::Timeout)?
        .map_err(|_| ActorError::Stopped)?;
        Ok(())
    }

    pub async fn record(&self, ticket: Ticket, remove: bool) -> ActorResult<()> {
        let owner = self
            .owner
            .lock()
            .unwrap()
            .clone()
            .filter(|o| o.valid_process())
            .ok_or(ActorError::Stopped)?;
        let mut state = self.state.clone().lock_owned().await;
        if self.uncertain.load(Ordering::Acquire) {
            return Err(ActorError::InternalError(
                "ordered sender directory sync failed; reopen state".into(),
            ));
        }
        let pos = state.tickets.iter().position(|t| t.same_ticket(&ticket));
        if (!remove && pos.is_some_and(|i| state.tickets[i] == ticket)) || (remove && pos.is_none())
        {
            return Ok(());
        }
        let mut tickets = state.tickets.clone();
        match pos {
            Some(i) if remove => {
                tickets.swap_remove(i);
            }
            Some(i) => tickets[i] = ticket.clone(),
            None if tickets.len() < self.capacity => tickets.push(ticket.clone()),
            None => {
                return Err(crate::error::ErrCode::Overloaded
                    .to_actor_error("ordered sender journal at capacity".into()))
            }
        }
        let pending = self.pending.clone();
        let uncertain = self.uncertain.clone();
        tokio::task::spawn_blocking(move || {
            let snapshot = Snapshot {
                namespace: owner.namespace,
                tickets,
            };
            let bytes = serde_json::to_vec(&snapshot).map_err(error)?;
            if bytes.len() > LIMIT {
                return Err("ordered sender state exceeds byte limit".to_owned());
            }
            let next = owner.directory.join("sender.next");
            match std::fs::symlink_metadata(&next) {
                Ok(_) => {
                    drop(regular(&next, Access::Read)?);
                    std::fs::remove_file(&next).map_err(error)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(error(e)),
            }
            let mut file = regular(&next, Access::New)?;
            file.write_all(MAGIC).map_err(error)?;
            file.write_all(&Sha256::digest(&bytes)).map_err(error)?;
            file.write_all(&bytes).map_err(error)?;
            file.sync_all().map_err(error)?;
            std::fs::rename(next, owner.directory.join("sender.state")).map_err(error)?;
            uncertain.store(true, Ordering::Release);
            File::open(&owner.directory)
                .map_err(error)?
                .sync_all()
                .map_err(error)?;
            *state = snapshot;
            uncertain.store(false, Ordering::Release);
            if remove {
                pending.send_if_modified(|tickets| {
                    let count = tickets.len();
                    tickets.retain(|t| !t.same_ticket(&ticket));
                    count != tickets.len()
                });
            }
            Ok(())
        })
        .await
        .map_err(|e| ActorError::InternalError(e.to_string()))?
        .map_err(ActorError::InternalError)
    }
}
