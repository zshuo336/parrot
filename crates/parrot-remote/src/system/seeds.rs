//! Configured seed ownership persists after the first successful connection.
use super::{discovery, NodeAddr, RemoteActorSystem, RemoteConfig, RemoteError};
use futures::{stream, StreamExt};
use parrot_config::endpoint::SeedEndpoint;
use std::{collections::BTreeMap, future::Future, io, net::SocketAddr, sync::Weak, time::Duration};

#[derive(Default)]
pub(super) struct SeedGroup {
    identity: String,
    explicit: bool,
    numeric: Vec<NodeAddr>,
    dns: Vec<SeedEndpoint>,
    connected_id: Option<String>,
    next_address: usize,
    next_dns: usize,
}
pub(super) fn groups(config: &RemoteConfig) -> Result<Vec<SeedGroup>, RemoteError> {
    let mut groups: BTreeMap<String, SeedGroup> = BTreeMap::new();
    if config.seed_endpoints.is_empty() {
        for seed in &config.seeds {
            let group = groups.entry(seed.node_id.clone()).or_default();
            group.identity = seed.node_id.clone();
            group.explicit |= seed.identity_is_explicit;
            if !group.numeric.contains(seed) {
                group.numeric.push(seed.clone());
            }
        }
    } else {
        for endpoint in &config.seed_endpoints {
            let group = groups.entry(endpoint.node_id.clone()).or_default();
            group.identity = endpoint.node_id.clone();
            group.explicit |= endpoint.identity_is_explicit;
            if endpoint.scheme == "mem" || endpoint.socket_addr().is_some() {
                let address = NodeAddr::from_endpoint(endpoint).map_err(RemoteError::Transport)?;
                if !group.numeric.contains(&address) {
                    group.numeric.push(address);
                }
            } else if !group.dns.contains(endpoint) {
                group.dns.push(endpoint.clone());
            }
        }
    }
    Ok(groups.into_values().collect())
}

impl SeedGroup {
    async fn candidates<F, Fut>(&mut self, lookup: F) -> Vec<(NodeAddr, bool)>
    where
        F: Fn(String, u16) -> Fut,
        Fut: Future<Output = io::Result<Vec<SocketAddr>>>,
    {
        let mut addresses: Vec<_> = self.numeric.iter().cloned().map(|a| (a, true)).collect();
        let mut sources = self.dns.clone();
        if !sources.is_empty() {
            let start = self.next_dns % sources.len();
            sources.rotate_left(start);
            self.next_dns = (start + 4) % sources.len();
        }
        let mut lookups = stream::iter(sources.into_iter().map(|source| {
            let lookup = &lookup;
            async move {
                let result = lookup(source.host.clone().unwrap_or_default(), source.port).await;
                (source, result)
            }
        }))
        .buffer_unordered(4);
        let collect = async {
            while let Some((source, result)) = lookups.next().await {
                match result {
                    Ok(resolved) => {
                        for addr in resolved {
                            let address = NodeAddr {
                                node_id: source.node_id.clone(),
                                identity_is_explicit: source.identity_is_explicit,
                                scheme: source.scheme.into(),
                                addr,
                            };
                            if !addresses.iter().any(|(existing, _)| existing == &address) {
                                addresses.push((address, false));
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(node = %self.identity, %error, "seed DNS refresh failed")
                    }
                }
            }
        };
        // Keep numeric and completed DNS answers usable even when another
        // source is stuck. The actual OS jobs keep their global DNS permits.
        let _ = tokio::time::timeout(Duration::from_secs(1), collect).await;
        addresses.sort_by(|(a, _), (b, _)| {
            (&a.node_id, &a.scheme, a.addr, a.identity_is_explicit).cmp(&(
                &b.node_id,
                &b.scheme,
                b.addr,
                b.identity_is_explicit,
            ))
        });
        addresses
    }

    async fn attempt(&mut self, weak: &Weak<RemoteActorSystem>) -> Option<String> {
        let mut addresses = self.candidates(discovery::lookup_host).await;
        if addresses.is_empty() {
            return None;
        }
        let start = self.next_address % addresses.len();
        addresses.rotate_left(start);
        for (offset, (address, retain)) in addresses.iter().enumerate() {
            self.next_address = (start + offset + 1) % addresses.len();
            match tokio::time::timeout(
                Duration::from_secs(if *retain { 2 } else { 1 }),
                RemoteActorSystem::connect_weak_identity(weak, address, *retain),
            )
            .await
            {
                Ok(Ok(identity)) => return Some(identity),
                Ok(Err(error)) => {
                    tracing::debug!(node = %address.node_id, %error, "seed connection retry")
                }
                Err(_) => tracing::debug!(node = %address.node_id, "seed candidate timed out"),
            }
        }
        None
    }

    async fn maintain(mut self, weak: Weak<RemoteActorSystem>) -> Self {
        let Some(system) = weak.upgrade() else {
            return self;
        };
        let known = self
            .connected_id
            .as_deref()
            .or(self.explicit.then_some(self.identity.as_str()));
        if known.is_some_and(|id| system.direct_sender_of(id).is_some()) {
            return self;
        }
        drop(system);
        // Each group gets a bounded round; unreachable groups cannot occupy
        // every worker forever. Cursor updates survive an expired round.
        if let Ok(Some(identity)) =
            tokio::time::timeout(Duration::from_secs(2), self.attempt(&weak)).await
        {
            self.connected_id = Some(identity);
        }
        self
    }
}

pub(super) async fn run(weak: Weak<RemoteActorSystem>, mut groups: Vec<SeedGroup>) {
    let mut backoff = Duration::from_millis(50);
    while !groups.is_empty() {
        groups = stream::iter(groups.into_iter().map(|group| group.maintain(weak.clone())))
            .buffer_unordered(4)
            .collect()
            .await;
        if weak.strong_count() == 0 {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn stalled_dns_does_not_discard_static_or_other_completed_answers() {
        let mut group = SeedGroup {
            identity: "peer".into(),
            ..Default::default()
        };
        group
            .numeric
            .push(NodeAddr::tcp("peer", "127.0.0.1:1".parse().unwrap()));
        group.dns = ["tcp://peer@stalled.test:2", "tcp://peer@working.test:3"]
            .map(|s| SeedEndpoint::parse(s, "tcp").unwrap())
            .to_vec();
        let answers = group
            .candidates(|host, port| async move {
                if host == "stalled.test" {
                    std::future::pending::<()>().await;
                }
                Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
            })
            .await;
        assert_eq!(answers.len(), 2);
        assert!(answers
            .iter()
            .any(|(addr, retained)| addr.addr.port() == 1 && *retained));
        assert!(answers
            .iter()
            .any(|(addr, retained)| addr.addr.port() == 3 && !retained));
    }
}
