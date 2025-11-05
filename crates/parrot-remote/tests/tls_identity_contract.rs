//! Identity extraction is additional to TLS chain/signature verification.
use parrot_remote::tls::verify_node_identity;
use rcgen::{CertificateParams, DistinguishedName, DnType, Issuer, KeyPair};
use rustls::pki_types::CertificateDer;

fn issued(subject: Option<&str>) -> CertificateDer<'static> {
    let mut ca = CertificateParams::new(Vec::new()).unwrap();
    ca.distinguished_name = DistinguishedName::new();
    ca.distinguished_name.push(DnType::CommonName, "trusted-ca");
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let issuer = Issuer::new(ca, KeyPair::generate().unwrap());
    let mut leaf = CertificateParams::new(Vec::new()).unwrap();
    leaf.distinguished_name = DistinguishedName::new();
    if let Some(name) = subject {
        leaf.distinguished_name.push(DnType::CommonName, name);
    }
    leaf.signed_by(&KeyPair::generate().unwrap(), &issuer)
        .unwrap()
        .der()
        .clone()
}
#[test]
fn signed_certificate_uses_leaf_subject_instead_of_issuer_cn() {
    verify_node_identity(&[issued(Some("node-real"))], "node-real")
        .expect("a CA-issued leaf must authenticate its own subject identity");
}
#[test]
fn issuer_common_name_cannot_be_claimed_as_the_leaf_identity() {
    assert!(verify_node_identity(&[issued(Some("node-real"))], "trusted-ca").is_err());
}
#[test]
fn leaf_without_common_name_cannot_borrow_issuer_identity() {
    assert!(verify_node_identity(&[issued(None)], "trusted-ca").is_err());
}
#[test]
fn truncated_der_is_an_error_and_never_panics() {
    // This OID followed by an incomplete UTF8String used to index past EOF.
    let der = CertificateDer::from(vec![0x06, 0x03, 0x55, 0x04, 0x03, 0x0c]);
    let result = std::panic::catch_unwind(|| verify_node_identity(&[der], "node-real"));
    assert!(result.is_ok(), "untrusted certificate bytes panicked");
    assert!(result.unwrap().is_err());
}
#[test]
fn cn_shaped_bytes_are_not_a_certificate() {
    let der = CertificateDer::from(b"\x06\x03\x55\x04\x03\x0c\x04node".to_vec());
    assert!(verify_node_identity(&[der], "node").is_err());
}

#[test]
fn tls_builders_work_without_mutating_global_crypto_provider() {
    use parrot_remote::tls::{client_connector, server_acceptor, TlsConfig};
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    let cert = rcgen::generate_simple_self_signed(vec!["node.test".into()]).unwrap();
    let config = TlsConfig {
        cert_pem: cert.cert.pem().into_bytes(),
        key_pem: cert.signing_key.serialize_pem().into_bytes(),
        ca_pem: cert.cert.pem().into_bytes(),
    };
    let server = std::panic::catch_unwind(|| server_acceptor(&config));
    let client = std::panic::catch_unwind(|| client_connector(&config));
    assert!(
        server.is_ok(),
        "server builder required global crypto initialization"
    );
    assert!(
        client.is_ok(),
        "client builder required global crypto initialization"
    );
    server.unwrap().unwrap();
    client.unwrap().unwrap();
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
}

#[test]
fn complete_der_is_required_and_every_truncation_is_rejected() {
    let der = issued(Some("node-real"));
    for end in 0..der.len() {
        let truncated = CertificateDer::from(der[..end].to_vec());
        assert!(
            verify_node_identity(&[truncated], "node-real").is_err(),
            "accepted prefix of length {end}"
        );
    }
    let mut trailing = der.to_vec();
    trailing.extend_from_slice(b"trailing");
    assert!(verify_node_identity(&[trailing.into()], "node-real").is_err());
}

#[test]
fn long_form_lengths_and_utf8_subjects_are_parsed_without_loss() {
    for name in ["node".repeat(40), "节点甲".into()] {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, name.as_str());
        let cert = params.self_signed(&KeyPair::generate().unwrap()).unwrap();
        verify_node_identity(std::slice::from_ref(cert.der()), &name).unwrap();
    }
}

#[test]
fn ambiguous_subject_names_are_rejected_even_when_one_matches() {
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "node-real");
    // Distinct rcgen keys with the same serialized CN OID create two subject
    // attributes without forging or invalidating the certificate signature.
    params
        .distinguished_name
        .push(DnType::CustomDnType(vec![2, 5, 4, 3]), "node-other");
    let cert = params.self_signed(&KeyPair::generate().unwrap()).unwrap();
    for claimed in ["node-real", "node-other"] {
        assert!(verify_node_identity(std::slice::from_ref(cert.der()), claimed).is_err());
    }
}

#[test]
fn cn_in_extension_cannot_supply_missing_subject_identity() {
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.distinguished_name = DistinguishedName::new();
    params
        .custom_extensions
        .push(rcgen::CustomExtension::from_oid_content(
            &[1, 2, 3, 4],
            b"\x06\x03\x55\x04\x03\x0c\x04node".to_vec(),
        ));
    let cert = params.self_signed(&KeyPair::generate().unwrap()).unwrap();
    assert!(verify_node_identity(std::slice::from_ref(cert.der()), "node").is_err());
}

#[tokio::test]
async fn ca_issued_mutual_tls_transfers_data_and_checks_leaf_identities() {
    use parrot_remote::tls::{client_connector, server_acceptor, TlsConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut ca = CertificateParams::default();
    ca.distinguished_name.push(DnType::CommonName, "test-root");
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let ca_key = KeyPair::generate().unwrap();
    let root = ca.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca, ca_key);
    let identity = |name: &str| {
        let mut params = CertificateParams::new(vec![name.into()]).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let key = KeyPair::generate().unwrap();
        let leaf = params.signed_by(&key, &issuer).unwrap();
        verify_node_identity(std::slice::from_ref(leaf.der()), name).unwrap();
        assert!(verify_node_identity(std::slice::from_ref(leaf.der()), "test-root").is_err());
        TlsConfig {
            cert_pem: leaf.pem().into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
            ca_pem: root.pem().into_bytes(),
        }
    };
    let server = server_acceptor(&identity("server.test")).unwrap();
    let client = client_connector(&identity("client.test")).unwrap();
    let (client_io, server_io) = tokio::io::duplex(8192);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let (client, server) = tokio::join!(
            client.connect(
                rustls::pki_types::ServerName::try_from("server.test").unwrap(),
                client_io
            ),
            server.accept(server_io)
        );
        let mut client = client.unwrap();
        let mut server = server.unwrap();
        verify_node_identity(
            client.get_ref().1.peer_certificates().unwrap(),
            "server.test",
        )
        .unwrap();
        verify_node_identity(
            server.get_ref().1.peer_certificates().unwrap(),
            "client.test",
        )
        .unwrap();
        assert!(
            verify_node_identity(server.get_ref().1.peer_certificates().unwrap(), "test-root")
                .is_err()
        );
        client.write_all(b"authenticated").await.unwrap();
        client.flush().await.unwrap();
        let mut message = [0; 13];
        server.read_exact(&mut message).await.unwrap();
        assert_eq!(&message, b"authenticated");
    })
    .await
    .expect("real mutual TLS handshake or transfer stalled");
}
