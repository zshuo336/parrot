//! Test the public derive path, including externally specified wire bytes.
use parrot_api::message::{CodecRegistration, RemoteMessage};
use parrot_api::types::BoxedMessage;
use parrot_api_derive::RemoteMessage;
use parrot_remote::codec_registry::CodecRegistry;
use std::any::TypeId;

mod left {
    #[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
    pub struct Event(pub u64);
    impl parrot_api::Message for Event {
        type Result = ();
    }
}
mod right {
    #[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
    pub struct Event(pub String);
    impl parrot_api::Message for Event {
        type Result = ();
    }
}

#[derive(Clone, PartialEq, prost::Message, RemoteMessage)]
#[remote(key = "pb:parrot.contract.Record")]
struct Record {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(string, tag = "2")]
    name: String,
}
impl parrot_api::Message for Record {
    type Result = ();
}

fn registration<T: 'static>() -> &'static CodecRegistration {
    inventory::iter::<CodecRegistration>
        .into_iter()
        .find(|reg| reg.type_id == TypeId::of::<T>())
        .expect("derive must register its concrete type")
}

#[test]
fn default_keys_include_expansion_module() {
    assert_eq!(
        left::Event::TYPE_KEY,
        "bin:derived_codec_contract::left::Event#v1"
    );
    assert_eq!(
        right::Event::TYPE_KEY,
        "bin:derived_codec_contract::right::Event#v1"
    );
}

#[test]
fn same_named_types_coexist_in_global_registry() {
    let registry = CodecRegistry::global();
    for msg in [
        Box::new(left::Event(42)) as BoxedMessage,
        Box::new(right::Event("right".into())),
    ] {
        let (key, payload) = registry.encode_outgoing(&msg).unwrap();
        let decoded = registry.decode_incoming(&key, &payload).unwrap();
        if let Some(left) = decoded.downcast_ref::<left::Event>() {
            assert_eq!(left.0, 42);
        } else {
            assert_eq!(decoded.downcast_ref::<right::Event>().unwrap().0, "right");
        }
    }
}

#[test]
fn protobuf_derive_matches_external_wire_vector() {
    // protobuf: field 1 varint(150), field 2 length-delimited UTF-8("ok").
    // A symmetric local roundtrip would not detect a mislabeled bincode codec.
    let wire = [0x08, 0x96, 0x01, 0x12, 0x02, b'o', b'k'];
    let value = Record {
        id: 150,
        name: "ok".into(),
    };
    let boxed: BoxedMessage = Box::new(value.clone());
    let reg = registration::<Record>();
    assert_eq!((reg.encode)(&boxed).unwrap(), wire);
    let decoded = (reg.decode)(&wire).unwrap();
    assert_eq!(decoded.downcast_ref::<Record>().unwrap(), &value);
}

#[test]
fn bincode_derive_rejects_unconsumed_suffix() {
    let reg = registration::<left::Event>();
    let value: BoxedMessage = Box::new(left::Event(42));
    let mut wire = (reg.encode)(&value).unwrap();
    assert!((reg.decode)(&wire).is_ok());
    wire.push(0);
    assert!(
        (reg.decode)(&wire).is_err(),
        "trailing bytes must not be silently ignored"
    );
}

#[derive(serde::Serialize, serde::Deserialize, RemoteMessage)]
#[remote(key = "bin:stable_contract::Explicit#v2")]
struct Explicit(u64);
impl parrot_api::Message for Explicit {
    type Result = ();
}

#[test]
fn explicit_identity_and_downcast_errors_are_preserved() {
    assert_eq!(Explicit::TYPE_KEY, "bin:stable_contract::Explicit#v2");
    let registry = CodecRegistry::global();
    let value: BoxedMessage = Box::new(Explicit(999));
    let (key, wire) = registry.encode_outgoing(&value).unwrap();
    assert_eq!(key, Explicit::TYPE_KEY);
    let back = registry.decode_incoming(&key, &wire).unwrap();
    assert_eq!(back.downcast_ref::<Explicit>().unwrap().0, 999);
    assert!((registration::<Record>().encode)(&value).is_err());
    assert!((registration::<left::Event>().encode)(&value).is_err());
}

