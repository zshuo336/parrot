//! Bounded, cancellable preparation of an immutable legacy index history.
use crate::{checked_reply, shutdown::Stop, ActorRef, WsPrepareIndex, WsPrepareIndexR};
use std::{future::Future, time::Duration};

#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub ready: bool,
    pub verified: u64,
}

impl Progress {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 9 || bytes[0] > 1 {
            return Err("invalid index preparation receipt".into());
        }
        let verified = u64::from_le_bytes(bytes[1..].try_into().unwrap());
        if verified > 100001 {
            return Err("index preparation count exceeds storage budget".into());
        }
        Ok(Self {
            ready: bytes[0] == 1,
            verified,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = vec![u8::from(self.ready)];
        bytes.extend_from_slice(&self.verified.to_le_bytes());
        bytes
    }
}

pub(crate) async fn prepare(target: &dyn ActorRef, stop: &Stop) -> Result<bool, String> {
    wait_until_ready(stop, Duration::from_secs(600), || async {
        let WsPrepareIndexR(progress) = checked_reply(
            target,
            "prepare index history",
            Box::new(WsPrepareIndex(vec![])),
        )
        .await?;
        Ok(progress)
    })
    .await
}

// false means cancellation; the driver still retires the owning JVM component.
async fn wait_until_ready<F, Fut>(
    stop: &Stop,
    budget: Duration,
    mut poll: F,
) -> Result<bool, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Progress, String>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    let mut last_log = None;
    loop {
        if stop.requested() {
            return Ok(false);
        }
        let progress = tokio::select! {
            biased;
            _ = stop.wait() => return Ok(false),
            _ = tokio::time::sleep_until(deadline) => return Err("index history preparation deadline exceeded".into()),
            result = poll() => result?,
        };
        if progress.ready {
            return Ok(true);
        }
        let now = tokio::time::Instant::now();
        if last_log.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1)) {
            println!("[ws] 旧索引历史校验：已验证 {} 个快照", progress.verified);
            last_log = Some(now);
        }
        tokio::select! {
            biased;
            _ = stop.wait() => return Ok(false),
            _ = tokio::time::sleep_until(deadline) => return Err("index history preparation deadline exceeded".into()),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_remote::codec_registry::CodecRegistry;

    #[test]
    fn readiness_wire_requires_complete_bounded_receipts() {
        let registry = CodecRegistry::global();
        let key = "bin:ws/PrepareIndexR";
        for ready in [false, true] {
            for verified in [0, 1, 100000, 100001] {
                let bytes = Progress { ready, verified }.encode();
                let message = registry.decode_incoming(key, &bytes).unwrap();
                assert_eq!(registry.encode_outgoing(&message).unwrap().1, bytes);
                for n in 0..9 {
                    assert!(registry.decode_incoming(key, &bytes[..n]).is_err());
                }
                let mut extra = bytes;
                extra.push(0);
                assert!(registry.decode_incoming(key, &extra).is_err());
            }
        }
        for flag in [2, 255] {
            let mut bytes = vec![0; 9];
            bytes[0] = flag;
            assert!(registry.decode_incoming(key, &bytes).is_err());
        }
        for verified in [100002, u64::MAX] {
            assert!(registry
                .decode_incoming(
                    key,
                    &Progress {
                        ready: true,
                        verified
                    }
                    .encode()
                )
                .is_err());
        }
    }

    #[tokio::test]
    async fn pending_is_polled_until_ready_and_errors_propagate() {
        let mut calls = 0;
        assert!(wait_until_ready(&Stop::new(), Duration::from_secs(2), || {
            calls += 1;
            std::future::ready(Ok(Progress {
                ready: calls == 3,
                verified: calls,
            }))
        })
        .await
        .unwrap());
        assert_eq!(calls, 3);
        let error = wait_until_ready(&Stop::new(), Duration::from_secs(1), || {
            std::future::ready(Err("corrupt ancestor".to_string()))
        })
        .await
        .unwrap_err();
        assert_eq!(error, "corrupt ancestor");
    }

    #[tokio::test]
    async fn preparation_deadline_bounds_stalled_rpc_and_pending_replies() {
        let error = wait_until_ready(
            &Stop::new(),
            Duration::from_millis(20),
            std::future::pending,
        )
        .await
        .unwrap_err();
        assert!(error.contains("deadline"));
        let error = wait_until_ready(&Stop::new(), Duration::from_millis(20), || {
            std::future::ready(Ok(Progress {
                ready: false,
                verified: 0,
            }))
        })
        .await
        .unwrap_err();
        assert!(error.contains("deadline"));
    }

    #[tokio::test]
    async fn stop_cancels_inflight_preparation_and_prevents_late_poll() {
        let stop = Stop::new();
        let (result, ()) = tokio::join!(
            wait_until_ready(&stop, Duration::from_secs(1), std::future::pending),
            async {
                tokio::task::yield_now().await;
                stop.request();
            },
        );
        assert!(!result.unwrap());
        let mut calls = 0;
        assert!(!wait_until_ready(&stop, Duration::from_secs(1), || {
            calls += 1;
            std::future::ready(Ok(Progress {
                ready: true,
                verified: 0,
            }))
        })
        .await
        .unwrap());
        assert_eq!(calls, 0);
    }
}
