//! Versioned causal metadata and business bytes share one outer WAL CRC.
use super::WalRecord;
use parrot_api::trace::TraceParent;

pub(super) const KEY: &str = "#outbox-causal-v1";
pub(super) fn reserved(key: &str) -> bool {
    key.starts_with("#outbox-causal-")
}

#[derive(Clone)]
pub(super) struct PendingRecord {
    pub(super) record: WalRecord,
    pub(super) parent: Option<TraceParent>,
}
impl std::ops::Deref for PendingRecord {
    type Target = WalRecord;
    fn deref(&self) -> &Self::Target {
        &self.record
    }
}

pub(super) fn wrap(record: WalRecord, parent: Option<TraceParent>) -> Result<WalRecord, String> {
    let Some(parent) = parent else {
        return Ok(record);
    };
    check_inner(&record.type_key)?;
    let length = u32::try_from(record.type_key.len()).map_err(|_| "causal WAL key too large")?;
    let size = 29usize
        .checked_add(record.type_key.len())
        .and_then(|n| n.checked_add(record.payload.len()))
        .ok_or("causal WAL size overflow")?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(size)
        .map_err(|_| "causal WAL allocation failed")?;
    payload.push(1);
    payload.extend_from_slice(&parent.trace_id());
    payload.extend_from_slice(&parent.span_id());
    payload.extend_from_slice(&length.to_le_bytes());
    payload.extend_from_slice(record.type_key.as_bytes());
    payload.extend_from_slice(&record.payload);
    Ok(WalRecord {
        type_key: KEY.into(),
        payload: payload.into(),
        ..record
    })
}

fn check_inner(key: &str) -> Result<(), String> {
    if key.is_empty() || reserved(key) || key == "#ack" || key == super::submission::RECEIPT {
        return Err("invalid nested causal WAL record".into());
    }
    Ok(())
}

