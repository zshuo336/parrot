//! Upgrade an already validated legacy binding before writing causal WAL data.
use super::*;

pub(super) fn upgrade(wal: &Wal, identity: &Identity) -> Result<(), OutboxError> {
    let next = wal.dir.join("outbox.identity.next");
    match std::fs::symlink_metadata(&next) {
        Ok(_) => {
            // A validated old primary is authoritative until rename. A torn
            // regular staging file has no committed data; never follow links.
            drop(open_regular(&next, OpenKind::Read).map_err(OutboxError::Storage)?);
            std::fs::remove_file(&next).map_err(storage)?;
            sync_dir(&wal.dir).map_err(OutboxError::Storage)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(storage(error)),
    }
    let bytes =
        serde_json::to_vec(identity).map_err(|error| OutboxError::Invalid(error.to_string()))?;
    if bytes.len() + 4 > 16 * 1024 {
        return Err(OutboxError::Invalid("identity exceeds byte budget".into()));
    }
    let mut file = open_regular(&next, OpenKind::New).map_err(OutboxError::Storage)?;
    file.write_all(&crc32(&bytes).to_le_bytes())
        .and_then(|()| file.write_all(&bytes))
        .and_then(|()| file.sync_all())
        .map_err(storage)?;
    std::fs::rename(next, wal.dir.join("outbox.identity")).map_err(storage)?;
    sync_dir(&wal.dir).map_err(OutboxError::Storage)
}
fn storage(error: std::io::Error) -> OutboxError {
    OutboxError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    const OLD_IDENTITY: &[u8] =
        include_bytes!("../../../tests/fixtures/outbox_trace/legacy-v2/outbox.identity");
    const OLD_WAL: &[u8] =
        include_bytes!("../../../tests/fixtures/outbox_trace/legacy-v2/seg-00000001.log");
    struct Scratch(PathBuf);
    impl Scratch {
        fn legacy() -> Self {
            let root = std::env::var_os("PARROT_OUTBOX_TRACE_EVIDENCE")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let path = root.join(format!("outbox-binding-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("outbox.identity"), OLD_IDENTITY).unwrap();
            std::fs::write(path.join("seg-00000001.log"), OLD_WAL).unwrap();
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
    fn desired() -> Identity {
        Identity {
            version: 4,
            transactional: true,
            source: "source".into(),
            receiver: "receiver".into(),
            path: "/state".into(),
            sender: "session".into(),
        }
    }
    fn version(path: &std::path::Path) -> u32 {
        let bytes = std::fs::read(path.join("outbox.identity")).unwrap();
        assert_eq!(
            crc32(&bytes[4..]),
            u32::from_le_bytes(bytes[..4].try_into().unwrap())
        );
        serde_json::from_slice::<Identity>(&bytes[4..])
            .unwrap()
            .version
    }
    #[test]
    fn real_legacy_v2_fixture_upgrades_without_changing_business_or_idempotency() {
        let directory = Scratch::legacy();
        let wal = Wal::open(&directory.0).unwrap();
        let before = wal.replay("receiver").unwrap();
        let status = bind_and_recover(&wal, &desired()).unwrap();
        assert_eq!(status.pending, 1);
        assert_eq!(status.submission_receipts, 1);
        assert_eq!(version(&directory.0), 4);
        assert!(!directory.0.join("outbox.identity.next").exists());
        assert_eq!(wal.replay("receiver").unwrap(), before);
        assert!(wal.state().unwrap().pending[0].parent.is_none());
        drop(wal);
        let wal = Wal::open(&directory.0).unwrap();
        bind_and_recover(&wal, &desired()).unwrap();
        let mut retry = before[0].clone();
        retry.seq = 2;
        assert_eq!(
            wal.append_submission(retry, Some("initial")).unwrap(),
            (1, false)
        );
    }
    #[test]
    fn invalid_recovered_sequence_does_not_upgrade_old_primary() {
        let directory = Scratch::legacy();
        let wal = Wal::open(&directory.0).unwrap();
        let mut record = wal.replay("receiver").unwrap()[0].clone();
        record.seq = 3;
        wal.append(record).unwrap();
        wal.sync().unwrap();
        assert!(bind_and_recover(&wal, &desired()).is_err());
        assert_eq!(
            std::fs::read(directory.0.join("outbox.identity")).unwrap(),
            OLD_IDENTITY
        );
    }
    #[test]
    fn mode_change_does_not_upgrade_old_primary() {
        let directory = Scratch::legacy();
        let wal = Wal::open(&directory.0).unwrap();
        let mut identity = desired();
        identity.version = 3;
        assert!(bind_and_recover(&wal, &identity).is_err());
        assert_eq!(
            std::fs::read(directory.0.join("outbox.identity")).unwrap(),
            OLD_IDENTITY
        );
    }
    #[test]
    fn torn_migration_staging_is_recovered_only_after_validating_old_primary() {
        for length in [0, 1, 4, 32, 100] {
            let directory = Scratch::legacy();
            std::fs::write(directory.0.join("outbox.identity.next"), vec![0xff; length]).unwrap();
            let wal = Wal::open(&directory.0).unwrap();
            bind_and_recover(&wal, &desired()).unwrap();
            assert_eq!(version(&directory.0), 4);
            assert!(!directory.0.join("outbox.identity.next").exists());
        }
    }
    #[cfg(unix)]
    #[test]
    fn migration_rejects_staging_symlinks_and_preserves_both_target_and_old_primary() {
        let directory = Scratch::legacy();
        let target = directory.0.join("outside");
        std::fs::write(&target, b"do not touch").unwrap();
        std::os::unix::fs::symlink(&target, directory.0.join("outbox.identity.next")).unwrap();
        let wal = Wal::open(&directory.0).unwrap();
        assert!(bind_and_recover(&wal, &desired()).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"do not touch");
        assert_eq!(
            std::fs::read(directory.0.join("outbox.identity")).unwrap(),
            OLD_IDENTITY
        );
    }
    #[test]
    fn legacy_binding_cannot_contain_causal_records_before_upgrade() {
        let directory = Scratch::legacy();
        let wal = Wal::open(&directory.0).unwrap();
        let mut record = wal.replay("receiver").unwrap()[0].clone();
        record.seq = 2;
        wal.append_submission_with_parent(
            record,
            None,
            Some(parrot_api::trace::TraceParent::new([1; 16], [2; 8]).unwrap()),
        )
        .unwrap();
        wal.sync().unwrap();
        assert!(bind_and_recover(&wal, &desired()).is_err());
        assert_eq!(
            std::fs::read(directory.0.join("outbox.identity")).unwrap(),
            OLD_IDENTITY
        );
    }
    #[test]
    fn legacy_v1_plain_layout_upgrades_to_v3_without_inventing_a_parent() {
        let directory = Scratch::legacy();
        let path = directory.0.join("plain");
        std::fs::create_dir(&path).unwrap();
        let mut identity = desired();
        identity.version = 1;
        let body = serde_json::to_vec(&identity).unwrap();
        let mut bytes = crc32(&body).to_le_bytes().to_vec();
        bytes.extend_from_slice(&body);
        std::fs::write(path.join("outbox.identity"), bytes).unwrap();
        let wal = Wal::open(&path).unwrap();
        let record = WalRecord {
            endpoint: "receiver".into(),
            sender: "session".into(),
            seq: 1,
            path: "/state".into(),
            type_key: "delta".into(),
            payload: Bytes::from_static(b"plain"),
        };
        wal.append(record.clone()).unwrap();
        wal.sync().unwrap();
        drop(wal);
        let wal = Wal::open(&path).unwrap();
        identity.version = 3;
        assert_eq!(bind_and_recover(&wal, &identity).unwrap().pending, 1);
        assert_eq!(version(&path), 3);
        assert_eq!(wal.replay("receiver").unwrap(), vec![record]);
        assert!(wal.state().unwrap().pending[0].parent.is_none());
    }
    #[test]
    fn trace_bytes_must_not_consume_the_space_reserved_for_completion_ack() {
        let directory = Scratch::legacy();
        let identity = desired();
        let parent = parrot_api::trace::TraceParent::new([1; 16], [2; 8]).unwrap();
        let record = WalRecord {
            endpoint: identity.receiver.clone(),
            sender: identity.sender.clone(),
            seq: 1,
            path: identity.path.clone(),
            type_key: "delta".into(),
            payload: Bytes::from_static(b"business"),
        };
        let limits = WalLimits::default();
        let stored = super::super::super::causal::wrap(record, Some(parent)).unwrap();
        let data = encoded_size(&stored, limits).unwrap();
        let ack = encoded_size(
            &ack_record(&identity.receiver, &identity.sender, u64::MAX),
            limits,
        )
        .unwrap();
        let cap = (data + ack - 1) as u64;
        let wal = Wal::open_with_limits(
            directory.0.join("budget"),
            cap,
            WalLimits {
                max_disk_bytes: cap,
                ..limits
            },
        )
        .unwrap();
        let (reply, _response) = oneshot::channel();
        let command = Command {
            parent: Some(parent),
            request_id: None,
            key: "delta".into(),
            payload: Bytes::from_static(b"business"),
            _bytes: Arc::new(Semaphore::new(64))
                .try_acquire_many_owned(64)
                .unwrap(),
            reply,
        };
        assert!(append_for_group(&wal, &identity, &command, 1).is_err());
        assert_eq!(wal.pending_len().unwrap(), 0);
        assert_eq!(wal.state().unwrap().disk_bytes, 0);
    }
    #[test]
    fn admission_frame_limit_matches_real_plain_and_transactional_wire_encoding() {
        use crate::frame::{Frame, MAX_FRAME_LEN, TRACE_CONTEXT_BYTES};
        for transactional in [false, true] {
            let mut identity = desired();
            identity.transactional = transactional;
            let path = format!("parrot://{}{}", identity.receiver, identity.path);
            let frame = |length| {
                let payload = Bytes::from(vec![0; length]);
                if transactional {
                    let envelope = super::super::super::DurableEnvelope {
                        source: identity.source.clone(),
                        session: identity.sender.clone(),
                        receiver: identity.receiver.clone(),
                        path: identity.path.clone(),
                        sequence: u64::MAX,
                        type_key: "delta".into(),
                        payload,
                    };
                    Frame::tell_with_ack(
                        1,
                        &path,
                        super::super::super::DURABLE_KEY,
                        envelope.encode().unwrap(),
                    )
                } else {
                    Frame::tell_with_ack(1, &path, "delta", payload)
                }
            };
            let mut empty = bytes::BytesMut::new();
            frame(0).encode(&mut empty).unwrap();
            let exact = MAX_FRAME_LEN as usize + 4 - empty.len() - TRACE_CONTEXT_BYTES;
            let parent = parrot_api::trace::TraceParent::new([1; 16], [2; 8]).unwrap();
            let mut wire = bytes::BytesMut::new();
            frame(exact)
                .with_trace_parent(parent)
                .unwrap()
                .encode(&mut wire)
                .unwrap();
            assert_eq!(wire.len(), MAX_FRAME_LEN as usize + 4);
            assert!(identity.validate_message("delta", exact, true).is_ok());
            assert!(identity.validate_message("delta", exact + 1, true).is_err());
            assert!(frame(exact + 1).with_trace_parent(parent).is_err());
            assert!(identity
                .validate_message("delta", exact + TRACE_CONTEXT_BYTES, false)
                .is_ok());
            assert!(identity
                .validate_message("delta", exact + TRACE_CONTEXT_BYTES + 1, false)
                .is_err());
        }
    }
}
