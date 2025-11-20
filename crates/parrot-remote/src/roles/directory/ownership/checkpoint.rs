use super::*;
use std::collections::{BTreeMap, HashSet};

#[derive(Serialize, Deserialize)]
pub(in super::super) struct Image {
    records: Vec<OwnershipRecord>,
    current_values: Vec<(String, u64)>,
    values: Vec<Blob>,
    requests: Vec<History>,
}
#[derive(Serialize, Deserialize)]
struct Blob {
    revision: u64,
    resource: String,
    value: Bytes,
}
#[derive(Serialize, Deserialize)]
struct History {
    client: String,
    id: String,
    digest: [u8; 32],
    index: u64,
    term: u64,
    outcome: Outcome,
}
#[derive(Serialize, Deserialize)]
enum Outcome {
    Changed(OwnershipRecord),
    Observed(Option<OwnershipRecord>),
    Rejected {
        reason: OwnershipRejection,
        current: Option<OwnershipRecord>,
    },
    ValueChanged {
        ownership: OwnershipRecord,
        revision: u64,
    },
    ValueObserved {
        ownership: Option<OwnershipRecord>,
        revision: Option<u64>,
    },
}

impl State {
    pub(in super::super) fn check_positions(
        &self,
        positions: &mut HashSet<u64>,
    ) -> Result<(), DirectoryApplyError> {
        for (_, status) in self.requests.values() {
            let OwnershipStatus::Applied { index, .. } = status else {
                return Err(invalid());
            };
            if !positions.insert(*index) {
                return Err(invalid());
            }
        }
        Ok(())
    }
    pub(in super::super) fn capture(&self) -> Image {
        let mut records: Vec<_> = self.records.values().cloned().collect();
        records.sort_by(|a, b| a.resource.cmp(&b.resource));
        let mut current_values: Vec<_> = self
            .values
            .iter()
            .map(|(resource, value)| (resource.clone(), value.revision))
            .collect();
        current_values.sort();
        let mut values = BTreeMap::new();
        let mut requests: Vec<_> = self
            .requests
            .iter()
            .map(|((client, id), (digest, status))| {
                let OwnershipStatus::Applied {
                    index,
                    term,
                    outcome,
                } = status
                else {
                    unreachable!("ownership state retains only applied outcomes");
                };
                let mut intern = |record: &OwnershipRecord, value: &OwnershipValue| {
                    values.entry(value.revision).or_insert_with(|| Blob {
                        revision: value.revision,
                        resource: record.resource.clone(),
                        value: value.value.clone(),
                    });
                    value.revision
                };
                let outcome = match outcome {
                    OwnershipOutcome::Changed(record) => Outcome::Changed(record.clone()),
                    OwnershipOutcome::Observed(record) => Outcome::Observed(record.clone()),
                    OwnershipOutcome::Rejected { reason, current } => Outcome::Rejected {
                        reason: reason.clone(),
                        current: current.clone(),
                    },
                    OwnershipOutcome::ValueChanged { ownership, value } => Outcome::ValueChanged {
                        ownership: ownership.clone(),
                        revision: intern(ownership, value),
                    },
                    OwnershipOutcome::ValueObserved { ownership, value } => {
                        Outcome::ValueObserved {
                            ownership: ownership.clone(),
                            revision: value.as_ref().map(|value| {
                                intern(
                                    ownership.as_ref().expect("value has retained ownership"),
                                    value,
                                )
                            }),
                        }
                    }
                };
                History {
                    client: client.clone(),
                    id: id.clone(),
                    digest: *digest,
                    index: *index,
                    term: *term,
                    outcome,
                }
            })
            .collect();
        requests.sort_by(|a, b| (&a.client, &a.id).cmp(&(&b.client, &b.id)));
        Image {
            records,
            current_values,
            values: values.into_values().collect(),
            requests,
        }
    }
}

