#[path = "support/processor_inputs.rs"]
mod support;

use parrot::thread::{
    ActorProcessor, BackpressureStrategy, ProcessorInterface, SchedulingMode, ThreadActorConfig,
    mailbox::{Mailbox, mpsc::MpscMailbox},
};
use parrot_api::address::ActorPath;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, atomic::Ordering},
};
use support::{Counts, Policy, inputs, invalid_configs, panic_text};

#[tokio::test]
async fn legacy_constructor_rejects_invalid_overrides_and_retires_unstarted_inputs() {
    for (key, mut config) in invalid_configs() {
        let counts = Arc::new(Counts::default());
        config.message_admission = Some(Arc::new(Policy {
            counts: counts.clone(),
            panic_drop: false,
        }));
        let (actor, context, context_owner) = inputs(&counts, false, false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            ActorProcessor::new(actor, context, "/raw-processor".into(), config)
        }));
        let rejection = match result {
            Ok(processor) => {
                drop(processor);
                None
            }
            Err(payload) => Some(panic_text(payload.as_ref()).to_owned()),
        };
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(counts.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(counts.policy_drops.load(Ordering::SeqCst), 1);
        assert!(context_owner.upgrade().is_none());
        assert!(
            rejection
                .as_deref()
                .is_some_and(|message| message.contains(key)),
            "processor constructor accepted invalid override {key}: {rejection:?}"
        );
    }
}

#[tokio::test]
async fn a_direct_processor_cannot_accept_a_budget_that_makes_no_progress() {
    let counts = Arc::new(Counts::default());
    let (actor, context, _) = inputs(&counts, false, false);
    let result = catch_unwind(AssertUnwindSafe(|| {
        ActorProcessor::new(
            actor,
            context,
            "/raw-processor".into(),
            ThreadActorConfig {
                scheduling_mode: Some(SchedulingMode::SharedPool {
                    max_messages_per_run: 0,
                }),
                ..Default::default()
            },
        )
    }));
    let processor = match result {
        Err(payload) => {
            assert!(
                panic_text(payload.as_ref()).contains("actor.scheduling_mode.max_messages_per_run")
            );
            assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
            assert_eq!(counts.actor_drops.load(Ordering::SeqCst), 1);
            return;
        }
        Ok(processor) => Arc::new(processor),
    };
    let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(
        1,
        ActorPath::placeholder("/raw-processor"),
    ));
    mailbox
        .push(Box::new(7_u64), BackpressureStrategy::Error)
        .await
        .unwrap();
    processor
        .clone()
        .initialize_and_start_erased()
        .await
        .unwrap();
    let processed = processor
        .process_batch_of_messages(&mailbox, 2, false)
        .await
        .unwrap();
    let remaining = mailbox.len().await;
    processor.clone().stop_erased().await.unwrap();
    drop(processor);
    panic!("processor accepted zero turn budget: processed={processed:?}, remaining={remaining}");
}
