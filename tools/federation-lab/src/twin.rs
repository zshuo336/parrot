//! Bounded digital-twin checks against the actual DirectoryShards implementation.
//!
//! A batch registers model nodes through propose/commit/apply, then independently
//! resolves them and their cluster borders. This checks the local directory path,
//! not network reachability, replication under faults or million-node residency.

use parrot_remote::raft::ManualClock;
use parrot_remote::roles::directory::{DirCmd, DirQuery, DirectoryShards};
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::Instant;

/// At most this many requested nodes are materialized in a batch. Cluster border
/// nodes may add another BATCH_SIZE entries; Raft logs are dropped with the batch.
pub const BATCH_SIZE: usize = 512;
/// Usable model addresses within 10.0.0.0/8, excluding network and broadcast.
pub const MAX_MODEL_NODES: u64 = (1 << 24) - 2;

#[derive(Debug, Clone)]
pub struct TwinConfig {
    pub realm: String,
    pub clusters: usize,
    pub nodes_per_cluster: usize,
    pub directory_shards: usize,
}

impl Default for TwinConfig {
    fn default() -> Self {
        Self {
            realm: "fed".into(),
            clusters: 50,
            nodes_per_cluster: 20_000,
            directory_shards: 16,
        }
    }
}

impl TwinConfig {
    /// Validate before multiplying, allocating rings or generating addresses.
    pub fn total_nodes(&self) -> Result<u64, String> {
        if self.realm.is_empty()
            || !self
                .realm
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("realm must be a nonempty ASCII name (letters, digits, '-' or '_')".into());
        }
        if !(1..=256).contains(&self.directory_shards) {
            return Err("directory_shards must be in 1..=256".into());
        }
        let total = self
            .clusters
            .checked_mul(self.nodes_per_cluster)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or("address space size overflow")?;
        if total == 0 || total > MAX_MODEL_NODES {
            return Err(format!(
                "address space must contain 1..={MAX_MODEL_NODES} nodes"
            ));
        }
        Ok(total)
    }

    pub fn address_of(&self, idx: u64) -> Option<String> {
        if idx >= self.total_nodes().ok()? {
            return None;
        }
        Some(format!(
            "parrot://{}/c{}/n{}",
            self.realm,
            idx / self.nodes_per_cluster as u64,
            idx % self.nodes_per_cluster as u64
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedEndpoint {
    /// Unique model endpoint: 10.0.0.1 plus the flattened node index, port 7.
    /// These addresses are synthetic, not promises of live listening sockets.
    pub endpoint: String,
    pub border: String,
}

pub struct DigitalTwin {
    config: TwinConfig,
    total: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraversalReport {
    pub visited: u64,
    pub correct: u64,
    pub unreachable: u64,
    pub mismatch: u64,
    pub elapsed_ms: u128,
    pub elapsed_ns: u128,
    /// Allocator/RSS measurement is unavailable, never a fabricated zero.
    pub materialized_bytes: Option<u64>,
    /// Actual maximum resident directory node count across bounded batches.
    pub peak_materialized_nodes: usize,
}

impl TraversalReport {
    pub fn passed(&self, total: u64) -> bool {
        total > 0
            && self.visited == total
            && self.correct == total
            && self.unreachable == 0
            && self.mismatch == 0
    }
}

impl DigitalTwin {
    pub fn build(config: TwinConfig) -> Result<Self, String> {
        let total = config.total_nodes()?;
        Ok(Self { config, total })
    }

    pub fn total_nodes(&self) -> u64 {
        self.total
    }

    fn indices_of(&self, addr: &str) -> Option<(u64, u64)> {
        let rest = addr.strip_prefix("parrot://")?;
        let (realm, rest) = rest.split_once('/')?;
        if realm != self.config.realm {
            return None;
        }
        let (c, n) = rest.split_once('/')?;
        let cluster = parse_canonical_u64(c.strip_prefix('c')?)?;
        let node = parse_canonical_u64(n.strip_prefix('n')?)?;
        if cluster >= self.config.clusters as u64 || node >= self.config.nodes_per_cluster as u64 {
            return None;
        }
        Some((cluster, node))
    }

    pub fn expected_of(&self, addr: &str) -> Option<ExpectedEndpoint> {
        let (cluster, node) = self.indices_of(addr)?;
        let index = cluster * self.config.nodes_per_cluster as u64 + node;
        let ip = Ipv4Addr::from(0x0a00_0001 + index as u32);
        Some(ExpectedEndpoint {
            endpoint: format!("tcp://{ip}:7"),
            border: format!("parrot://{}/c{cluster}", self.config.realm),
        })
    }

    /// Inverse mapping checks socket syntax, port, subnet and model range.
    pub fn address_for_endpoint(&self, endpoint: &str) -> Option<String> {
        let socket: SocketAddrV4 = endpoint.strip_prefix("tcp://")?.parse().ok()?;
        if socket.port() != 7 || socket.ip().octets()[0] != 10 {
            return None;
        }
        let index = u32::from(*socket.ip()).checked_sub(0x0a00_0001)?;
        self.config.address_of(index as u64)
    }

    /// Populate a bounded set through real solo-Raft directory write APIs.
    /// The writer derives registrations from numeric topology, not expected_of.
    pub fn populate_directory(&self, indices: &[u64]) -> Result<DirectoryShards, String> {
        if indices.len() > BATCH_SIZE || indices.iter().any(|&i| i >= self.total) {
            return Err("directory batch exceeds its size or address-space bounds".into());
        }
        let clock = Arc::new(ManualClock::new());
        let mut directory = DirectoryShards::new(self.config.directory_shards, clock.clone());
        clock.advance(1400);
        for shard in directory.shards.values_mut() {
            shard.raft.tick();
        }
        let mut registered = HashSet::new();
        let mut clusters = HashSet::new();
        for &idx in indices {
            let cluster = idx / self.config.nodes_per_cluster as u64;
            let border_idx = cluster * self.config.nodes_per_cluster as u64;
            for entry in [border_idx, idx] {
                if !registered.insert(entry) {
                    continue;
                }
                let node = entry % self.config.nodes_per_cluster as u64;
                let host = entry + 1;
                let endpoint = format!(
                    "tcp://10.{}.{}.{}:7",
                    (host >> 16) & 255,
                    (host >> 8) & 255,
                    host & 255
                );
                directory
                    .propose(&DirCmd::Upsert {
                        node: format!("{}/c{cluster}/n{node}", self.config.realm),
                        endpoints: vec![endpoint],
                    })
                    .map_err(|e| format!("directory registration failed: {e:?}"))?;
            }
            if clusters.insert(cluster) {
                directory
                    .propose(&DirCmd::BorderDeclare {
                        prefix: format!("parrot://{}/c{cluster}/", self.config.realm),
                        node: format!("{}/c{cluster}/n0", self.config.realm),
                    })
                    .map_err(|e| format!("border registration failed: {e:?}"))?;
            }
        }
        directory
            .apply_committed_all()
            .map_err(|e| format!("directory application failed: {e}"))?;
        Ok(directory)
    }

    /// Compare the independent model with observed directory reads. Missing,
    /// stale or corrupt registrations are mismatches, not successful lookups.
    pub fn verify(&self, addr: &str, directory: &DirectoryShards) -> VerifyOutcome {
        let Some(expected) = self.expected_of(addr) else {
            return VerifyOutcome::OutOfRealm;
        };
        if self.address_for_endpoint(&expected.endpoint).as_deref() != Some(addr) {
            return VerifyOutcome::Mismatch;
        }
        let key = addr.strip_prefix("parrot://").expect("validated address");
        let entry = directory
            .query_checked(&DirQuery::Resolve {
                prefix_or_node: key.into(),
            })
            .ok()
            .flatten();
        if !entry.is_some_and(|e| e.endpoints == [expected.endpoint]) {
            return VerifyOutcome::Mismatch;
        }
        let border_addr = format!("{}/n0", expected.border);
        let border = directory
            .query_checked(&DirQuery::BorderFor {
                prefix: addr.into(),
            })
            .ok()
            .flatten();
        if !border.is_some_and(|e| e.endpoints == [border_addr.clone()]) {
            return VerifyOutcome::Mismatch;
        }
        let border_entry = directory
            .query_checked(&DirQuery::Resolve {
                prefix_or_node: border_addr.strip_prefix("parrot://").unwrap().into(),
            })
            .ok()
            .flatten();
        let border_expected = self.expected_of(&border_addr).unwrap();
        if !border_entry.is_some_and(|e| e.endpoints == [border_expected.endpoint]) {
            return VerifyOutcome::Mismatch;
        }
        VerifyOutcome::Correct
    }

    pub fn traverse_all(&self) -> Result<TraversalReport, String> {
        self.traverse_sampled(self.total)
    }

    /// Spread samples over the full range including both endpoints (except a
    /// singleton sample, which selects index zero); no duplicate sample indices.
    pub fn traverse_sampled(&self, sample_count: u64) -> Result<TraversalReport, String> {
        if sample_count == 0 || sample_count > self.total {
            return Err(format!("sample_count must be in 1..={}", self.total));
        }
        let start = Instant::now();
        let mut report = TraversalReport {
            visited: 0,
            correct: 0,
            unreachable: 0,
            mismatch: 0,
            elapsed_ms: 0,
            elapsed_ns: 0,
            materialized_bytes: None,
            peak_materialized_nodes: 0,
        };
        let mut offset = 0;
        while offset < sample_count {
            let end = (offset + BATCH_SIZE as u64).min(sample_count);
            let indices: Vec<u64> = (offset..end)
                .map(|i| {
                    if sample_count == 1 {
                        0
                    } else {
                        ((i as u128 * (self.total - 1) as u128) / (sample_count - 1) as u128) as u64
                    }
                })
                .collect();
            let directory = self.populate_directory(&indices)?;
            report.peak_materialized_nodes =
                report.peak_materialized_nodes.max(directory.total_nodes());
            for idx in indices {
                let addr = self.config.address_of(idx).expect("validated sample index");
                match self.verify(&addr, &directory) {
                    VerifyOutcome::Correct => report.correct += 1,
                    VerifyOutcome::OutOfRealm => report.unreachable += 1,
                    VerifyOutcome::Mismatch => report.mismatch += 1,
                }
                report.visited += 1;
            }
            offset = end;
        }
        report.elapsed_ns = start.elapsed().as_nanos();
        report.elapsed_ms = report.elapsed_ns / 1_000_000;
        Ok(report)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOutcome {
    Correct,
    OutOfRealm,
    Mismatch,
}

fn parse_canonical_u64(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}