fn invalid() -> DirectoryApplyError {
    DirectoryApplyError::Invalid("inconsistent ownership snapshot".into())
}
impl Image {
    pub(in super::super) fn restore(
        self,
        index: u64,
        term: u64,
    ) -> Result<State, DirectoryApplyError> {
        if self.records.len() > MAX_RESOURCES || self.current_values.len() > MAX_RESOURCES {
            return Err(DirectoryApplyError::Capacity);
        }
        let mut state = State::default();
        let mut previous = None;
        for record in self.records {
            if previous.as_ref().is_some_and(|old| old >= &record.resource) {
                return Err(invalid());
            }
            previous = Some(record.resource.clone());
            validate_record(&record, index)?;
            state.records.insert(record.resource.clone(), record);
        }
        let mut blobs = BTreeMap::new();
        for blob in self.values {
            if blob.revision == 0
                || blob.revision > index
                || blob.value.len() > MAX_VALUE_BYTES
                || blobs
                    .last_key_value()
                    .is_some_and(|(last, _)| *last >= blob.revision)
            {
                return Err(invalid());
            }
            OwnershipCommand::Observe {
                resource: blob.resource.clone(),
            }
            .validate()?;
            blobs.insert(blob.revision, blob);
        }
        let lookup = |record: Option<&OwnershipRecord>,
                      revision: u64,
                      at: u64|
         -> Result<OwnershipValue, DirectoryApplyError> {
            let blob = blobs.get(&revision).ok_or_else(invalid)?;
            if revision > at || record.is_none_or(|record| record.resource != blob.resource) {
                return Err(invalid());
            }
            Ok(OwnershipValue {
                revision,
                value: blob.value.clone(),
            })
        };
        let mut previous = None;
        for (resource, revision) in self.current_values {
            if previous.as_ref().is_some_and(|old| old >= &resource) {
                return Err(invalid());
            }
            previous = Some(resource.clone());
            let value = lookup(state.records.get(&resource), revision, index)?;
            state.values.insert(resource, value);
        }
        let mut previous = None;
        let mut positions = HashSet::new();
        for request in self.requests {
            super::super::request::identity(&request.client, &request.id)?;
            let key = (request.client, request.id);
            if previous.as_ref().is_some_and(|old| old >= &key)
                || request.index == 0
                || request.index > index
                || request.term == 0
                || request.term > term
                || !positions.insert(request.index)
            {
                return Err(invalid());
            }
            previous = Some(key.clone());
            let outcome = match request.outcome {
                Outcome::Changed(record) => {
                    validate_record(&record, request.index)?;
                    if record.generation != request.index
                        || record
                            .holder
                            .as_ref()
                            .is_some_and(|holder| holder != &key.0)
                    {
                        return Err(invalid());
                    }
                    OwnershipOutcome::Changed(record)
                }
                Outcome::Observed(record) => {
                    if let Some(record) = &record {
                        validate_record(record, request.index)?;
                    }
                    OwnershipOutcome::Observed(record)
                }
                Outcome::Rejected { reason, current } => {
                    if let Some(record) = &current {
                        validate_record(record, request.index)?;
                    }
                    OwnershipOutcome::Rejected { reason, current }
                }
                Outcome::ValueChanged {
                    ownership,
                    revision,
                } => {
                    validate_record(&ownership, request.index)?;
                    if ownership.generation >= request.index
                        || ownership.holder.as_deref() != Some(key.0.as_str())
                        || revision != request.index
                    {
                        return Err(invalid());
                    }
                    let value = lookup(Some(&ownership), revision, request.index)?;
                    OwnershipOutcome::ValueChanged { ownership, value }
                }
                Outcome::ValueObserved {
                    ownership,
                    revision,
                } => {
                    if let Some(record) = &ownership {
                        validate_record(record, request.index)?;
                    }
                    let value = revision
                        .map(|revision| lookup(ownership.as_ref(), revision, request.index))
                        .transpose()?;
                    OwnershipOutcome::ValueObserved { ownership, value }
                }
            };
            state.requests.insert(
                key,
                (
                    request.digest,
                    OwnershipStatus::Applied {
                        index: request.index,
                        term: request.term,
                        outcome,
                    },
                ),
            );
        }
        // Every reference must name an original committed change. Live records
        // and values must point at the latest such change for their resource.
        let mut changes = HashMap::new();
        let mut latest = HashMap::<&str, u64>::new();
        let mut writes = HashMap::<&str, u64>::new();
        let mut written = HashSet::new();
        for (_, status) in state.requests.values() {
            let OwnershipStatus::Applied { outcome, .. } = status else {
                unreachable!()
            };
            match outcome {
                OwnershipOutcome::Changed(record) => {
                    changes.insert(record.generation, record);
                    let newest = latest.entry(&record.resource).or_default();
                    *newest = (*newest).max(record.generation);
                }
                OwnershipOutcome::ValueChanged { ownership, value } => {
                    written.insert(value.revision);
                    let newest = writes.entry(&ownership.resource).or_default();
                    *newest = (*newest).max(value.revision);
                }
                _ => {}
            }
        }
        if written.len() != blobs.len()
            || blobs.keys().any(|revision| !written.contains(revision))
            || latest.len() != state.records.len()
            || writes.len() != state.values.len()
        {
            return Err(invalid());
        }
        let verify = |record: &OwnershipRecord| -> Result<(), DirectoryApplyError> {
            if changes.get(&record.generation).copied() != Some(record) {
                return Err(invalid());
            }
            Ok(())
        };
        for record in state.records.values() {
            verify(record)?;
            if latest.get(record.resource.as_str()) != Some(&record.generation) {
                return Err(invalid());
            }
        }
        for (resource, value) in &state.values {
            if writes.get(resource.as_str()) != Some(&value.revision) {
                return Err(invalid());
            }
        }
        for (_, status) in state.requests.values() {
            let OwnershipStatus::Applied { outcome, .. } = status else {
                unreachable!()
            };
            match outcome {
                OwnershipOutcome::Changed(record) => verify(record)?,
                OwnershipOutcome::Observed(record)
                | OwnershipOutcome::Rejected {
                    current: record, ..
                } => {
                    if let Some(record) = record {
                        verify(record)?;
                    }
                }
                OwnershipOutcome::ValueChanged { ownership, .. }
                | OwnershipOutcome::ValueObserved {
                    ownership: Some(ownership),
                    ..
                } => verify(ownership)?,
                _ => {}
            }
        }
        Ok(state)
    }
}
fn validate_record(record: &OwnershipRecord, index: u64) -> Result<(), DirectoryApplyError> {
    OwnershipCommand::Observe {
        resource: record.resource.clone(),
    }
    .validate()?;
    if record.generation == 0 || record.generation > index {
        return Err(invalid());
    }
    if let Some(holder) = &record.holder {
        super::super::request::identity(holder, "snapshot")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let mut state = State::default();
        for (index, (id, command)) in [
            (
                "claim",
                OwnershipCommand::Claim {
                    resource: "resource".into(),
                    expected: None,
                },
            ),
            (
                "write",
                OwnershipCommand::WriteValue {
                    resource: "resource".into(),
                    generation: 1,
                    expected_revision: None,
                    value: b"old".to_vec(),
                },
            ),
            (
                "read",
                OwnershipCommand::ReadValue {
                    resource: "resource".into(),
                },
            ),
            (
                "release",
                OwnershipCommand::Release {
                    resource: "resource".into(),
                    expected: 1,
                },
            ),
            (
                "released-read",
                OwnershipCommand::ReadValue {
                    resource: "resource".into(),
                },
            ),
            (
                "reclaim",
                OwnershipCommand::Claim {
                    resource: "resource".into(),
                    expected: Some(4),
                },
            ),
            (
                "rewrite",
                OwnershipCommand::WriteValue {
                    resource: "resource".into(),
                    generation: 6,
                    expected_revision: Some(2),
                    value: b"new".to_vec(),
                },
            ),
            (
                "new-read",
                OwnershipCommand::ReadValue {
                    resource: "resource".into(),
                },
            ),
        ]
        .into_iter()
        .enumerate()
        {
            state.apply(
                decode(&encode("holder", id, &command).unwrap()).unwrap(),
                index as u64 + 1,
                2,
            );
        }
        state
    }
    #[test]
    fn ownership_image_references_original_changes_and_latest_live_state() {
        let restored = state().capture().restore(8, 2).unwrap();
        let (_, status) = restored
            .requests
            .get(&("holder".into(), "new-read".into()))
            .unwrap();
        let OwnershipStatus::Applied {
            outcome:
                OwnershipOutcome::ValueObserved {
                    value: Some(value), ..
                },
            ..
        } = status
        else {
            panic!("{status:?}");
        };
        assert_eq!(
            value.value.as_ptr(),
            restored.values["resource"].value.as_ptr()
        );
        for case in 0..13 {
            let mut image = state().capture();
            match case {
                0 => image.records.clear(),
                1 => image.current_values.clear(),
                2 => image.values.clear(),
                3 => image.values[0].resource = "other".into(),
                4 => image.values[0].revision = 0,
                5 => image.values[0].value = Bytes::from(vec![0; MAX_VALUE_BYTES + 1]),
                6 => image.requests.push(History {
                    client: "holder".into(),
                    id: "claim".into(),
                    digest: [0; 32],
                    index: 1,
                    term: 2,
                    outcome: Outcome::Observed(None),
                }),
                7 => image.requests[0].index = 0,
                8 => image.requests[0].term = 3,
                9 => image.records[0].generation = 4,
                10 => image.current_values[0].1 = 2,
                11 => {
                    let request = image
                        .requests
                        .iter_mut()
                        .find(|request| request.id == "read")
                        .unwrap();
                    let Outcome::ValueObserved { revision, .. } = &mut request.outcome else {
                        unreachable!()
                    };
                    *revision = Some(7);
                }
                _ => {
                    let request = image
                        .requests
                        .iter_mut()
                        .find(|request| request.id == "rewrite")
                        .unwrap();
                    let Outcome::ValueChanged { ownership, .. } = &mut request.outcome else {
                        unreachable!()
                    };
                    ownership.holder = Some("other".into());
                }
            }
            assert!(image.restore(8, 2).is_err(), "case {case}");
        }
    }
}