pub(super) fn read(record: WalRecord) -> Result<(WalRecord, Option<TraceParent>), String> {
    if !reserved(&record.type_key) {
        return Ok((record, None));
    }
    if record.type_key != KEY || record.payload.len() < 29 || record.payload[0] != 1 {
        return Err("unsupported or truncated causal WAL record".into());
    }
    let parent = TraceParent::new(
        record.payload[1..17].try_into().unwrap(),
        record.payload[17..25].try_into().unwrap(),
    )
    .map_err(|e| e.to_string())?;
    let length = u32::from_le_bytes(record.payload[25..29].try_into().unwrap()) as usize;
    let end = 29usize
        .checked_add(length)
        .filter(|n| *n <= record.payload.len())
        .ok_or("truncated causal WAL key")?;
    let key = std::str::from_utf8(&record.payload[29..end])
        .map_err(|_| "invalid causal WAL key UTF-8")?;
    check_inner(key)?;
    // Do not retain the metadata/key backing allocation through a small slice.
    let inner = WalRecord {
        type_key: key.into(),
        payload: bytes::Bytes::copy_from_slice(&record.payload[end..]),
        ..record
    };
    Ok((inner, Some(parent)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::*;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let root = std::env::var_os("PARROT_OUTBOX_TRACE_EVIDENCE")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let path = root.join(format!("causal-wal-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            if std::env::var_os("PARROT_OUTBOX_TRACE_EVIDENCE").is_none() {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
    fn business() -> WalRecord {
        WalRecord {
            endpoint: "receiver".into(),
            sender: "session".into(),
            seq: 1,
            path: "/state".into(),
            type_key: "delta".into(),
            payload: bytes::Bytes::from_static(b"business"),
        }
    }
    fn parent() -> TraceParent {
        TraceParent::new([1; 16], [2; 8]).unwrap()
    }

    #[test]
    fn causal_identity_and_idempotency_survive_reopen_without_changing_business_fingerprint() {
        let directory = Scratch::new();
        let original = business();
        let wal = Wal::open(&directory.0).unwrap();
        assert_eq!(
            wal.append_submission_with_parent(original.clone(), Some("request"), Some(parent()))
                .unwrap(),
            (1, true)
        );
        wal.sync().unwrap();
        drop(wal);
        let wal = Wal::open(&directory.0).unwrap();
        assert_eq!(wal.replay("receiver").unwrap(), vec![original.clone()]);
        assert_eq!(wal.state().unwrap().pending[0].parent, Some(parent()));
        let mut retry = original;
        retry.seq = 2;
        assert_eq!(
            wal.append_submission_with_parent(
                retry.clone(),
                Some("request"),
                Some(TraceParent::new([3; 16], [4; 8]).unwrap())
            )
            .unwrap(),
            (1, false)
        );
        assert_eq!(wal.state().unwrap().pending[0].parent, Some(parent()));
        wal.ack("receiver", "session", 1).unwrap();
        wal.sync().unwrap();
        drop(wal);
        let wal = Wal::open(&directory.0).unwrap();
        assert_eq!(wal.pending_len().unwrap(), 0);
        assert_eq!(
            wal.append_submission_with_parent(retry.clone(), Some("request"), None)
                .unwrap(),
            (1, false)
        );
        retry.payload = bytes::Bytes::from_static(b"changed");
        assert!(wal
            .append_submission_with_parent(retry, Some("request"), Some(parent()))
            .is_err());
    }
    #[test]
    fn every_torn_crc_record_prefix_recovers_neither_business_nor_receipt() {
        let (acceptance, _) = submission::wrap(&business(), "request").unwrap();
        let stored = wrap(acceptance, Some(parent())).unwrap();
        let encoded = encode_record(&stored, WalLimits::default()).unwrap();
        for end in 1..encoded.len() {
            let directory = Scratch::new();
            let file = directory.0.join("seg-00000001.log");
            std::fs::write(&file, &encoded[..end]).unwrap();
            let wal = Wal::open(&directory.0).unwrap();
            assert_eq!(wal.pending_len().unwrap(), 0);
            assert_eq!(wal.state().unwrap().receipts.len(), 0);
            assert_eq!(std::fs::metadata(file).unwrap().len(), 0);
        }
    }
    #[test]
    fn invalid_complete_metadata_is_rejected_before_repairing_a_later_torn_tail() {
        let valid = wrap(business(), Some(parent())).unwrap();
        let mut variants = Vec::new();
        let mut record = valid.clone();
        record.type_key = "#outbox-causal-v2".into();
        variants.push(record);
        for index in [0usize, 1, 17] {
            let mut record = valid.clone();
            let mut payload = record.payload.to_vec();
            match index {
                0 => payload[0] = 2,
                1 => payload[1..17].fill(0),
                _ => payload[17..25].fill(0),
            }
            record.payload = payload.into();
            variants.push(record);
        }
        let mut record = valid.clone();
        record.payload = record.payload.slice(..24);
        variants.push(record);
        let mut record = valid.clone();
        let mut bytes = record.payload.to_vec();
        bytes[25..29].copy_from_slice(&u32::MAX.to_le_bytes());
        record.payload = bytes.into();
        variants.push(record);
        for key in ["#ack", submission::RECEIPT, KEY, "#outbox-causal-v9"] {
            let mut record = valid.clone();
            let mut payload = record.payload[..25].to_vec();
            payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
            payload.extend_from_slice(key.as_bytes());
            record.payload = payload.into();
            variants.push(record);
        }
        let mut record = valid;
        let mut payload = record.payload.to_vec();
        payload[29] = 0xff;
        record.payload = payload.into();
        variants.push(record);
        for record in variants {
            let directory = Scratch::new();
            let file = directory.0.join("seg-00000001.log");
            let mut bytes = encode_record(&record, WalLimits::default()).unwrap();
            bytes.extend_from_slice(&[1, 2, 3]);
            std::fs::write(&file, &bytes).unwrap();
            assert!(Wal::open(&directory.0).is_err());
            assert_eq!(std::fs::read(file).unwrap(), bytes);
        }
    }
    #[test]
    fn optional_metadata_counts_against_record_and_pending_memory_budgets() {
        let record = business();
        let plain = encode_record(&record, WalLimits::default()).unwrap();
        let directory = Scratch::new();
        let limits = WalLimits {
            max_record_bytes: plain.len() - 8,
            ..Default::default()
        };
        let wal = Wal::open_with_limits(&directory.0, 1024, limits).unwrap();
        assert!(wal
            .append_submission_with_parent(record.clone(), None, Some(parent()))
            .is_err());
        assert_eq!(wal.pending_len().unwrap(), 0);
        assert_eq!(wal.state().unwrap().disk_bytes, 0);
        wal.append(record.clone()).unwrap();
        wal.sync().unwrap();
        let directory = Scratch::new();
        let limits = WalLimits {
            max_pending_bytes: record_memory(&record) - 1,
            ..Default::default()
        };
        let wal = Wal::open_with_limits(&directory.0, 1024, limits).unwrap();
        assert!(wal
            .append_submission_with_parent(record, None, Some(parent()))
            .is_err());
        assert_eq!(wal.pending_len().unwrap(), 0);
        assert_eq!(wal.state().unwrap().disk_bytes, 0);
    }
    #[test]
    fn ordinary_wal_writer_cannot_forge_reserved_causal_records() {
        let directory = Scratch::new();
        let wal = Wal::open(&directory.0).unwrap();
        for key in [KEY, "#outbox-causal-v2", "#outbox-causal-unknown"] {
            let mut record = business();
            record.type_key = key.into();
            assert!(wal.append(record).is_err());
        }
        assert_eq!(wal.pending_len().unwrap(), 0);
        assert_eq!(wal.state().unwrap().disk_bytes, 0);
    }
    #[test]
    fn exact_pending_memory_limit_includes_parent_storage_on_append_and_recovery() {
        let directory = Scratch::new();
        let record = business();
        let budget = std::mem::size_of::<PendingRecord>()
            + record.endpoint.len()
            + record.sender.len()
            + record.path.len()
            + record.type_key.len()
            + record.payload.len();
        let limits = WalLimits {
            max_pending_bytes: budget,
            ..Default::default()
        };
        let wal = Wal::open_with_limits(&directory.0, 1024, limits).unwrap();
        wal.append_submission_with_parent(record.clone(), None, Some(parent()))
            .unwrap();
        let before = std::fs::read(directory.0.join("seg-00000001.log")).unwrap();
        let mut second = record.clone();
        second.seq = 2;
        assert!(wal.append(second).is_err());
        assert_eq!(
            std::fs::read(directory.0.join("seg-00000001.log")).unwrap(),
            before
        );
        wal.sync().unwrap();
        drop(wal);
        assert!(Wal::open_with_limits(
            &directory.0,
            1024,
            WalLimits {
                max_pending_bytes: budget - 1,
                ..limits
            }
        )
        .is_err());
        assert_eq!(
            std::fs::read(directory.0.join("seg-00000001.log")).unwrap(),
            before
        );
        let wal = Wal::open_with_limits(&directory.0, 1024, limits).unwrap();
        assert_eq!(wal.replay("receiver").unwrap(), vec![record]);
        assert_eq!(wal.state().unwrap().pending[0].parent, Some(parent()));
    }
}
