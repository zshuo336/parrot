//! On-demand DNS with process-wide ownership of blocking resolver work.
use crate::node::{NodeAddr, NodeEndpoint};
use std::{
    io,
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, OnceLock},
};

const MAX_ADDRESSES: usize = 16;

async fn bounded_lookup<F>(
    slots: Arc<tokio::sync::Semaphore>,
    lookup: F,
) -> io::Result<Vec<SocketAddr>>
where
    F: FnOnce() -> io::Result<Vec<SocketAddr>> + Send + 'static,
{
    let permit = slots.acquire_owned().await.map_err(io::Error::other)?;
    // getaddrinfo cannot be forcibly cancelled. The actual blocking job owns
    // its permit even after the caller drops its JoinHandle, bounding detached
    // work across cancellation and across all RemoteActorSystem instances.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        lookup()
    })
    .await
    .map_err(io::Error::other)?
}

pub(super) async fn addresses(endpoint: &NodeEndpoint) -> io::Result<Vec<NodeAddr>> {
    let NodeEndpoint::Dns {
        node_id,
        scheme,
        host,
        port,
    } = endpoint
    else {
        return Ok(endpoint.address().cloned().into_iter().collect());
    };
    let answers = lookup_host(host.clone(), *port).await?;
    Ok(answers
        .into_iter()
        .map(|addr| NodeAddr {
            node_id: node_id.clone(),
            identity_is_explicit: true,
            scheme: scheme.clone(),
            addr,
        })
        .collect())
}

/// Shared by startup configuration, maintained seeds and discovered members.
pub(super) async fn lookup_host(host: String, port: u16) -> io::Result<Vec<SocketAddr>> {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let slots = SLOTS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
        .clone();
    let answers = bounded_lookup(slots, move || {
        Ok(bounded_answers((host.as_str(), port).to_socket_addrs()?))
    })
    .await?;
    if answers.is_empty() {
        return Err(io::Error::other("hostname returned no addresses"));
    }
    Ok(answers)
}

fn bounded_answers(addresses: impl Iterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let mut answers = Vec::new();
    for address in addresses.take(MAX_ADDRESSES) {
        if !answers.contains(&address) {
            answers.push(address);
        }
    }
    answers
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dns_answer_work_and_results_are_bounded_and_deduplicated() {
        let mut seen = 0;
        let answers = bounded_answers((0..100u16).map(|index| {
            seen += 1;
            SocketAddr::from(([127, 0, 0, 1], index / 2 + 1))
        }));
        assert_eq!(seen, 16);
        assert_eq!(answers.len(), 8);
        assert_eq!(answers[0].port(), 1);
        assert_eq!(answers[7].port(), 8);
    }
    #[tokio::test]
    async fn cancelled_lookup_keeps_capacity_until_the_actual_resolver_finishes() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let owned = slots.clone();
        let first = tokio::spawn(bounded_lookup(owned, move || {
            let _ = started.send(());
            let _ = wait.recv();
            Ok(vec![])
        }));
        ready.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(
            slots.available_permits(),
            0,
            "cancel must not release a still-running OS resolver slot"
        );
        let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = called.clone();
        let second = bounded_lookup(slots.clone(), move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![])
        });
        tokio::pin!(second);
        assert!(futures::poll!(second.as_mut()).is_pending());
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 0);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(slots.available_permits(), 1);
    }
}
