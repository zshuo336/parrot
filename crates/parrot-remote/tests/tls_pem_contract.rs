//! PEM migration must retain real key formats, trust and signing behavior.
use parrot_remote::tls::{
    client_config, client_connector, server_acceptor, server_config, TlsConfig,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const RSA_CERT: &[u8] = include_bytes!("fixtures/pem/rsa-cert.pem");
const EC_CERT: &[u8] = include_bytes!("fixtures/pem/ec-cert.pem");
const RSA_PKCS1: &[u8] = include_bytes!("fixtures/pem/rsa-pkcs1.pem");
const RSA_PKCS8: &[u8] = include_bytes!("fixtures/pem/rsa-pkcs8.pem");
const EC_SEC1: &[u8] = include_bytes!("fixtures/pem/ec-sec1.pem");
const EC_PKCS8: &[u8] = include_bytes!("fixtures/pem/ec-pkcs8.pem");
fn config(cert: &[u8], key: &[u8]) -> TlsConfig {
    TlsConfig {
        cert_pem: cert.to_vec(),
        key_pem: key.to_vec(),
        ca_pem: [RSA_CERT, EC_CERT].concat(),
    }
}

#[test]
fn empty_material_is_rejected_before_connecting() {
    for field in 0..3 {
        for empty in [
            b"".as_slice(),
            b"# only comments\n".as_slice(),
            b"-----BEGIN PUBLIC KEY-----\nMAA=\n-----END PUBLIC KEY-----\n".as_slice(),
        ] {
            let mut cfg = config(RSA_CERT, RSA_PKCS8);
            match field {
                0 => cfg.cert_pem = empty.to_vec(),
                1 => cfg.key_pem = empty.to_vec(),
                _ => cfg.ca_pem = empty.to_vec(),
            }
            assert!(
                server_config(&cfg).is_err(),
                "server accepted empty field {field}"
            );
            assert!(
                client_config(&cfg).is_err(),
                "client accepted empty field {field}"
            );
        }
    }
}

#[test]
fn malformed_recognized_sections_are_not_silently_filtered_out() {
    for field in 0..3 {
        for malformed in [
            b"-----BEGIN CERTIFICATE-----\n!not-base64!\n-----END CERTIFICATE-----\n".as_slice(),
            b"-----BEGIN PRIVATE KEY-----\nMAA=\n".as_slice(),
        ] {
            let mut cfg = config(RSA_CERT, RSA_PKCS8);
            let target = match field {
                0 => &mut cfg.cert_pem,
                1 => &mut cfg.key_pem,
                _ => &mut cfg.ca_pem,
            };
            *target = [malformed, target.as_slice()].concat();
            assert!(
                server_config(&cfg).is_err(),
                "server skipped malformed field {field}"
            );
            assert!(
                client_config(&cfg).is_err(),
                "client skipped malformed field {field}"
            );
        }
    }
}

#[test]
fn first_key_is_authoritative_and_mismatches_do_not_fall_through() {
    let cfg = config(RSA_CERT, &[EC_SEC1, RSA_PKCS8].concat());
    assert!(server_config(&cfg).is_err());
    assert!(client_config(&cfg).is_err());
    let cfg = config(
        RSA_CERT,
        &[RSA_CERT, RSA_PKCS1, b"-----BEGIN PRIVATE KEY-----\ninvalid"].concat(),
    );
    server_config(&cfg).unwrap();
    client_config(&cfg).unwrap();
}

async fn transfer(cfg: TlsConfig) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let acceptor = server_acceptor(&cfg).unwrap();
        let connector = client_connector(&cfg).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = async {
            let (socket, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(socket).await.unwrap();
            let mut data = [0; 10];
            tls.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"key-format");
            tls.write_all(b"verified").await.unwrap();
            tls.flush().await.unwrap();
        };
        let client = async {
            let socket = tokio::net::TcpStream::connect(address).await.unwrap();
            let mut tls = connector
                .connect("server.test".try_into().unwrap(), socket)
                .await
                .unwrap();
            tls.write_all(b"key-format").await.unwrap();
            tls.flush().await.unwrap();
            let mut data = [0; 8];
            tls.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"verified");
        };
        tokio::join!(server, client);
    })
    .await
    .expect("TLS format handshake/data exchange stalled");
}

