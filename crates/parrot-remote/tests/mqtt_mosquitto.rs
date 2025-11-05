//! Required by the explicit Linux broker gate. No missing-tool silent success.
use bytes::{Bytes, BytesMut};
use parrot_remote::{
    durable::*, frame::Frame, mqtt::*, LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use rcgen::{CertificateParams, DnType, Issuer, KeyPair};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("mqtt-mosquitto-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Broker(Child);
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn broker(root: &Path, port: u16) -> Broker {
    let conf = root.join("mosquitto.conf");
    std::fs::write(&conf, format!("listener {port} 127.0.0.1\nallow_anonymous false\nrequire_certificate true\nuse_identity_as_username true\ncafile {0}/ca.pem\ncertfile {0}/server.pem\nkeyfile {0}/server.key\nacl_file {0}/acl\npersistence false\n", root.display())).unwrap();
    let log = std::fs::File::create(root.join("mosquitto.log")).unwrap();
    Broker(
        Command::new(
            std::env::var_os("PARROT_MOSQUITTO_BIN").unwrap_or_else(|| "mosquitto".into()),
        )
        .args(["-c", conf.to_str().unwrap()])
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("Mosquitto is mandatory for this explicit integration gate"),
    )
}
struct Identity {
    cert: Vec<u8>,
    key: Vec<u8>,
}
fn certificates(root: &Path) -> (Vec<u8>, Identity, Identity, Identity) {
    let mut ca = CertificateParams::default();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let key = KeyPair::generate().unwrap();
    let cert = ca.self_signed(&key).unwrap();
    let ca_pem = cert.pem().into_bytes();
    let issuer = Issuer::new(ca, key);
    let leaf = |name: &str| {
        let mut params = CertificateParams::new(vec!["127.0.0.1".into()]).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let key = KeyPair::generate().unwrap();
        Identity {
            cert: params.signed_by(&key, &issuer).unwrap().pem().into_bytes(),
            key: key.serialize_pem().into_bytes(),
        }
    };
    let server = leaf("broker");
    std::fs::write(root.join("ca.pem"), &ca_pem).unwrap();
    std::fs::write(root.join("server.pem"), server.cert).unwrap();
    std::fs::write(root.join("server.key"), server.key).unwrap();
    std::fs::write(root.join("acl"), "user bridge\ntopic read $parrot/edge/device-a\ntopic write $parrot-receipts/edge/device-a\nuser producer\ntopic write $parrot/edge/device-a\ntopic read $parrot-receipts/edge/device-a\nuser foreign\ntopic read $parrot-receipts/edge/device-a\n").unwrap();
    // Mosquitto drops privileges when started as root inside the isolated test
    // container. These are ephemeral test keys, never deployment credentials.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["ca.pem", "server.pem", "server.key", "acl"] {
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
    }
    (ca_pem, leaf("bridge"), leaf("producer"), leaf("foreign"))
}
fn options(port: u16, id: &str, ca: &[u8], identity: &Identity) -> MqttOptions {
    let mut opts = MqttOptions::new(id, "127.0.0.1", port);
    opts.set_transport(rumqttc::Transport::tls(
        ca.to_vec(),
        Some((identity.cert.clone(), identity.key.clone())),
        None,
    ));
    opts.set_keep_alive(Duration::from_secs(5));
    opts
}
struct Lookup(Option<Arc<dyn DurableReceiver>>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
    fn durable_receiver(&self, path: &str) -> Option<Arc<dyn DurableReceiver>> {
        if path == "/sink" {
            self.0.clone()
        } else {
            None
        }
    }
}
async fn remote(id: &str, receiver: Option<Arc<dyn DurableReceiver>>) -> Arc<RemoteActorSystem> {
    let r = RemoteActorSystem::new(RemoteConfig::mem(id), Arc::new(Lookup(receiver))).unwrap();
    r.start().await.unwrap();
    r
}
struct Client {
    client: AsyncClient,
    receipts: tokio::sync::mpsc::Receiver<Bytes>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn client(options: MqttOptions, topic: String) -> Client {
    let (client, mut events) = AsyncClient::new(options, 8);
    client.subscribe(topic, QoS::AtLeastOnce).await.unwrap();
    let (tx, receipts) = tokio::sync::mpsc::channel(8);
    let (ready, wait) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut ready = Some(ready);
        loop {
            match events.poll().await.expect("real broker client error") {
                Event::Incoming(Packet::SubAck(ack)) => {
                    assert!(ack
                        .return_codes
                        .iter()
                        .all(|code| matches!(code, rumqttc::SubscribeReasonCode::Success(_))));
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                }
                Event::Incoming(Packet::Publish(p)) => {
                    tx.send(p.payload).await.unwrap();
                }
                _ => {}
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap();
    Client {
        client,
        receipts,
        task,
    }
}
fn frame(target: &str, seq: u64, value: u64) -> Bytes {
    let e = DurableEnvelope {
        source: "device-a".into(),
        session: "boot-1".into(),
        receiver: target.into(),
        path: "/sink".into(),
        sequence: seq,
        type_key: "counter:add#v1".into(),
        payload: Bytes::copy_from_slice(&value.to_le_bytes()),
    };
    let f = Frame::tell_with_ack(
        seq + 100,
        &e.target_path(),
        DURABLE_KEY,
        e.encode().unwrap(),
    );
    let mut bytes = BytesMut::new();
    f.encode(&mut bytes).unwrap();
    bytes.freeze()
}
async fn receive(client: &mut Client, seq: u64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let bytes = client.receipts.recv().await.unwrap();
            let mut bytes = BytesMut::from(bytes.as_ref());
            let f = Frame::decode(&mut bytes).unwrap().unwrap();
            assert!(bytes.is_empty());
            assert_eq!(f.type_key, DURABLE_RECEIPT_KEY);
            if f.header.correlation_id == seq + 100 {
                assert_eq!(f.payload.as_ref(), seq.to_le_bytes());
                break;
            }
            // Persistent MQTT sessions may redeliver a previous receipt after
            // reconnect. It must be ignored by the producer correlation layer.
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires owned Mosquitto executable; mandatory scripts/check_mqtt_bridge.sh --broker gate"]
async fn real_mosquitto_mtls_acl_and_bridge_reopen_preserve_business_commit() {
    let root = Directory::new();
    let (ca, bridge_id, producer_id, foreign_id) = certificates(&root.0);
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mut broker = broker(&root.0, port);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                broker.0.try_wait().unwrap().is_none(),
                "broker exited: {}",
                std::fs::read_to_string(root.0.join("mosquitto.log")).unwrap()
            );
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let target_id = format!("receiver-{}", uuid::Uuid::new_v4());
    let source = remote(&format!("source-{}", uuid::Uuid::new_v4()), None).await;
    let mut inbox = TransactionalInbox::open(
        root.0.join("inbox"),
        &target_id,
        "/sink",
        Bytes::copy_from_slice(&0u64.to_le_bytes()),
        InboxLimits::default(),
        |state, e| {
            if e.type_key != "counter:add#v1" || e.payload.len() != 8 {
                return Err("invalid operation".into());
            }
            let next = u64::from_le_bytes(state.try_into().unwrap())
                + u64::from_le_bytes(e.payload[..].try_into().unwrap());
            Ok(Bytes::copy_from_slice(&next.to_le_bytes()))
        },
    )
    .await
    .unwrap();
    let receiver = remote(&target_id, Some(Arc::new(inbox.handle()))).await;
    let cfg = BridgeConfig {
        mqtt: options(port, "bridge", &ca, &bridge_id),
        key: "edge/device-a".into(),
        producer: "device-a".into(),
        session: "boot-1".into(),
        receiver: NodeAddr::mem(&target_id),
        path: "/sink".into(),
        directory: root.0.join("outbox"),
        outbox: OutboxConfig {
            transactional: true,
            idempotent_submissions: true,
            ..Default::default()
        },
        max_frame_bytes: 4096,
        queue_capacity: 4,
        queue_bytes: 65536,
        retry_min: Duration::from_millis(20),
        retry_max: Duration::from_millis(100),
        insecure_development: false,
    };
    let bridge = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), bridge.handle().ready())
        .await
        .unwrap()
        .unwrap();
    let mut producer = client(
        options(port, "producer", &ca, &producer_id),
        cfg.receipt_topic(),
    )
    .await;
    let foreign = client(
        options(port, "foreign", &ca, &foreign_id),
        cfg.receipt_topic(),
    )
    .await;
    foreign
        .client
        .publish_bytes(
            cfg.topic(),
            QoS::AtLeastOnce,
            false,
            frame(&target_id, 99, 1000),
        )
        .await
        .unwrap();
    producer
        .client
        .publish_bytes(
            cfg.topic(),
            QoS::AtLeastOnce,
            false,
            frame(&target_id, 1, 7),
        )
        .await
        .unwrap();
    receive(&mut producer, 1).await;
    assert_eq!(inbox.handle().snapshot().revision, 1);
    bridge.close().await.unwrap();
    drop(bridge);
    producer
        .client
        .publish_bytes(
            cfg.topic(),
            QoS::AtLeastOnce,
            false,
            frame(&target_id, 2, 9),
        )
        .await
        .unwrap();
    let reopened = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    receive(&mut producer, 2).await;
    producer
        .client
        .publish_bytes(
            cfg.topic(),
            QoS::AtLeastOnce,
            false,
            frame(&target_id, 1, 7),
        )
        .await
        .unwrap();
    receive(&mut producer, 1).await;
    assert_eq!(inbox.handle().snapshot().revision, 2);
    assert_eq!(
        u64::from_le_bytes(inbox.handle().snapshot().state[..].try_into().unwrap()),
        16
    );
    assert_eq!(reopened.handle().status().last_enqueued, 2);
    reopened
        .handle()
        .drain(Duration::from_secs(5))
        .await
        .unwrap();
    drop(producer);
    drop(foreign);
    receiver.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    inbox.close().await.unwrap();
}
