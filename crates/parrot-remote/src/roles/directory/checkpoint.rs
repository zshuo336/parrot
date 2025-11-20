use super::{
    ownership, request, DirCmd, DirEntry, DirectoryApplyError, DirectoryRequestStatus, DirectorySm,
};
use crate::raft::{LogEntry, RaftSnapshot};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
};

const MAX_BYTES: usize = 16 * 1024 * 1024;
type RequestKey = (String, String);
type RequestResult = ([u8; 32], DirectoryRequestStatus);
pub(super) type Requests = HashMap<RequestKey, RequestResult>;
pub(super) struct Recovered {
    pub sm: DirectorySm,
    pub requests: Requests,
    pub ownership: ownership::State,
}
#[derive(Serialize, Deserialize)]
struct Image {
    version: u8,
    index: u64,
    term: u64,
    revision: u64,
    nodes: Vec<(String, DirEntry)>,
    borders: Vec<(String, String)>,
    keys: Vec<(String, Vec<String>)>,
    requests: Vec<(RequestKey, RequestResult)>,
    ownership: ownership::checkpoint::Image,
}
pub(super) fn encode(
    index: u64,
    term: u64,
    sm: &DirectorySm,
    requests: &Requests,
    ownership: &ownership::State,
) -> Result<Vec<u8>, DirectoryApplyError> {
    fn sorted<V: Clone>(map: &HashMap<String, V>) -> Vec<(String, V)> {
        let mut entries: Vec<_> = map
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }
    let mut requests: Vec<_> = requests
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    requests.sort_by(|a, b| a.0.cmp(&b.0));
    let image = Image {
        version: 1,
        index,
        term,
        revision: sm.version,
        nodes: sorted(&sm.nodes),
        borders: sorted(&sm.borders),
        keys: sorted(&sm.key_index),
        requests,
        ownership: ownership.capture(),
    };
    bincode::serde::encode_into_std_write(&image, &mut Counter(0), bincode::config::standard())
        .map_err(|_| DirectoryApplyError::Capacity)?;
    bincode::serde::encode_to_vec(image, bincode::config::standard())
        .map_err(|error| DirectoryApplyError::Invalid(error.to_string()))
}
fn invalid() -> DirectoryApplyError {
    DirectoryApplyError::Invalid("inconsistent directory snapshot".into())
}
fn ordered<K: Ord, V>(entries: &[(K, V)]) -> bool {
    entries.windows(2).all(|pair| pair[0].0 < pair[1].0)
}
pub(super) fn decode(snapshot: &RaftSnapshot) -> Result<Recovered, DirectoryApplyError> {
    let image: Image = parrot_api::message::serde_remote_deserialize_with_limits(
        &snapshot.data,
        parrot_api::message::RemoteDecodeLimits {
            max_payload_bytes: MAX_BYTES,
            max_values: 2 * MAX_BYTES,
            max_depth: 24,
        },
    )
    .map_err(DirectoryApplyError::Invalid)?;
    if image.version != 1
        || image.index != snapshot.index
        || image.term != snapshot.term
        || image.index == 0
        || image.term == 0
        || image.revision > image.index
        || !ordered(&image.nodes)
        || !ordered(&image.borders)
        || !ordered(&image.keys)
        || !ordered(&image.requests)
    {
        return Err(invalid());
    }
    for (node, entry) in &image.nodes {
        DirCmd::Upsert {
            node: node.clone(),
            endpoints: entry.endpoints.clone(),
        }
        .encode()?;
        if entry.version == 0 || entry.version > image.revision || entry.ttl_s != 3600 {
            return Err(invalid());
        }
    }
    for (prefix, node) in &image.borders {
        DirCmd::BorderDeclare {
            prefix: prefix.clone(),
            node: node.clone(),
        }
        .encode()?;
    }
    for (key, nodes) in &image.keys {
        DirCmd::KeyAggregate {
            key: key.clone(),
            nodes: nodes.clone(),
        }
        .encode()?;
    }
    let mut positions = HashSet::new();
    let mut revisions = HashSet::new();
    for ((client, id), (_, status)) in &image.requests {
        request::identity(client, id)?;
        let DirectoryRequestStatus::Applied {
            index,
            term,
            revision,
        } = status
        else {
            return Err(invalid());
        };
        if *index == 0
            || *index > image.index
            || *term == 0
            || *term > image.term
            || *revision == 0
            || *revision > image.revision
            || !positions.insert(*index)
            || !revisions.insert(*revision)
        {
            return Err(invalid());
        }
    }
    let ownership = image.ownership.restore(image.index, image.term)?;
    ownership.check_positions(&mut positions)?;
    Ok(Recovered {
        sm: DirectorySm {
            nodes: image.nodes.into_iter().collect(),
            borders: image.borders.into_iter().collect(),
            key_index: image.keys.into_iter().collect(),
            version: image.revision,
        },
        requests: image.requests.into_iter().collect(),
        ownership,
    })
}
pub(super) fn validate_replay(
    snapshot: Option<&RaftSnapshot>,
    entries: &[LogEntry],
) -> Result<(), String> {
    let recovered = snapshot
        .map(decode)
        .transpose()
        .map_err(|error| error.to_string())?;
    let mut revision = recovered.as_ref().map_or(0, |state| state.sm.version);
    let mut known: HashSet<_> =
        recovered.map_or_else(HashSet::new, |state| state.requests.into_keys().collect());
    for entry in entries {
        let changes = match request::decode(&entry.cmd).map_err(|error| error.to_string())? {
            request::StoredCommand::ReadBarrier | request::StoredCommand::Ownership(_) => false,
            request::StoredCommand::Plain(_) => true,
            request::StoredCommand::Request(request) => known.insert((request.client, request.id)),
        };
        revision = revision
            .checked_add(u64::from(changes))
            .ok_or_else(|| DirectoryApplyError::RevisionExhausted.to_string())?;
    }
    Ok(())
}
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_BYTES - self.0 {
            return Err(std::io::Error::other("directory snapshot capacity"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::StateMachine;

    fn image() -> Image {
        let mut sm = DirectorySm::default();
        sm.apply(
            &DirCmd::Upsert {
                node: "worker".into(),
                endpoints: vec!["mem://worker".into()],
            }
            .encode()
            .unwrap(),
        )
        .unwrap();
        let requests = HashMap::from([(
            ("client".into(), "request".into()),
            (
                [1; 32],
                DirectoryRequestStatus::Applied {
                    index: 1,
                    term: 1,
                    revision: 1,
                },
            ),
        )]);
        let bytes = encode(3, 2, &sm, &requests, &ownership::State::default()).unwrap();
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
            .unwrap()
            .0
    }

    #[test]
    fn checkpoint_encoding_is_canonical_and_rejects_inconsistent_application_images() {
        let original = image();
        let bytes = bincode::serde::encode_to_vec(&original, bincode::config::standard()).unwrap();
        let snapshot = RaftSnapshot {
            index: 3,
            term: 2,
            data: bytes.clone(),
        };
        let restored = decode(&snapshot).unwrap();
        assert_eq!(
            encode(3, 2, &restored.sm, &restored.requests, &restored.ownership).unwrap(),
            bytes
        );
        for case in 0..14 {
            let mut image = image();
            match case {
                0 => image.version = 2,
                1 => image.index = 4,
                2 => image.term = 3,
                3 => image.revision = 0,
                4 => image.nodes[0].1.version = 0,
                5 => image.nodes[0].1.version = 2,
                6 => image.nodes[0].1.ttl_s = 0,
                7 => image.nodes.push(image.nodes[0].clone()),
                8 => image.borders.push((String::new(), "worker".into())),
                9 => image.keys.push(("key".into(), vec!["worker".into(); 4097])),
                10 => image.requests[0].1 .1 = DirectoryRequestStatus::Unknown,
                11 => {
                    image.requests[0].1 .1 = DirectoryRequestStatus::Applied {
                        index: 4,
                        term: 1,
                        revision: 1,
                    }
                }
                12 => {
                    image.requests[0].1 .1 = DirectoryRequestStatus::Applied {
                        index: 1,
                        term: 3,
                        revision: 1,
                    }
                }
                _ => image.requests.push(image.requests[0].clone()),
            }
            let data = bincode::serde::encode_to_vec(image, bincode::config::standard()).unwrap();
            assert!(
                decode(&RaftSnapshot {
                    index: 3,
                    term: 2,
                    data
                })
                .is_err(),
                "case {case}"
            );
        }
    }
}
