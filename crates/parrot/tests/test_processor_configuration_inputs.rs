#[path = "support/processor_inputs.rs"]
mod support;

use parrot::thread::{
    ActorProcessor, BackpressureStrategy, ProcessorInterface, SchedulingMode, SystemError,
    ThreadActorConfig,
    mailbox::{Mailbox, mpsc::MpscMailbox},
};
use parrot_api::address::ActorPath;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, atomic::Ordering},
};
use support::{Counts, Policy, inputs, invalid_configs, panic_text};

#[tokio::test]
async fn fallible_constructor_reports_configuration_and_retires_every_input() {
    for (key, mut config) in invalid_configs() {
        let counts = Arc::new(Counts::default());
        config.message_admission = Some(Arc::new(Policy {
            counts: counts.clone(),
            panic_drop: false,
        }));
        let (actor, context, context_owner) = inputs(&counts, false, false);
        let tasks = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        let result = ActorProcessor::try_new(actor, context, "/raw-processor".into(), config);
        assert!(
            matches!(result, Err(SystemError::ConfigError(ref message)) if message.contains(key))
        );
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(counts.actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(counts.policy_drops.load(Ordering::SeqCst), 1);
        assert!(context_owner.upgrade().is_none());
        assert_eq!(
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks(),
            tasks
        );
    }
}

#[tokio::test]
async fn valid_fallible_construction_keeps_explicit_budgets_fifo_and_real_batching() {
    for coalesce in [false, true] {
        let counts = Arc::new(Counts::default());
        let (actor, context, context_owner) = inputs(&counts, false, coalesce);
        let processor = Arc::new(
            ActorProcessor::try_new(
                actor,
                context,
                "/raw-processor".into(),
                ThreadActorConfig {
                    scheduling_mode: Some(SchedulingMode::SharedPool {
                        max_messages_per_run: 2,
                    }),
                    message_batch_size: Some(if coalesce { 3 } else { 1 }),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert!(context_owner.upgrade().is_some());
        processor
            .clone()
            .initialize_and_start_erased()
            .await
            .unwrap();
        let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(
            5,
            ActorPath::placeholder("/raw-processor"),
        ));
        for value in 0..5_u64 {
            mailbox
                .push(Box::new(value), BackpressureStrategy::Error)
                .await
                .unwrap();
        }
        for (processed, remaining) in [(2, 3), (2, 1), (1, 0)] {
            assert_eq!(
                processor
                    .process_batch_of_messages(&mailbox, 4, false)
                    .await
                    .unwrap(),
                (processed, 0)
            );
            assert_eq!(mailbox.len().await, remaining);
        }
        assert_eq!(*counts.seen.lock().unwrap(), vec![0, 1, 2, 3, 4]);
        assert_eq!(
            *counts.batches.lock().unwrap(),
            if coalesce { vec![2, 2, 1] } else { vec![] }
        );
        processor.clone().stop_erased().await.unwrap();
        processor.clone().retire_erased().await.unwrap();
        assert!(context_owner.upgrade().is_none());
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(counts.actor_drops.load(Ordering::SeqCst), 1);
        drop(processor);
    }
}

#[tokio::test]
async fn rejected_inputs_isolate_actor_and_policy_disposal_panics_for_both_constructors() {
    if std::env::var_os("PARROT_PROCESSOR_REJECTION_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "rejected_inputs_isolate_actor_and_policy_disposal_panics_for_both_constructors",
                "--exact",
                "--nocapture",
            ])
            .env("PARROT_PROCESSOR_REJECTION_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "processor rejection child failed: {:?}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("processor rejection cleanup complete")
        );
        return;
    }
    for legacy in [false, true] {
        for (actor_panic, policy_panic) in [(true, false), (false, true), (true, true)] {
            let counts = Arc::new(Counts::default());
            let (actor, context, context_owner) = inputs(&counts, actor_panic, false);
            let config = ThreadActorConfig {
                mailbox_capacity: Some(0),
                message_admission: Some(Arc::new(Policy {
                    counts: counts.clone(),
                    panic_drop: policy_panic,
                })),
                ..Default::default()
            };
            let rejected = if legacy {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    ActorProcessor::new(actor, context, "/raw-processor".into(), config)
                }));
                let Err(payload) = result else {
                    panic!("legacy constructor accepted invalid input")
                };
                panic_text(payload.as_ref()).to_owned()
            } else {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    ActorProcessor::try_new(actor, context, "/raw-processor".into(), config)
                }));
                let Ok(Err(SystemError::Panic(message))) = result else {
                    panic!("cleanup failure must remain a panic receipt")
                };
                message
            };
            assert!(rejected.contains("actor.mailbox_capacity"), "{rejected}");
            assert_eq!(
                rejected.contains("processor actor disposal panic"),
                actor_panic,
                "{rejected}"
            );
            assert_eq!(
                rejected.contains("processor policy disposal panic"),
                policy_panic,
                "{rejected}"
            );
            assert_eq!(counts.actor_drops.load(Ordering::SeqCst), 1);
            assert_eq!(counts.policy_drops.load(Ordering::SeqCst), 1);
            assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
            assert!(context_owner.upgrade().is_none());
        }
    }
    println!("processor rejection cleanup complete");
}