#[test]
fn protobuf_accepts_unknown_fields_but_rejects_corrupt_payloads() {
    let decode = registration::<Record>().decode;
    // Unknown field 3 is legal protobuf schema evolution, not garbage suffix.
    let wire = [0x08, 0x96, 0x01, 0x12, 0x02, b'o', b'k', 0x18, 0x01];
    let decoded = decode(&wire).unwrap();
    assert_eq!(
        decoded.downcast_ref::<Record>().unwrap(),
        &Record {
            id: 150,
            name: "ok".into()
        }
    );
    for invalid in [
        &[0x00][..],
        &[0x08, 0x80],
        &[0x12, 0x02, b'o'],
        &[0x12, 0x01, 0xff],
    ] {
        assert!(
            decode(invalid).is_err(),
            "accepted corrupt payload {invalid:?}"
        );
    }
    assert_eq!(
        decode(&[]).unwrap().downcast_ref::<Record>().unwrap(),
        &Record::default()
    );
}

#[derive(Debug, Clone)]
struct Echo;
use parrot_api::address::ActorRef;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture};
use std::{sync::Arc, time::Duration};

#[async_trait::async_trait]
impl ActorRef for Echo {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("this test requires a decoded request and encoded reply")
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/user/echo".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for Echo {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/user/echo").then(|| self.clone_boxed())
    }
}

#[tokio::test]
async fn derived_bin_and_protobuf_messages_cross_real_tcp_in_both_directions() {
    use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
    let config = |name| {
        let mut config = RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap()));
        config.extra_caps = parrot_remote::handshake::caps::PB;
        config
    };
    let source = RemoteActorSystem::new(config("derive-source"), Arc::new(Echo)).unwrap();
    let target = RemoteActorSystem::new(config("derive-target"), Arc::new(Echo)).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        source.start().await.unwrap();
        target.start().await.unwrap();
        source
            .connect(&NodeAddr::tcp(
                "derive-target",
                target.local_addr().unwrap(),
            ))
            .await
            .unwrap();
        for (sender, destination) in [(&source, "derive-target"), (&target, "derive-source")] {
            let actor = sender
                .remote_ref(&format!("parrot://{destination}/user/echo"))
                .unwrap();
            let left = actor.send(Box::new(left::Event(987))).await.unwrap();
            assert_eq!(left.downcast_ref::<left::Event>().unwrap().0, 987);
            let right = actor
                .send(Box::new(right::Event("另一模块".into())))
                .await
                .unwrap();
            assert_eq!(right.downcast_ref::<right::Event>().unwrap().0, "另一模块");
            let original = Record {
                id: u64::MAX,
                name: "跨语言 schema".into(),
            };
            let reply = actor.send(Box::new(original.clone())).await.unwrap();
            assert_eq!(reply.downcast_ref::<Record>().unwrap(), &original);
            assert!(sender.callbacks.is_empty());
        }
    })
    .await;
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
    outcome.expect("TCP codec exchange must finish within its deadline");
}

#[allow(dead_code)]
mod shadowed_prelude {
    pub struct Result;
    pub struct Vec;
    pub struct String;
    pub struct Box;
    #[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
    pub struct Event(pub u64);
    impl parrot_api::Message for Event {
        type Result = ();
    }
}

#[test]
fn derive_is_hygienic_when_user_types_shadow_the_prelude() {
    let value: BoxedMessage = Box::new(shadowed_prelude::Event(123));
    let registry = CodecRegistry::global();
    let (key, wire) = registry.encode_outgoing(&value).unwrap();
    assert_eq!(
        key,
        "bin:derived_codec_contract::shadowed_prelude::Event#v1"
    );
    let decoded = registry.decode_incoming(&key, &wire).unwrap();
    assert_eq!(
        decoded.downcast_ref::<shadowed_prelude::Event>().unwrap().0,
        123
    );
}

#[derive(serde::Serialize, serde::Deserialize, RemoteMessage)]
struct BoundedUnits(Vec<()>);
impl parrot_api::Message for BoundedUnits {
    type Result = Self;
}

