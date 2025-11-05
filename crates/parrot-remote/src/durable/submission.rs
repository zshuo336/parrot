//! Atomic keyed acceptance records and retained checkpoint receipts.
use super::*;
use sha2::{Digest, Sha256};

pub(super) const ACCEPT: &str = "#outbox-accept-v1";
pub(super) const RECEIPT: &str = "#outbox-receipt-v1";
pub(super) fn reserved(key: &str) -> bool {
    matches!(key, ACCEPT | RECEIPT)
}
pub(super) fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) {
        return Err("submission ID must contain 1..1024 bytes and no control characters".into());
    }
    Ok(())
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Acceptance {
    request_id: String,
    type_key: String,
    payload: bytes::Bytes,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SavedReceipt {
    request_id: String,
    fingerprint: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Receipt {
    endpoint: String,
    sender: String,
    seq: u64,
    saved: SavedReceipt,
}
impl Receipt {
    fn key(&self) -> (String, String, String) {
        (
            self.endpoint.clone(),
            self.sender.clone(),
            self.saved.request_id.clone(),
        )
    }
    fn memory(&self) -> usize {
        // Include the separately-owned map key, receipt fields and fixed data.
        512 + 3 * (self.endpoint.len() + self.sender.len() + self.saved.request_id.len())
    }
    pub(super) fn record(&self) -> Result<WalRecord, String> {
        Ok(WalRecord {
            endpoint: self.endpoint.clone(),
            sender: self.sender.clone(),
            seq: self.seq,
            path: String::new(),
            type_key: RECEIPT.into(),
            payload: parrot_api::message::serde_remote_serialize(&self.saved)?.into(),
        })
    }
}
#[derive(Default)]
pub(super) struct ReceiptIndex {
    entries: HashMap<(String, String, String), Receipt>,
    sequences: HashMap<(String, String, u64), String>,
    bytes: usize,
    pub(super) checkpoint_bytes: usize,
}
impl ReceiptIndex {
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &Receipt> {
        self.entries.values()
    }
    pub(super) fn validate_binding(
        &self,
        endpoint: &str,
        sender: &str,
        last: u64,
    ) -> Result<(), String> {
        if self
            .entries
            .values()
            .any(|r| r.endpoint != endpoint || r.sender != sender || r.seq > last)
        {
            return Err("foreign or future submission receipt in outbox".into());
        }
        Ok(())
    }
    pub(super) fn lookup(&self, record: &WalRecord, id: &str) -> Result<Option<u64>, String> {
        validate_id(id)?;
        let key = (record.endpoint.clone(), record.sender.clone(), id.into());
        match self.entries.get(&key) {
            Some(receipt) if receipt.saved.fingerprint == fingerprint(record) => {
                Ok(Some(receipt.seq))
            }
            Some(_) => Err("submission ID conflicts with previously accepted content".into()),
            None => Ok(None),
        }
    }
    pub(super) fn check(&self, receipt: &Receipt, limits: WalLimits) -> Result<(), String> {
        if let Some(saved) = self.entries.get(&receipt.key()) {
            if saved != receipt {
                return Err("conflicting submission receipts in WAL".into());
            }
            return Ok(());
        }
        if self.sequences.contains_key(&(
            receipt.endpoint.clone(),
            receipt.sender.clone(),
            receipt.seq,
        )) {
            return Err("multiple submission IDs claim the same WAL sequence".into());
        }
        if self.entries.len() >= limits.max_submission_receipts
            || self
                .bytes
                .checked_add(receipt.memory())
                .is_none_or(|n| n > limits.max_submission_receipt_bytes)
        {
            return Err("submission receipt budget exceeded".into());
        }
        self.checkpoint_bytes
            .checked_add(encoded_size(&receipt.record()?, limits)?)
            .ok_or("submission checkpoint size overflow")?;
        Ok(())
    }
    pub(super) fn insert(&mut self, receipt: Receipt, limits: WalLimits) -> Result<(), String> {
        self.check(&receipt, limits)?;
        if !self.entries.contains_key(&receipt.key()) {
            self.bytes += receipt.memory();
            self.checkpoint_bytes += encoded_size(&receipt.record()?, limits)?;
            self.sequences.insert(
                (
                    receipt.endpoint.clone(),
                    receipt.sender.clone(),
                    receipt.seq,
                ),
                receipt.saved.request_id.clone(),
            );
            self.entries.insert(receipt.key(), receipt);
        }
        Ok(())
    }
}
fn fingerprint(record: &WalRecord) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"parrot.outbox.submission.v1\0");
    for bytes in [
        record.path.as_bytes(),
        record.type_key.as_bytes(),
        &record.payload,
    ] {
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    digest.finalize().into()
}
pub(super) fn wrap(record: &WalRecord, id: &str) -> Result<(WalRecord, Receipt), String> {
    validate_id(id)?;
    validate_record(record, false)?;
    let receipt = Receipt {
        endpoint: record.endpoint.clone(),
        sender: record.sender.clone(),
        seq: record.seq,
        saved: SavedReceipt {
            request_id: id.into(),
            fingerprint: fingerprint(record),
        },
    };
    let payload = parrot_api::message::serde_remote_serialize(&Acceptance {
        request_id: id.into(),
        type_key: record.type_key.clone(),
        payload: record.payload.clone(),
    })?;
    Ok((
        WalRecord {
            type_key: ACCEPT.into(),
            payload: payload.into(),
            ..record.clone()
        },
        receipt,
    ))
}
/// Returns normalized business and/or a receipt. Receipt-only records are not
/// pending work. The same outer CRC covers identity and business acceptance.
pub(super) fn read(record: WalRecord) -> Result<(Option<WalRecord>, Option<Receipt>), String> {
    match record.type_key.as_str() {
        ACCEPT => {
            let saved: Acceptance = parrot_api::message::serde_remote_deserialize(&record.payload)?;
            validate_id(&saved.request_id)?;
            let business = WalRecord {
                type_key: saved.type_key,
                payload: saved.payload,
                ..record
            };
            validate_record(&business, false)?;
            let receipt = Receipt {
                endpoint: business.endpoint.clone(),
                sender: business.sender.clone(),
                seq: business.seq,
                saved: SavedReceipt {
                    request_id: saved.request_id,
                    fingerprint: fingerprint(&business),
                },
            };
            Ok((Some(business), Some(receipt)))
        }
        RECEIPT => {
            if !record.path.is_empty() {
                return Err("submission receipt has a business path".into());
            }
            let saved: SavedReceipt =
                parrot_api::message::serde_remote_deserialize(&record.payload)?;
            validate_id(&saved.request_id)?;
            Ok((
                None,
                Some(Receipt {
                    endpoint: record.endpoint,
                    sender: record.sender,
                    seq: record.seq,
                    saved,
                }),
            ))
        }
        _ => Ok((Some(record), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "parrot-submission-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn rec(seq: u64, payload: &'static [u8]) -> WalRecord {
        WalRecord {
            endpoint: "edge".into(),
            sender: "session".into(),
            seq,
            path: "/sink".into(),
            type_key: "key".into(),
            payload: bytes::Bytes::from_static(payload),
        }
    }
    #[test]
    fn one_crc_record_preserves_key_and_business_through_crash_tail_and_checkpoint_gc() {
        let s = Scratch::new();
        let original = rec(1, b"a");
        let (stored, _) = wrap(&original, "request").unwrap();
        let encoded = encode_record(&stored, WalLimits::default()).unwrap();
        // Every torn prefix yields neither business nor receipt, never one half.
        for cut in [1, 7, 8, encoded.len() - 1] {
            std::fs::write(s.0.join("seg-00000001.log"), &encoded[..cut]).unwrap();
            let wal = Wal::open(&s.0).unwrap();
            assert_eq!(wal.pending_len().unwrap(), 0);
            assert_eq!(wal.state().unwrap().receipts.len(), 0);
            drop(wal);
        }
        std::fs::write(s.0.join("seg-00000001.log"), &encoded).unwrap();
        let wal = Wal::open(&s.0).unwrap();
        assert_eq!(wal.replay("edge").unwrap(), vec![original.clone()]);
        assert_eq!(
            wal.append_submission(rec(2, b"a"), Some("request"))
                .unwrap(),
            (1, false)
        );
        assert!(wal
            .append_submission(rec(2, b"b"), Some("request"))
            .is_err());
        wal.ack("edge", "session", 1).unwrap();
        wal.sync().unwrap();
        drop(wal);
        assert_eq!(list_segments(&s.0, 10).unwrap().len(), 1);
        let wal = Wal::open(&s.0).unwrap();
        assert_eq!(wal.pending_len().unwrap(), 0);
        assert_eq!(
            wal.append_submission(rec(2, b"a"), Some("request"))
                .unwrap(),
            (1, false)
        );
        assert!(wal
            .append_submission(rec(2, b"different"), Some("request"))
            .is_err());
        assert_eq!(
            wal.append_submission(rec(2, b"b"), Some("next")).unwrap(),
            (2, true)
        );
    }
    #[test]
    fn receipts_are_scoped_bounded_and_reduced_recovery_limits_preserve_evidence() {
        let s = Scratch::new();
        let limits = WalLimits {
            max_submission_receipts: 2,
            ..Default::default()
        };
        let wal = Wal::open_with_limits(&s.0, 4096, limits).unwrap();
        assert_eq!(
            wal.append_submission(rec(1, b"a"), Some("same")).unwrap(),
            (1, true)
        );
        let mut other = rec(1, b"b");
        other.sender = "other".into();
        assert_eq!(
            wal.append_submission(other, Some("same")).unwrap(),
            (1, true)
        );
        assert!(wal.append_submission(rec(2, b"c"), Some("third")).is_err());
        assert_eq!(
            wal.append_submission(rec(2, b"a"), Some("same")).unwrap(),
            (1, false)
        );
        wal.ack("edge", "session", 1).unwrap();
        wal.ack("edge", "other", 1).unwrap();
        wal.sync().unwrap();
        drop(wal);
        let files: Vec<_> = list_segments(&s.0, 10)
            .unwrap()
            .into_iter()
            .map(|sid| {
                let path = segment_path(&s.0, sid);
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        for limits in [
            WalLimits {
                max_submission_receipts: 1,
                ..Default::default()
            },
            WalLimits {
                max_submission_receipt_bytes: 1,
                ..Default::default()
            },
        ] {
            assert!(Wal::open_with_limits(&s.0, 4096, limits).is_err());
            for (p, bytes) in &files {
                assert_eq!(&std::fs::read(p).unwrap(), bytes);
            }
        }
        let wal = Wal::open(&s.0).unwrap();
        assert_eq!(wal.state().unwrap().receipts.len(), 2);
        assert_eq!(
            wal.append_submission(rec(2, b"a"), Some("same")).unwrap(),
            (1, false)
        );
    }
    #[test]
    fn conflicting_checkpoint_receipts_fail_recovery_without_erasing_files() {
        let s = Scratch::new();
        let (_, a) = wrap(&rec(1, b"a"), "id").unwrap();
        let (_, b) = wrap(&rec(1, b"b"), "id").unwrap();
        let mut data = encode_record(&a.record().unwrap(), WalLimits::default()).unwrap();
        data.extend(encode_record(&b.record().unwrap(), WalLimits::default()).unwrap());
        let path = s.0.join("seg-00000001.log");
        std::fs::write(&path, &data).unwrap();
        assert!(Wal::open(&s.0).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), data);
        let (_, alias) = wrap(&rec(1, b"a"), "another-id").unwrap();
        let mut data = encode_record(&a.record().unwrap(), WalLimits::default()).unwrap();
        data.extend(encode_record(&alias.record().unwrap(), WalLimits::default()).unwrap());
        std::fs::write(&path, &data).unwrap();
        assert!(Wal::open(&s.0).is_err());
        assert_eq!(std::fs::read(path).unwrap(), data);
    }
}
