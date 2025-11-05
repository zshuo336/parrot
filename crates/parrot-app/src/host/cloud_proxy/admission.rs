//! Byte admission before mailbox waiting, retained through processing.
use super::*;
use parrot::thread::{mailbox::admission::MessageAdmission, MailboxError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug)]
pub(super) struct Admitted {
    message: BoxedMessage,
    permit: OwnedSemaphorePermit,
}
impl Admitted {
    pub(super) fn message(message: &BoxedMessage) -> &BoxedMessage {
        message
            .downcast_ref::<Self>()
            .map_or(message, |m| &m.message)
    }
    pub(super) fn unpack(message: BoxedMessage) -> (BoxedMessage, Option<OwnedSemaphorePermit>) {
        match message.downcast::<Self>() {
            Ok(admitted) => (admitted.message, Some(admitted.permit)),
            Err(message) => (message, None),
        }
    }
}

#[derive(Debug)]
pub(super) struct Admission {
    bytes: Arc<Semaphore>,
    max_inflight: usize,
    max_request: usize,
    max_batch_payload: usize,
    idempotent: bool,
}
fn rejected(reason: &str) -> MailboxError {
    MailboxError::PushError(format!("cloud proxy admission: {reason}"))
}
impl Admission {
    pub(super) fn new(config: &CloudProxyConfig) -> Self {
        Self {
            bytes: Arc::new(Semaphore::new(config.max_inflight_bytes)),
            max_inflight: config.max_inflight_bytes,
            max_request: config.max_request_bytes,
            max_batch_payload: config.outbox_config().command_bytes as usize,
            idempotent: config.idempotent_submissions,
        }
    }
    pub(super) fn snapshot(&self) -> CloudProxyAdmissionStats {
        CloudProxyAdmissionStats {
            used_bytes: (self.max_inflight - self.bytes.available_permits()) as u64,
            max_inflight_bytes: self.max_inflight as u64,
            max_request_bytes: self.max_request as u64,
        }
    }
    fn reserve(&self, cost: usize) -> Result<OwnedSemaphorePermit, MailboxError> {
        if cost > self.max_request {
            return Err(rejected("request byte limit exceeded"));
        }
        self.bytes
            .clone()
            .try_acquire_many_owned(cost as u32)
            .map_err(|_| rejected("inflight byte limit exceeded"))
    }
    fn cost(messages: &[CloudProxySubmit]) -> Result<usize, MailboxError> {
        messages
            .iter()
            .try_fold(CLOUD_PROXY_REQUEST_OVERHEAD, |sum, m| {
                sum.checked_add(CLOUD_PROXY_RECORD_OVERHEAD)?
                    .checked_add(m.type_key.len())?
                    .checked_add(m.payload.len())
            })
            .ok_or_else(|| rejected("request size overflow"))
    }
    fn compact(message: &CloudProxySubmit) -> CloudProxySubmit {
        // Do not retain an arbitrarily large Bytes backing allocation or String
        // spare capacity on the strength of a tiny slice/length charge.
        CloudProxySubmit {
            type_key: message.type_key.as_str().into(),
            payload: bytes::Bytes::copy_from_slice(&message.payload),
        }
    }
}
impl MessageAdmission for Admission {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
        if message.is::<CloudProxyInspect>()
            || message.is::<CloudProxyInspectStorage>()
            || message.is::<CloudProxyInspectAdmission>()
            || message.is::<CloudProxyDrain>()
            || message.is::<ParrotAppHook>()
        {
            return Ok(message);
        }
        let (message, permit): (BoxedMessage, _) = if let Some(submit) =
            message.downcast_ref::<CloudProxySubmitIdempotent>()
        {
            if !self.idempotent {
                return Err(rejected("idempotent submissions are not enabled"));
            }
            if submit.request_id.is_empty()
                || submit.request_id.len() > 1024
                || submit.request_id.chars().any(char::is_control)
            {
                return Err(rejected("invalid submission ID"));
            }
            let cost = CLOUD_PROXY_REQUEST_OVERHEAD
                .checked_add(CLOUD_PROXY_RECORD_OVERHEAD)
                .and_then(|n| n.checked_add(submit.request_id.len()))
                .and_then(|n| n.checked_add(submit.type_key.len()))
                .and_then(|n| n.checked_add(submit.payload.len()))
                .ok_or_else(|| rejected("request size overflow"))?;
            let permit = self.reserve(cost)?;
            (
                Box::new(CloudProxySubmitIdempotent {
                    request_id: submit.request_id.as_str().into(),
                    type_key: submit.type_key.as_str().into(),
                    payload: bytes::Bytes::copy_from_slice(&submit.payload),
                }),
                permit,
            )
        } else if let Some(submit) = message.downcast_ref::<CloudProxySubmit>() {
            let permit = self.reserve(Self::cost(std::slice::from_ref(submit))?)?;
            (Box::new(Self::compact(submit)), permit)
        } else if let Some(batch) = message.downcast_ref::<CloudProxySubmitBatch>() {
            if batch.messages.is_empty() || batch.messages.len() > CLOUD_PROXY_MAX_BATCH_RECORDS {
                return Err(rejected("batch record limit exceeded or empty batch"));
            }
            let cost = Self::cost(&batch.messages)?;
            let payload = cost
                - CLOUD_PROXY_REQUEST_OVERHEAD
                - batch.messages.len() * CLOUD_PROXY_RECORD_OVERHEAD;
            if payload > self.max_batch_payload {
                return Err(rejected("batch payload limit exceeded"));
            }
            let permit = self.reserve(cost)?;
            (
                Box::new(CloudProxySubmitBatch {
                    messages: batch.messages.iter().map(Self::compact).collect(),
                }),
                permit,
            )
        } else {
            // Unknown codec output size: reserve the per-request maximum BEFORE
            // invoking it. User codecs remain trusted synchronous code, not a
            // memory sandbox. Oversize output/errors/panic release the reserve.
            let mut permit = self.reserve(self.max_request)?;
            let (type_key, payload) = CodecRegistry::global()
                .encode_outgoing(&message)
                .map_err(|e| rejected(&format!("business codec: {e:?}")))?;
            let submit = CloudProxySubmit {
                type_key,
                payload: payload.into(),
            };
            let cost = Self::cost(std::slice::from_ref(&submit))?;
            if cost > self.max_request {
                return Err(rejected("encoded request byte limit exceeded"));
            }
            drop(permit.split(self.max_request - cost));
            (Box::new(Self::compact(&submit)), permit)
        };
        Ok(Box::new(Admitted { message, permit }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(request: usize, total: usize) -> Admission {
        Admission::new(&CloudProxyConfig {
            max_request_bytes: request,
            max_inflight_bytes: total,
            ..Default::default()
        })
    }
    fn submit(n: usize) -> BoxedMessage {
        Box::new(CloudProxySubmit {
            type_key: "key".into(),
            payload: bytes::Bytes::from(vec![1; n]),
        })
    }
    #[test]
    fn exact_byte_boundaries_survive_processing_and_compact_large_backing_allocations() {
        let cost = CLOUD_PROXY_REQUEST_OVERHEAD + CLOUD_PROXY_RECORD_OVERHEAD + 3 + 5;
        let p = policy(cost, cost * 2);
        let backing = bytes::Bytes::from(vec![9; 1024 * 1024]);
        let mut key = String::with_capacity(1024 * 1024);
        key.push_str("key");
        let first = p
            .prepare(Box::new(CloudProxySubmit {
                type_key: key,
                payload: backing.slice(0..5),
            }))
            .unwrap();
        let prepared = Admitted::message(&first)
            .downcast_ref::<CloudProxySubmit>()
            .unwrap();
        assert_ne!(prepared.payload.as_ptr(), backing.as_ptr());
        assert_eq!(prepared.type_key.capacity(), 3);
        assert_eq!(&prepared.payload[..], &[9; 5]);
        assert!(p.prepare(submit(6)).is_err());
        let second = p.prepare(submit(5)).unwrap();
        assert_eq!(p.snapshot().used_bytes, (2 * cost) as u64);
        assert!(p.prepare(submit(0)).is_err());
        let (_, guard) = Admitted::unpack(first);
        assert!(
            p.prepare(submit(5)).is_err(),
            "processing still owns its charge"
        );
        drop(guard);
        let third = p.prepare(submit(5)).unwrap();
        drop((second, third));
        assert_eq!(p.snapshot().used_bytes, 0);
    }
    #[test]
    fn batch_charge_is_aggregate_and_controls_still_enter_when_business_budget_is_full() {
        let record = CloudProxySubmit {
            type_key: "key".into(),
            payload: bytes::Bytes::from_static(b"12345"),
        };
        let cost = CLOUD_PROXY_REQUEST_OVERHEAD + 2 * (CLOUD_PROXY_RECORD_OVERHEAD + 8);
        let p = policy(cost, cost);
        let batch = p
            .prepare(Box::new(CloudProxySubmitBatch {
                messages: vec![record.clone(), record.clone()],
            }))
            .unwrap();
        assert_eq!(p.snapshot().used_bytes, cost as u64);
        assert!(p.prepare(submit(0)).is_err());
        for control in [
            Box::new(CloudProxyInspect) as BoxedMessage,
            Box::new(CloudProxyInspectAdmission),
            Box::new(CloudProxyDrain),
            Box::new(ParrotAppHook(crate::hook_phase::DRAIN)),
        ] {
            assert!(p.prepare(control).is_ok());
        }
        drop(batch);
        for messages in [vec![], vec![record.clone(); 257], vec![record; 3]] {
            assert!(p
                .prepare(Box::new(CloudProxySubmitBatch { messages }))
                .is_err());
            assert_eq!(p.snapshot().used_bytes, 0);
        }
    }
    #[test]
    fn concurrent_producers_share_one_budget_without_overcommit() {
        let cost = CLOUD_PROXY_REQUEST_OVERHEAD + CLOUD_PROXY_RECORD_OVERHEAD + 8;
        let p = policy(cost, cost * 4);
        let ready = std::sync::Barrier::new(17);
        let release = std::sync::Barrier::new(17);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let (p, ready, release) = (&p, &ready, &release);
                scope.spawn(move || {
                    let result = p.prepare(submit(5));
                    ready.wait();
                    release.wait();
                    drop(result);
                });
            }
            ready.wait();
            assert_eq!(p.snapshot().used_bytes, (cost * 4) as u64);
            release.wait();
        });
        assert_eq!(p.snapshot().used_bytes, 0);
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Probe {
        policy: Arc<Admission>,
        calls: Arc<AtomicUsize>,
        mode: u8,
    }
    parrot_api::message::inventory::submit! {
        parrot_api::message::CodecRegistration {
            type_key: "bin:admission-reserve-probe#v1",
            type_id: std::any::TypeId::of::<Probe>(),
            encode: |message| {
                let p = message.downcast_ref::<Probe>().unwrap();
                p.calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(p.policy.snapshot().used_bytes, p.policy.max_request as u64);
                match p.mode {
                    0 => Ok(vec![1, 2, 3]),
                    1 => Err("codec failed".into()),
                    2 => panic!("codec panic"),
                    _ => Ok(vec![0; 1024]),
                }
            },
            decode: |_| Err("not a network probe".into()),
        }
    }
    #[test]
    fn codecs_reserve_before_invocation_and_release_on_error_panic_or_oversized_output() {
        let p = Arc::new(Admission::new(&CloudProxyConfig {
            max_request_bytes: 512,
            max_inflight_bytes: 512,
            ..Default::default()
        }));
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = |mode| {
            Box::new(Probe {
                policy: p.clone(),
                calls: calls.clone(),
                mode,
            }) as BoxedMessage
        };
        let full = p
            .prepare(Box::new(CloudProxySubmit {
                type_key: "key".into(),
                payload: bytes::Bytes::from(vec![0; 317]),
            }))
            .unwrap();
        assert!(p.prepare(probe(0)).is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "saturated policy must not invoke user codec"
        );
        drop(full);
        for mode in 1..=3 {
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.prepare(probe(mode))));
            if mode == 2 {
                assert!(outcome.is_err());
            } else {
                assert!(outcome.unwrap().is_err());
            }
            assert_eq!(p.snapshot().used_bytes, 0);
        }
        let accepted = p.prepare(probe(0)).unwrap();
        let message = Admitted::message(&accepted)
            .downcast_ref::<CloudProxySubmit>()
            .unwrap();
        assert_eq!(message.type_key, "bin:admission-reserve-probe#v1");
        assert_eq!(&message.payload[..], &[1, 2, 3]);
        assert_eq!(
            p.snapshot().used_bytes,
            (CLOUD_PROXY_REQUEST_OVERHEAD
                + CLOUD_PROXY_RECORD_OVERHEAD
                + message.type_key.len()
                + 3) as u64
        );
        drop(accepted);
        assert_eq!(p.snapshot().used_bytes, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
