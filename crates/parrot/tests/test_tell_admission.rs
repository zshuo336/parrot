use parrot::thread::{
    config::BackpressureStrategy,
    mailbox::{Mailbox, mpsc::MpscMailbox, spsc::SpscMailbox, spsc_ringbuf::SpscRingbufMailbox},
};
use parrot_api::address::ActorPath;

async fn dropped_message_is_not_success(mailbox: impl Mailbox) {
    mailbox
        .push(Box::new(1u64), BackpressureStrategy::Block)
        .await
        .unwrap();
    let result = mailbox
        .push(Box::new(2u64), BackpressureStrategy::DropNewest)
        .await;
    let first = mailbox.pop().await.unwrap();
    assert_eq!(*first.downcast::<u64>().unwrap(), 1);
    assert!(mailbox.is_empty().await);
    assert!(
        matches!(
            result,
            Err(parrot::thread::error::MailboxError::Dropped { capacity: 1 })
        ),
        "a discarded message must report Dropped"
    );
}
#[tokio::test]
async fn mpsc_drop_newest_reports_rejection() {
    dropped_message_is_not_success(MpscMailbox::new(1, ActorPath::placeholder("/mpsc"))).await;
}
#[tokio::test]
async fn spsc_drop_newest_reports_rejection() {
    dropped_message_is_not_success(SpscMailbox::new(1, ActorPath::placeholder("/spsc"))).await;
}
#[tokio::test]
async fn ring_drop_newest_reports_rejection() {
    dropped_message_is_not_success(SpscRingbufMailbox::new(1, ActorPath::placeholder("/ring")))
        .await;
}
