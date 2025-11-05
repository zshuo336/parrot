//! A logical actor owns its policy independently of retained framework handles.
use super::MessageAdmission;
use crate::owned_payload::OwnedPayload;
use crate::thread::error::MailboxError;
use futures::FutureExt;
use parrot_api::types::{BoxedFuture, BoxedMessage};
use std::sync::{Arc, Mutex};

type Receipt = futures::future::Shared<BoxedFuture<'static, Result<(), String>>>;
struct State {
    policy: Option<OwnedPayload<Arc<dyn MessageAdmission>>>,
    active: usize,
    closed: bool,
}
struct Owner {
    state: Mutex<State>,
    retired: Receipt,
}
struct Lease {
    policy: Option<Arc<dyn MessageAdmission>>,
    owner: Arc<Owner>,
}

pub(crate) struct ManagedAdmission {
    owner: Arc<Owner>,
}
impl std::fmt::Debug for ManagedAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Formatting a framework config must not invoke a user policy's Debug.
        f.write_str("ManagedAdmission")
    }
}
impl ManagedAdmission {
    pub(crate) fn new(
        policy: Arc<dyn MessageAdmission>,
        runtime: &tokio::runtime::Handle,
        slots: &Arc<tokio::sync::Semaphore>,
    ) -> Arc<Self> {
        let policy = OwnedPayload::new(policy, runtime, slots);
        let retired = policy.receipt();
        // This receipt only contains OwnedPayload's synthesized disposal error;
        // business callback errors never pass through this conversion.
        let retired: BoxedFuture<'static, Result<(), String>> = Box::pin(async move {
            retired
                .await
                .map_err(|error| format!("message admission policy disposal failed: {error}"))
        });
        Arc::new(Self {
            owner: Arc::new(Owner {
                state: Mutex::new(State {
                    policy: Some(policy),
                    active: 0,
                    closed: false,
                }),
                retired: retired.shared(),
            }),
        })
    }

    /// Closing starts disposal without depending on the returned waiter. A
    /// callback already admitted keeps the raw policy alive until it returns.
    pub(crate) fn close(&self) -> Receipt {
        let policy = {
            let mut state = self.owner.state.lock().unwrap();
            state.closed = true;
            if state.active == 0 {
                state.policy.take()
            } else {
                None
            }
        };
        drop(policy);
        self.owner.retired.clone()
    }
}
impl MessageAdmission for ManagedAdmission {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
        let lease = {
            let mut state = self.owner.state.lock().unwrap();
            if state.closed {
                return Err(MailboxError::Closed);
            }
            let policy = state
                .policy
                .as_ref()
                .expect("open admission owner retains policy")
                .value()
                .clone();
            state.active += 1;
            Lease {
                policy: Some(policy),
                owner: self.owner.clone(),
            }
        };
        // AdmissionMailbox owns panic-to-rejection conversion. This lease also
        // protects direct calls through a retained processor's config, including
        // unwinding calls, without holding the state lock across user code.
        lease
            .policy
            .as_ref()
            .expect("live admission lease")
            .prepare(message)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        // Release this raw clone before publishing that the callback is gone.
        // The original owner still holds one, so this cannot run user Drop.
        drop(self.policy.take());
        let policy = {
            let mut state = self.owner.state.lock().unwrap();
            state.active -= 1;
            if state.closed && state.active == 0 {
                state.policy.take()
            } else {
                None
            }
        };
        // Host shutdown can make dispatch drop its capture synchronously. Keep
        // that independently protected cleanup outside the state lock as well.
        drop(policy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct BadPayload;
    impl Drop for BadPayload {
        fn drop(&mut self) {
            panic!("secondary admission payload destructor");
        }
    }
    struct Policy {
        drops: Arc<AtomicUsize>,
        callback: Option<Mutex<std::sync::mpsc::Receiver<()>>>,
        entered: Option<std::sync::mpsc::SyncSender<()>>,
    }
    impl std::fmt::Debug for Policy {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("framework must not format the raw policy");
        }
    }
    impl MessageAdmission for Policy {
        fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
            if let Some(entered) = &self.entered {
                entered.send(()).unwrap();
            }
            if let Some(callback) = &self.callback {
                callback
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
            }
            Ok(message)
        }
    }
    impl Drop for Policy {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(BadPayload);
        }
    }
    fn receipt_after_host_exit(receipt: Receipt) -> Result<(), String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(3), receipt)
                    .await
                    .unwrap()
            })
    }
    #[test]
    fn host_exit_finishes_queued_policy_disposal_and_retained_receipt() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let policy = ManagedAdmission::new(
            Arc::new(Policy {
                drops: drops.clone(),
                callback: None,
                entered: None,
            }),
            runtime.handle(),
            &Arc::new(tokio::sync::Semaphore::new(0)),
        );
        assert_eq!(format!("{policy:?}"), "ManagedAdmission");
        let receipt = policy.close();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(runtime);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let first = receipt_after_host_exit(receipt).unwrap_err();
        assert!(first.contains("panic payload destructor panicked"));
        assert_eq!(first, receipt_after_host_exit(policy.close()).unwrap_err());
        assert!(matches!(
            policy.prepare(Box::new(())),
            Err(MailboxError::Closed)
        ));
    }
    #[test]
    fn last_callback_after_host_exit_retires_policy_without_unwinding_producer() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let (started, entered) = std::sync::mpsc::sync_channel(1);
        let (release, callback) = std::sync::mpsc::channel();
        let policy = ManagedAdmission::new(
            Arc::new(Policy {
                drops: drops.clone(),
                callback: Some(Mutex::new(callback)),
                entered: Some(started),
            }),
            runtime.handle(),
            &Arc::new(tokio::sync::Semaphore::new(1)),
        );
        let producer = policy.clone();
        let thread = std::thread::spawn(move || producer.prepare(Box::new(42u64)));
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let receipt = policy.close();
        drop(runtime);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        let output = match thread.join() {
            Ok(result) => result.unwrap(),
            Err(payload) => panic!(
                "producer escaped: {}",
                crate::owned_payload::panic_message(payload)
            ),
        };
        assert_eq!(*output.downcast::<u64>().unwrap(), 42);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            receipt_after_host_exit(receipt)
                .unwrap_err()
                .contains("panic payload destructor panicked")
        );
    }
}
