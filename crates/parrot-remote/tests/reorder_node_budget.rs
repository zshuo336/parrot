use bytes::Bytes;
use parrot_remote::{
    frame::Frame, ingress::Ingress, registry::CallbackRegistry, transport::FrameSender, LocalLookup,
};
use std::{sync::Arc, time::Duration};

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:budget-duplicate#v1",
        type_id: std::any::TypeId::of::<u8>(),
        encode: |message: &parrot_api::types::BoxedMessage| {
            Ok(vec![*message.downcast_ref::<u8>().ok_or("expected u8")?])
        },
        decode: |payload: &[u8]| {
            if payload.len() != 1 { return Err("expected one byte".into()); }
            Ok(Box::new(payload[0]) as parrot_api::types::BoxedMessage)
        },
    }
}

struct EmptyLookup;
#[async_trait::async_trait]
impl LocalLookup for EmptyLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}

struct Close(Arc<Ingress>);
impl Drop for Close {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn ingress() -> Close {
    let ingress = Arc::new(Ingress::new(
        Arc::new(EmptyLookup),
        Arc::new(CallbackRegistry::new(16)),
    ));
    // Retain genuine gaps without relying on scheduler speed or timer expiry.
    ingress.set_reorder_knobs(Duration::from_secs(3600), 1024);
    Close(ingress)
}

fn connection(node: &str) -> (FrameSender, tokio::sync::mpsc::Receiver<Frame>) {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    (FrameSender::new(tx, node), rx)
}

fn pending(sequence: u32, payload: Bytes) -> Frame {
    Frame::tell_with_seq("/budget", "bin:budget#v1", payload, sequence)
}

#[tokio::test]
async fn default_reorder_stream_limit_is_cumulative_across_sources() {
    let ingress = ingress();
    let mut connections = Vec::new();
    for index in 0..256 {
        let source = format!("source-{index}");
        let (back, rx) = connection(&source);
        ingress
            .0
            .dispatch(pending(2, Bytes::new()), &back, &source)
            .await;
        assert!(
            !back.is_closed(),
            "source within the default stream budget was rejected"
        );
        connections.push((back, rx));
    }
    let (overflow, _rx) = connection("overflow");
    ingress
        .0
        .dispatch(pending(2, Bytes::new()), &overflow, "overflow")
        .await;
    assert!(
        overflow.is_closed(),
        "a 257th live reorder source exceeded the node budget"
    );
    assert!(connections.iter().all(|(back, _)| !back.is_closed()));
    ingress.0.close();
    ingress.0.wait_closed().await;
}

#[tokio::test]
async fn default_reorder_frame_limit_includes_all_source_queues_and_buffers() {
    let ingress = ingress();
    let mut connections = Vec::new();
    for source in 0..32 {
        let source = format!("source-{source}");
        let (back, rx) = connection(&source);
        for sequence in 2..130 {
            ingress
                .0
                .dispatch(pending(sequence, Bytes::new()), &back, &source)
                .await;
            assert!(
                !back.is_closed(),
                "frame within the default node budget was rejected"
            );
        }
        connections.push((back, rx));
    }
    let overflow = &connections[0].0;
    ingress
        .0
        .dispatch(pending(130, Bytes::new()), overflow, "source-0")
        .await;
    assert!(
        overflow.is_closed(),
        "4097 pending frames across 32 sources exceeded the node budget"
    );
    assert!(connections[1..].iter().all(|(back, _)| !back.is_closed()));
    ingress.0.close();
    ingress.0.wait_closed().await;
}

#[tokio::test]
async fn default_reorder_byte_limit_includes_encoded_metadata_and_queued_payloads() {
    let ingress = ingress();
    let (back, _rx) = connection("large-source");
    let payload = Bytes::from(vec![1; 1024 * 1024]);
    let mut encoded = bytes::BytesMut::new();
    pending(2, payload.clone()).encode(&mut encoded).unwrap();
    let admitted = (64 * 1024 * 1024) / encoded.len();
    for index in 0..admitted {
        ingress
            .0
            .dispatch(
                pending(index as u32 + 2, payload.clone()),
                &back,
                "large-source",
            )
            .await;
        assert!(
            !back.is_closed(),
            "encoded bytes within the default node budget were rejected"
        );
    }
    ingress
        .0
        .dispatch(pending(admitted as u32 + 2, payload), &back, "large-source")
        .await;
    assert!(
        back.is_closed(),
        "retained reorder payloads exceeded the default 64 MiB node budget"
    );
    ingress.0.close();
    ingress.0.wait_closed().await;
}

#[derive(Clone, Debug)]
struct Recording(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
#[async_trait::async_trait]
impl LocalLookup for Recording {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        Some(Box::new(self.clone()))
    }
}
impl parrot_api::address::ActorRef for Recording {
    fn deliver<'a>(
        &'a self,
        message: parrot_api::types::BoxedMessage,
    ) -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<()>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .push(*message.downcast_ref::<u8>().unwrap());
            Ok(())
        })
    }
    fn send<'a>(
        &'a self,
        message: parrot_api::types::BoxedMessage,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        Box::pin(async { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: parrot_api::types::BoxedMessage,
        _: Option<Duration>,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        self.send(message)
    }
    fn stop<'a>(
        &'a self,
    ) -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/budget".into()
    }
    fn is_alive<'a>(&'a self) -> parrot_api::types::BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> parrot_api::types::BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[tokio::test]
async fn duplicate_sequence_cannot_replace_the_first_accepted_business_payload() {
    let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ingress = Close(Arc::new(Ingress::new(
        Arc::new(Recording(sink.clone())),
        Arc::new(CallbackRegistry::new(4)),
    )));
    let (back, _rx) = connection("duplicate-source");
    for (value, sequence) in [(2, 2), (9, 2), (1, 1)] {
        ingress
            .0
            .dispatch(
                Frame::tell_with_seq(
                    "/budget",
                    "bin:budget-duplicate#v1",
                    Bytes::from(vec![value]),
                    sequence,
                ),
                &back,
                "duplicate-source",
            )
            .await;
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while sink.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        *sink.lock().unwrap(),
        vec![1, 2],
        "a duplicate replaced previously accepted content"
    );
    ingress.0.close();
    ingress.0.wait_closed().await;
}