#[tokio::test]
async fn hostile_bincode_payloads_return_codec_errors_and_keep_tcp_usable() {
    use futures::{FutureExt, SinkExt, StreamExt};
    use parrot_remote::{
        frame::{frame_type, Frame, MAX_FRAME_LEN},
        handshake::{caps, HandshakeBody, TopologyRole},
        transport::FrameCodec,
        ErrCode, RemoteActorSystem, RemoteConfig,
    };
    let config = RemoteConfig::tcp("decode-budget-server", Some("127.0.0.1:0".parse().unwrap()));
    let server = RemoteActorSystem::new(config, Arc::new(Echo)).unwrap();
    let outcome =
        std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(10), async {
            server.start().await.unwrap();
            let socket = tokio::net::TcpStream::connect(server.local_addr().unwrap())
                .await
                .unwrap();
            let mut peer = tokio_util::codec::Framed::new(socket, FrameCodec);
            let hello = HandshakeBody {
                node_id: "hostile-codec-client".into(),
                realm: None,
                cluster: None,
                capabilities: caps::BIN,
                max_frame_len: MAX_FRAME_LEN,
                topology_role: TopologyRole::Normal,
                hop_limit: 8,
                direct_addr: None,
            };
            let mut payload = bytes::BytesMut::new();
            hello.encode_tlv(&mut payload).unwrap();
            peer.send(Frame::handshake(payload.freeze())).await.unwrap();
            assert_eq!(
                peer.next().await.unwrap().unwrap().header.frame_type,
                frame_type::HANDSHAKE_ACK
            );
            let mut impossible_string = vec![253];
            impossible_string.extend_from_slice(&u64::MAX.to_le_bytes());
            // A bounded-size regression input also terminates on the old decoder;
            // the effectively infinite and stack-overflow cases run in API children.
            let over_budget = parrot_api::message::serde_remote_serialize(
                &((parrot_api::message::RemoteDecodeLimits::default().max_values + 1) as u64),
            )
            .unwrap();
            for (index, (key, bytes)) in [
                (BoundedUnits::TYPE_KEY, over_budget),
                (right::Event::TYPE_KEY, impossible_string),
            ]
            .into_iter()
            .enumerate()
            {
                let cid = (index as u64 + 1) * 10;
                peer.send(Frame::ask(
                    cid,
                    "/user/echo",
                    key,
                    bytes.clone().into(),
                    None,
                ))
                .await
                .unwrap();
                let error = next_business_frame(&mut peer).await;
                assert_eq!(error.header.frame_type, frame_type::REPLY_ERR);
                assert_eq!(error.header.correlation_id, cid);
                assert_eq!(
                    parrot_remote::error::decode_err_payload(&error.payload)
                        .unwrap()
                        .0,
                    ErrCode::CodecError
                );
                // A failed tell must be rejected before the Echo's panic-on-deliver
                // handler. The following ask proves that the same socket still works.
                peer.send(Frame::tell("/user/echo", key, bytes.into()))
                    .await
                    .unwrap();
                let good = parrot_api::message::serde_remote_serialize(&left::Event(cid)).unwrap();
                peer.send(Frame::ask(
                    cid + 1,
                    "/user/echo",
                    left::Event::TYPE_KEY,
                    good.into(),
                    None,
                ))
                .await
                .unwrap();
                let reply = next_business_frame(&mut peer).await;
                assert_eq!(reply.header.frame_type, frame_type::REPLY);
                assert_eq!(reply.header.correlation_id, cid + 1);
                let decoded = CodecRegistry::global()
                    .decode_incoming(&reply.type_key, &reply.payload)
                    .unwrap();
                assert_eq!(decoded.downcast_ref::<left::Event>().unwrap().0, cid);
            }
        }))
        .catch_unwind()
        .await;
    tokio::time::timeout(Duration::from_secs(5), server.shutdown())
        .await
        .unwrap()
        .unwrap();
    match outcome {
        Ok(result) => result.expect("hostile payload processing must finish within its deadline"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

async fn next_business_frame(
    peer: &mut tokio_util::codec::Framed<
        tokio::net::TcpStream,
        parrot_remote::transport::FrameCodec,
    >,
) -> parrot_remote::frame::Frame {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::frame::{frame_type, Frame};
    loop {
        let frame = peer.next().await.unwrap().unwrap();
        match frame.header.frame_type {
            frame_type::HEARTBEAT => peer.send(Frame::heartbeat_ack()).await.unwrap(),
            frame_type::HEARTBEAT_ACK => {}
            _ => return frame,
        }
    }
}