#[tokio::test]
async fn pkcs1_pkcs8_and_sec1_sign_real_mutual_tls_connections() {
    for (cert, key) in [
        (RSA_CERT, RSA_PKCS1),
        (RSA_CERT, RSA_PKCS8),
        (EC_CERT, EC_SEC1),
        (EC_CERT, EC_PKCS8),
    ] {
        let mut cfg = config(cert, key);
        cfg.key_pem = [b"# comments\r\n".as_slice(), cert, key].concat();
        transfer(cfg).await;
    }
}

#[tokio::test]
async fn mqtt_connector_signs_with_every_supported_key_format() {
    use rumqttc::{AsyncClient, Event, MqttOptions, Packet, Transport};
    for (cert, key) in [
        (RSA_CERT, RSA_PKCS1),
        (RSA_CERT, RSA_PKCS8),
        (EC_CERT, EC_SEC1),
        (EC_CERT, EC_PKCS8),
    ] {
        tokio::time::timeout(Duration::from_secs(5), async {
            let cfg = config(cert, key);
            let acceptor = server_acceptor(&cfg).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            // The fixture includes an IP SAN; no machine DNS or verifier bypass.
            let mut options = MqttOptions::new("pem-contract", "127.0.0.1", address.port());
            options.set_transport(Transport::tls(
                cfg.ca_pem,
                Some((cfg.cert_pem, cfg.key_pem)),
                None,
            ));
            let (_client, mut eventloop) = AsyncClient::new(options, 4);
            let server = async {
                let (socket, _) = listener.accept().await.unwrap();
                let mut tls = acceptor.accept(socket).await.unwrap();
                assert_eq!(tls.read_u8().await.unwrap(), 0x10, "expected MQTT CONNECT");
                // This fixed CONNECT has a short client ID and no credentials.
                let remaining = tls.read_u8().await.unwrap();
                assert_eq!(remaining, 24);
                let mut payload = [0; 24];
                tls.read_exact(&mut payload).await.unwrap();
                assert_eq!(&payload[..8], b"\0\x04MQTT\x04\x02");
                assert_eq!(&payload[10..], b"\0\x0cpem-contract");
                tls.write_all(b"\x20\x02\x00\x00").await.unwrap();
                tls.flush().await.unwrap();
            };
            let client = async {
                let event = eventloop.poll().await.unwrap();
                match event {
                    Event::Incoming(Packet::ConnAck(ack)) => {
                        assert_eq!(ack.code, rumqttc::ConnectReturnCode::Success);
                        assert!(!ack.session_present);
                    }
                    other => panic!("expected CONNACK, got {other:?}"),
                }
            };
            tokio::join!(server, client);
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn mqtt_invalid_material_never_sends_a_tls_client_hello() {
    use rumqttc::{AsyncClient, ConnectionError, MqttOptions, Transport};
    let malformed = b"-----BEGIN CERTIFICATE-----\n!not-base64!\n-----END CERTIFICATE-----\n";
    let mut cases = Vec::new();
    for field in 0..3 {
        for invalid in [b"".as_slice(), malformed.as_slice()] {
            let mut cfg = config(RSA_CERT, RSA_PKCS8);
            let value = match field {
                0 => &mut cfg.cert_pem,
                1 => &mut cfg.key_pem,
                _ => &mut cfg.ca_pem,
            };
            *value = if invalid.is_empty() {
                Vec::new()
            } else {
                [invalid, value.as_slice()].concat()
            };
            cases.push(cfg);
        }
    }
    cases.push(config(RSA_CERT, &[EC_SEC1, RSA_PKCS8].concat()));
    for cfg in cases {
        tokio::time::timeout(Duration::from_secs(5), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut options = MqttOptions::new(
                "invalid-pem",
                "127.0.0.1",
                listener.local_addr().unwrap().port(),
            );
            options.set_transport(Transport::tls(
                cfg.ca_pem,
                Some((cfg.cert_pem, cfg.key_pem)),
                None,
            ));
            let (_client, mut eventloop) = AsyncClient::new(options, 4);
            let server = async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut byte = [0];
                assert_eq!(
                    socket.read(&mut byte).await.unwrap(),
                    0,
                    "invalid configuration emitted a TLS ClientHello"
                );
            };
            let client = async {
                let error = eventloop.poll().await.unwrap_err();
                assert!(
                    matches!(error, ConnectionError::Tls(_)),
                    "wrong error: {error:?}"
                );
            };
            tokio::join!(server, client);
        })
        .await
        .expect("MQTT invalid configuration did not retire");
    }
}
