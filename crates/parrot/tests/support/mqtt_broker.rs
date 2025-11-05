//! Owned TCP protocol fixture. This is deliberately not a Mosquitto emulator or
//! evidence of production broker interoperability. It exposes ACK ordering and
//! connection generations directly, without relying on an ambient broker.
use parrot_remote::bytes::{Bytes, BytesMut};
use parrot_remote::mqtt::client as rumqttc;
use rumqttc::{Packet, Publish, QoS};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub struct Broker {
    listener: TcpListener,
}
impl Broker {
    pub async fn new() -> Self {
        Self {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
        }
    }
    pub fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }
    pub async fn accept(&self, topic: &str) -> Session {
        let (socket, _) = tokio::time::timeout(Duration::from_secs(5), self.listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut session = Session {
            socket,
            bytes: BytesMut::new(),
        };
        let Packet::Connect(connect) = session.next().await else {
            panic!("expected CONNECT")
        };
        assert!(
            !connect.clean_session,
            "bridge must request a persistent broker session"
        );
        session.raw(&[0x20, 2, 0, 0]).await;
        let Packet::Subscribe(subscribe) = session.next().await else {
            panic!("expected SUBSCRIBE")
        };
        assert_eq!(subscribe.filters.len(), 1);
        assert_eq!(subscribe.filters[0].path, topic);
        assert_eq!(subscribe.filters[0].qos, QoS::AtLeastOnce);
        let [hi, lo] = subscribe.pkid.to_be_bytes();
        session.raw(&[0x90, 3, hi, lo, 1]).await;
        session
    }
}
pub struct Session {
    socket: TcpStream,
    bytes: BytesMut,
}
impl Session {
    pub async fn raw(&mut self, bytes: &[u8]) {
        self.socket.write_all(bytes).await.unwrap();
    }
    pub async fn publish(&mut self, topic: &str, qos: QoS, pkid: u16, payload: Bytes) {
        let publish = Publish {
            dup: false,
            qos,
            retain: false,
            topic: topic.into(),
            pkid,
            payload,
        };
        self.publication(publish).await;
    }
    pub async fn publication(&mut self, publish: Publish) {
        let mut bytes = BytesMut::new();
        publish.write(&mut bytes).unwrap();
        self.raw(&bytes).await;
    }
    async fn read(&mut self) -> Option<Packet> {
        loop {
            match Packet::read(&mut self.bytes, 1024 * 1024) {
                Ok(Packet::PingReq) => self.raw(&[0xd0, 0]).await,
                Ok(packet) => return Some(packet),
                Err(rumqttc::mqttbytes::Error::InsufficientBytes(_)) => {
                    match self.socket.read_buf(&mut self.bytes).await {
                        Ok(0) => return None,
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::BrokenPipe
                            ) =>
                        {
                            return None;
                        }
                        Err(error) => panic!("unexpected socket read failure: {error}"),
                    }
                }
                Err(error) => panic!("unexpected MQTT bytes: {error:?}"),
            }
        }
    }
    pub async fn next(&mut self) -> Packet {
        tokio::time::timeout(Duration::from_secs(5), self.read())
            .await
            .expect("MQTT packet deadline")
            .expect("connection stays open")
    }
    pub async fn quiet(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(50), self.read())
                .await
                .is_err(),
            "unexpected receipt/ACK before the barrier"
        );
    }
    pub async fn closed(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), self.read())
                .await
                .unwrap()
                .is_none(),
            "invalid input must not be acknowledged"
        );
    }
    pub async fn ack(&mut self, id: u16) {
        let [hi, lo] = id.to_be_bytes();
        self.raw(&[0x40, 2, hi, lo]).await;
    }
    pub async fn incoming_ack(&mut self, expected: u16) {
        let Packet::PubAck(ack) = self.next().await else {
            panic!("expected subscriber PUBACK")
        };
        assert_eq!(ack.pkid, expected);
    }
}
