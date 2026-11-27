// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use parrot_remote::tls::{client_config, server_config, TlsConfig};
use rcgen::{date_time_ymd, CertificateRevocationListParams, KeyIdMethod, RevokedCertParams};

impl Pki {
    fn crl(&self, issued: i32, until: i32) -> Vec<u8> {
        CertificateRevocationListParams {
            this_update: date_time_ymd(issued, 1, 1),
            next_update: date_time_ymd(until, 1, 1),
            crl_number: 1u64.into(),
            issuing_distribution_point: None,
            revoked_certs: vec![RevokedCertParams {
                serial_number: 13u64.into(),
                revocation_time: date_time_ymd(issued, 1, 1),
                reason_code: None,
                invalidity_date: None,
            }],
            key_identifier_method: KeyIdMethod::Sha256,
        }
        .signed_by(&self.issuer)
        .unwrap()
        .pem()
        .unwrap()
        .into_bytes()
    }
}

#[test]
fn required_crl_survives_file_loading_and_config_resolution() {
    let pki = Pki::new();
    let cert = pki.node("local.test");
    let tmp = tempfile::tempdir().unwrap();
    let path = |name| tmp.path().join(name).to_str().unwrap().to_owned();
    std::fs::write(path("ca"), &cert.ca_pem).unwrap();
    std::fs::write(path("cert"), &cert.cert_pem).unwrap();
    std::fs::write(path("key"), &cert.key_pem).unwrap();
    let mut cfg = parrot_config::ParrotConfig::default();
    cfg.remote.node.node_id = Some("local.test".into());
    cfg.remote.security = parrot_config::RemoteSecuritySection {
        mode: Some("mtls".into()),
        ca_path: Some(path("ca")),
        cert_path: Some(path("cert")),
        key_path: Some(path("key")),
        require_crl: Some(true),
        ..Default::default()
    };
    let resolved = cfg.build().unwrap();
    assert!(RemoteConfig::from_resolved(&resolved)
        .err()
        .unwrap()
        .to_string()
        .contains("required X509 CRL missing"));
    std::fs::write(path("ca"), [cert.ca_pem, pki.crl(2020, 2099)].concat()).unwrap();
    RemoteConfig::from_resolved(&resolved).unwrap();
}

#[test]
fn invalid_or_ambiguous_crl_bundles_cannot_build_tls_endpoints() {
    let pki = Pki::new();
    let valid = pki.crl(2020, 2099);
    for crls in [
        pki.crl(2000, 2001),
        pki.crl(2090, 2099),
        [valid.clone(), valid].concat(),
        b"-----BEGIN X509 CRL-----\nMAA=\n-----END X509 CRL-----\n".to_vec(),
    ] {
        let mut cert = pki.node("node.test");
        cert.ca_pem.extend(crls);
        assert!(server_config(&cert).is_err());
        assert!(client_config(&cert).is_err());
    }
}

async fn connections(scheme: &str, server_tls: TlsConfig, clients: Vec<(TlsConfig, bool)>) {
    let state = victim();
    let server = system(
        config(scheme, "server.test").with_mtls(server_tls, ["operator.test".into()]),
        &state,
    );
    server.start().await.unwrap();
    for (client_tls, accepted) in clients {
        let client = system(
            config(scheme, "operator.test").with_mtls(client_tls, []),
            &state,
        );
        client.start().await.unwrap();
        let result = client
            .connect(&address(&server, scheme, "server.test"))
            .await;
        if accepted {
            result.unwrap();
            client
                .admin_stop("server.test", "/user/victim")
                .await
                .unwrap();
            assert!(!state.alive.load(Ordering::SeqCst));
        } else {
            assert!(result.is_err());
            assert!(client.links_snapshot().await.is_empty());
            assert!(server.links_snapshot().await.is_empty());
            assert!(state.alive.load(Ordering::SeqCst));
        }
        client.shutdown().await.unwrap();
    }
    server.shutdown().await.unwrap();
}

async fn check_revocation(scheme: &str) {
    let pki = Pki::new();
    let crl = pki.crl(2020, 2099);
    let issue = |name, serial| {
        let mut cert = pki.issue(name, Some(serial));
        cert.ca_pem.extend_from_slice(&crl);
        cert
    };
    let mut bad_trust = pki.issue("operator.test", Some(15));
    bad_trust.ca_pem.extend(Pki::new().crl(2020, 2099));
    connections(
        scheme,
        issue("server.test", 14),
        vec![
            (issue("operator.test", 13), false),
            (bad_trust, false),
            (issue("operator.test", 15), true),
        ],
    )
    .await;
    connections(
        scheme,
        issue("server.test", 13),
        vec![(issue("operator.test", 15), false)],
    )
    .await;
}

#[tokio::test]
async fn a_clean_leaf_requires_current_status_for_its_intermediate_ca() {
    let root = Pki::new();
    for serial in [13u64, 16] {
        let mut params = rcgen::CertificateParams::default();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "issuing-ca");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        params.serial_number = Some(serial.into());
        let key = rcgen::KeyPair::generate().unwrap();
        let chain = params.signed_by(&key, &root.issuer).unwrap().pem();
        let issuer = Pki {
            issuer: rcgen::Issuer::new(params, key),
            ca: root.ca.clone(),
        };
        let mut operator = issuer.issue("operator.test", Some(17));
        operator.cert_pem.extend_from_slice(chain.as_bytes());
        let mut server = root.issue("server.test", Some(14));
        server.ca_pem.extend(root.crl(2020, 2099));
        connections("tcp", server.clone(), vec![(operator.clone(), false)]).await;
        server.ca_pem.extend(issuer.crl(2020, 2099));
        connections("tcp", server, vec![(operator, serial == 16)]).await;
    }
}

#[tokio::test]
async fn tcp_checks_revocations_in_both_directions_and_accepts_reissued_peers() {
    check_revocation("tcp").await;
}

#[tokio::test]
async fn quic_checks_revocations_in_both_directions_and_accepts_reissued_peers() {
    check_revocation("quic").await;
}
