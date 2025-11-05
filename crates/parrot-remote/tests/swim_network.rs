//! Membership must converge to nodes' own advertised/listening addresses,
//! including a replacement process with the same identity and a new endpoint.
use parrot_api::types::BoxedActorRef;
use parrot_remote::swim::{Member, MemberStatus};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
fn tcp(id: &str) -> Arc<RemoteActorSystem> {
    RemoteActorSystem::new(
        RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap()
}
async fn snapshot(system: &RemoteActorSystem) -> BTreeMap<String, Member> {
    system
        .membership
        .lock()
        .await
        .members
        .iter()
        .map(|(id, member)| {
            let mut member = member.clone();
            member.status_until_ms = 0; // failure deadlines use different local clocks
            (id.clone(), member)
        })
        .collect()
}
async fn converge(
    systems: &[&Arc<RemoteActorSystem>],
    expected: &[(&str, SocketAddr)],
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let mut views = Vec::new();
        for system in systems {
            views.push(snapshot(system).await);
        }
        let valid = views.iter().all(|view| {
            view.len() == expected.len()
                && expected.iter().all(|(id, address)| {
                    view.get(*id).is_some_and(|member| {
                        member.status == MemberStatus::Alive
                            && member.addr.node_id == *id
                            && member.addr.scheme == systems[0].config.scheme
                            && member.addr.host == address.ip().to_string()
                            && member.addr.port == address.port()
                    })
                })
        }) && views.windows(2).all(|pair| pair[0] == pair[1]);
        if valid {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("expected {expected:?}; actual {views:?}"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn tcp_star_converges_to_listeners_not_outbound_ephemeral_ports() {
    let a = tcp("swim-net-a");
    let b = tcp("swim-net-b");
    let c = tcp("swim-net-c");
    for node in [&a, &b, &c] {
        node.start().await.unwrap();
    }
    let expected = [
        ("swim-net-a", a.local_addr().unwrap()),
        ("swim-net-b", b.local_addr().unwrap()),
        ("swim-net-c", c.local_addr().unwrap()),
    ];
    a.connect(&NodeAddr::tcp("swim-net-b", expected[1].1))
        .await
        .unwrap();
    a.connect(&NodeAddr::tcp("swim-net-c", expected[2].1))
        .await
        .unwrap();
    let result = converge(&[&a, &b, &c], &expected).await;
    for node in [&a, &b, &c] {
        node.shutdown().await.unwrap();
    }
    result.expect("real TCP gossip must converge on owner endpoints");
}

#[tokio::test]
async fn replacing_tcp_node_recovers_its_new_endpoint_from_retained_old_state() {
    let a = tcp("swim-restart-a");
    let old_b = tcp("swim-restart-b");
    a.start().await.unwrap();
    old_b.start().await.unwrap();
    let old_address = old_b.local_addr().unwrap();
    a.connect(&NodeAddr::tcp("swim-restart-b", old_address))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !a
            .membership
            .lock()
            .await
            .members
            .contains_key("swim-restart-b")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Bind the replacement before releasing the old socket, so this tests a
    // guaranteed endpoint change rather than depending on ephemeral-port reuse.
    let new_b = tcp("swim-restart-b");
    new_b.start().await.unwrap();
    let new_address = new_b.local_addr().unwrap();
    assert_ne!(new_address, old_address);
    old_b.shutdown().await.unwrap();
    a.connect(&NodeAddr::tcp("swim-restart-b", new_address))
        .await
        .unwrap();
    let result = converge(
        &[&a, &new_b],
        &[
            ("swim-restart-a", a.local_addr().unwrap()),
            ("swim-restart-b", new_address),
        ],
    )
    .await;
    a.shutdown().await.unwrap();
    new_b.shutdown().await.unwrap();
    result.expect("restarted owner must correct retained address and incarnation");
}

#[tokio::test]
async fn outbound_only_node_does_not_advertise_a_transient_client_port() {
    let a = RemoteActorSystem::new(
        RemoteConfig::tcp("swim-client-only", None).with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    let b = tcp("swim-listener");
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect(&NodeAddr::tcp("swim-listener", b.local_addr().unwrap()))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let a_view = snapshot(&a).await;
            let b_view = snapshot(&b).await;
            if a_view == b_view && a_view.len() == 2 {
                let own = &a_view["swim-client-only"];
                assert!(own.addr.host.is_empty());
                assert_eq!(own.addr.port, 0);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    result.expect("non-listening node must converge to an explicitly nondialable endpoint");
}

#[tokio::test]
async fn explicitly_advertised_dns_and_ipv6_addresses_override_bind_addresses() {
    for (index, advertised, host, port) in [
        (0, "member.example:9801", "member.example", 9801),
        (1, "[2001:db8::1234]:9802", "2001:db8::1234", 9802),
    ] {
        let id = format!("swim-advertised-{index}");
        let a = tcp(&format!("swim-observer-{index}"));
        let b = RemoteActorSystem::new(
            RemoteConfig::tcp(&id, Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development()
                .with_direct_addr(advertised),
            Arc::new(Empty),
        )
        .unwrap();
        a.start().await.unwrap();
        b.start().await.unwrap();
        a.connect(&NodeAddr::tcp(&id, b.local_addr().unwrap()))
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let view = snapshot(&a).await;
                if view.get(&id).is_some_and(|m| {
                    m.addr.host == host && m.addr.port == port && m.incarnation >= 1
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        a.shutdown().await.unwrap();
        b.shutdown().await.unwrap();
        result.expect(
            "gossip must preserve the owner's explicit advertisement without DNS resolution",
        );
    }
}

#[test]
fn invalid_advertised_endpoints_are_rejected_during_configuration() {
    for address in [
        "bad address",
        "host:0",
        "host:65536",
        "::1:80",
        "tcp://host:80",
        "other@host:80",
    ] {
        let result = RemoteActorSystem::new(
            RemoteConfig::tcp("swim-invalid", None).with_direct_addr(address),
            Arc::new(Empty),
        );
        assert!(result.is_err(), "accepted invalid direct_addr {address}");
    }
}

#[tokio::test]
async fn shutdown_cancels_start_waiting_for_the_membership_lock() {
    let system = tcp("swim-start-cancel");
    let guard = system.membership.lock().await;
    let starting = system.clone();
    let task = tokio::spawn(async move { starting.start().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while system.local_addr().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let shutdown = tokio::time::timeout(Duration::from_secs(1), system.shutdown()).await;
    // The lock stays held until shutdown has had to cancel the startup wait.
    drop(guard);
    shutdown
        .expect("membership initialization cannot pin shutdown")
        .unwrap();
    assert!(task.await.unwrap().is_err());
}

struct Pki {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    ca: Vec<u8>,
}
impl Pki {
    fn new() -> Self {
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().unwrap();
        let ca = params.self_signed(&key).unwrap().pem().into_bytes();
        Self {
            issuer: rcgen::Issuer::new(params, key),
            ca,
        }
    }
    fn node(&self, name: &str) -> parrot_remote::tls::TlsConfig {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        parrot_remote::tls::TlsConfig {
            cert_pem: cert.pem().into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
            ca_pem: self.ca.clone(),
        }
    }
}
async fn secure_star(scheme: &str) {
    let pki = Pki::new();
    let ids = ["swim-secure-a", "swim-secure-b", "swim-secure-c"];
    let systems: Vec<_> = ids
        .iter()
        .map(|id| {
            let bind = Some("127.0.0.1:0".parse().unwrap());
            let config = if scheme == "tcp" {
                RemoteConfig::tcp(*id, bind)
            } else {
                RemoteConfig::quic(*id, bind)
            };
            RemoteActorSystem::new(config.with_mtls(pki.node(id), []), Arc::new(Empty)).unwrap()
        })
        .collect();
    for system in &systems {
        system.start().await.unwrap();
    }
    let expected: Vec<_> = ids
        .iter()
        .zip(&systems)
        .map(|(id, system)| (*id, system.local_addr().unwrap()))
        .collect();
    for (id, address) in &expected[1..] {
        let target = if scheme == "tcp" {
            NodeAddr::tcp(*id, *address)
        } else {
            NodeAddr::quic(*id, *address)
        };
        systems[0].connect(&target).await.unwrap();
    }
    let refs: Vec<_> = systems.iter().collect();
    let result = converge(&refs, &expected).await;
    // The leaves have no direct link. Cluster certificates, without an admin
    // allowlist, authorize a real probe through the center on TCP and QUIC.
    assert!(!systems[1].missing_direct_connections(&[ids[2]]).is_empty());
    assert_eq!(
        systems[1].probe_member(ids[2]).await,
        parrot_remote::swim::ProbeOutcome::Reachable
    );
    assert!(!systems[1].missing_direct_connections(&[ids[2]]).is_empty());
    for system in &systems {
        for (_, sender, _) in system.links_snapshot().await {
            assert_eq!(
                sender.authentication(),
                parrot_remote::security::PeerAuthentication::MutualTls
            );
        }
        system.shutdown().await.unwrap();
    }
    result.expect("verified cluster identities must converge without administrative allowlisting");
}
#[tokio::test]
async fn mtls_tcp_star_converges_on_authoritative_endpoints() {
    secure_star("tcp").await;
}
#[tokio::test]
async fn mtls_quic_star_converges_on_authoritative_endpoints() {
    secure_star("quic").await;
}

// The child runs this same test in a fresh OS process. No helper-only test is
// counted as a separate passing contract, and all children are killed/reaped.
struct NodeProcess {
    child: std::process::Child,
    directory: std::path::PathBuf,
    reaped: bool,
}
impl NodeProcess {
    fn spawn(forbidden_port: u16) -> Self {
        let directory =
            std::env::temp_dir().join(format!("parrot-swim-process-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let log = std::fs::File::create(directory.join("child.log")).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "killed_tcp_process_recovers_membership_after_restart",
                "--nocapture",
            ])
            .env("PARROT_SWIM_CHILD_DIRECTORY", &directory)
            .env("PARROT_SWIM_FORBIDDEN_PORT", forbidden_port.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        Self {
            child,
            directory,
            reaped: false,
        }
    }
    async fn address(&mut self) -> SocketAddr {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(self.directory.join("address")) {
                if let Ok(address) = text.parse() {
                    return address;
                }
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                self.reaped = true;
                panic!(
                    "child exited before binding ({status}): {}",
                    self.directory.display()
                );
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "child startup timeout: {}",
                self.directory.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    fn view(&self) -> Option<BTreeMap<String, Member>> {
        serde_json::from_slice(&std::fs::read(self.directory.join("view.json")).ok()?).ok()
    }
    fn kill_and_reap(&mut self) -> std::process::ExitStatus {
        self.child.kill().unwrap();
        let status = self.child.wait().unwrap();
        self.reaped = true;
        status
    }
}
impl Drop for NodeProcess {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Logs remain available on failure; successful parents remove only
        // their own newly created directories after all assertions pass.
    }
}

async fn child_node(directory: std::path::PathBuf) {
    let forbidden = std::env::var("PARROT_SWIM_FORBIDDEN_PORT")
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let mut selected = None;
    for _ in 0..16 {
        let node = tcp("swim-process-peer");
        node.start().await.unwrap();
        if node.local_addr().unwrap().port() != forbidden {
            selected = Some(node);
            break;
        }
        node.shutdown().await.unwrap();
    }
    let node = selected.expect("could not allocate a distinct listener");
    std::fs::write(
        directory.join("address"),
        node.local_addr().unwrap().to_string(),
    )
    .unwrap();
    loop {
        let view = snapshot(&node).await;
        std::fs::write(
            directory.join("view.json"),
            serde_json::to_vec(&view).unwrap(),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn killed_tcp_process_recovers_membership_after_restart() {
    if let Some(directory) = std::env::var_os("PARROT_SWIM_CHILD_DIRECTORY") {
        child_node(directory.into()).await;
        return;
    }
    let a = tcp("swim-process-observer");
    a.start().await.unwrap();
    let mut first = NodeProcess::spawn(0);
    let first_pid = first.child.id();
    let old_address = first.address().await;
    a.connect(&NodeAddr::tcp("swim-process-peer", old_address))
        .await
        .unwrap();
    let old_incarnation = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = snapshot(&a).await;
            if let Some(peer) = view
                .get("swim-process-peer")
                .filter(|peer| peer.incarnation >= 1)
            {
                if first.view().as_ref() == Some(&view) {
                    break peer.incarnation;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("initial process must actually exchange membership");
    let detection_started = tokio::time::Instant::now();
    let exit = first.kill_and_reap();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit.signal(), Some(9), "test must really deliver SIGKILL");
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if snapshot(&a)
                .await
                .get("swim-process-peer")
                .is_some_and(|peer| peer.status == MemberStatus::Dead)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a real killed process must transition to Dead without injected clocks or statuses");
    let detected_after = detection_started.elapsed();
    let mut second = NodeProcess::spawn(old_address.port());
    let second_pid = second.child.id();
    let new_address = second.address().await;
    assert_ne!(new_address, old_address);
    a.connect(&NodeAddr::tcp("swim-process-peer", new_address))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = snapshot(&a).await;
            let recovered = view.get("swim-process-peer").is_some_and(|peer| {
                peer.addr.port == new_address.port()
                    && peer.addr.host == new_address.ip().to_string()
                    && peer.addr.scheme == "tcp"
                    && peer.addr.node_id == "swim-process-peer"
                    && peer.status == MemberStatus::Alive
                    && peer.incarnation > old_incarnation
            });
            if recovered && second.view().as_ref() == Some(&view) {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let second_exit = second.kill_and_reap();
    a.shutdown().await.unwrap();
    let view = result.unwrap_or_else(|error| {
        panic!(
            "process restart did not converge: {error}; logs {}, {}",
            first.directory.display(),
            second.directory.display()
        )
    });
    eprintln!("SWIM process recovery: old_pid={first_pid} old_addr={old_address} old_exit={exit} detected_dead_after={detected_after:?}; new_pid={second_pid} new_addr={new_address} cleanup_exit={second_exit}; view={view:?}");
    std::fs::remove_dir_all(&first.directory).unwrap();
    std::fs::remove_dir_all(&second.directory).unwrap();
}
