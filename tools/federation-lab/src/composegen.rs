//! S4 · federation-lab composegen（DEV_06 §5.1）。
//!
//! 50×200 个逻辑节点按4:1规划为2500个容器。输出为JSON（YAML 1.2子集），
//! 包含容器和折叠数量；这不是已接通supervisor、节点发现及RR会话的部署证明。
//! 完整折叠运行与网络规模验收仍属于DEV_06交付目标。
//!
//! 用法：cargo run --manifest-path tools/federation-lab/Cargo.toml -- composegen
//!      [--clusters N] [--nodes-per-cluster N] [--fold K] [--out FILE]

use serde_json::json;
use std::{io, path::PathBuf};

pub const MAX_COMPOSE_CONTAINERS: usize = 10_000;
pub const MAX_COMPOSE_LOGICAL_NODES: usize = 1_000_000;
pub const MAX_FOLD: usize = 4;
const FIRST_HUB_PORT: usize = 7000;

/// 生成参数。
#[derive(Debug, Clone)]
pub struct ComposeGenConfig {
    pub clusters: usize,
    pub nodes_per_cluster: usize,
    /// 折叠比（每容器逻辑节点数——资源上限 4:1）。
    pub fold: usize,
    /// realm 名（federation 内唯一前缀）。
    pub realm: String,
    /// 输出对象（compose YAML 文本 + 拓扑统计）。
    pub output: Output,
}

/// 输出目标。
#[derive(Debug, Clone)]
pub enum Output {
    /// 返回 YAML 字符串（测试/孪生复用——不落盘）。
    Buffer,
    /// 落盘路径（CLI 用）。
    File(PathBuf),
}

/// 生成结果统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TopologyStats {
    pub clusters: usize,
    pub containers: usize,
    pub logical_nodes: usize,
    /// hub border 容器数（= clusters）。
    pub hubs: usize,
    /// RR 会话数（full-mesh C(clusters, 2)）。
    pub rr_sessions: usize,
}

/// 单容器定义。
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSpec {
    pub name: String,
    pub cluster: usize,
    /// 承载的逻辑节点数（fold 后）。
    pub logical_nodes: usize,
    /// 是否 hub border（每集群 1 个）。
    pub is_hub: bool,
    /// 对外暴露端口（hub 才有）。
    pub port: Option<u16>,
    /// 依赖（hub 先起）。
    pub depends_on: Vec<String>,
}

impl ComposeGenConfig {
    pub fn fifty_clusters() -> Self {
        Self {
            clusters: 50,
            nodes_per_cluster: 200,
            fold: 4,
            realm: "fed".into(),
            output: Output::Buffer,
        }
    }

    /// CI 缩减版（10 集群——07 §14.4）。
    pub fn ci_small() -> Self {
        Self {
            clusters: 10,
            nodes_per_cluster: 200,
            fold: 4,
            realm: "fed".into(),
            output: Output::Buffer,
        }
    }

    /// 容器总数（每集群 nodes_per_cluster/fold 容器，向上取整）。
    pub fn containers_per_cluster(&self) -> io::Result<usize> {
        self.stats()?;
        Ok(self.nodes_per_cluster.div_ceil(self.fold))
    }

    pub fn total_containers(&self) -> io::Result<usize> {
        Ok(self.stats()?.containers)
    }

    pub fn total_logical_nodes(&self) -> io::Result<usize> {
        Ok(self.stats()?.logical_nodes)
    }

    /// RR full-mesh 会话数。
    pub fn rr_sessions(&self) -> io::Result<usize> {
        Ok(self.stats()?.rr_sessions)
    }

    /// 展开全部容器（fold 模式：每容器 fold 个逻辑节点）。
    pub fn containers(&self) -> io::Result<Vec<ContainerSpec>> {
        let stats = self.stats()?;
        let per = self.nodes_per_cluster.div_ceil(self.fold);
        let mut out = Vec::with_capacity(stats.containers);
        for c in 0..self.clusters {
            for i in 0..per {
                let is_hub = i == 0;
                // 折叠：最后一容器可能少折（200 % 4 == 0 常态整除）
                let logical = if i + 1 < per {
                    self.fold
                } else {
                    self.nodes_per_cluster - (per - 1) * self.fold
                };
                out.push(ContainerSpec {
                    name: format!("{}-c{}-n{}", self.realm, c, i),
                    cluster: c,
                    logical_nodes: logical,
                    is_hub,
                    port: is_hub.then_some((FIRST_HUB_PORT + c) as u16),
                    depends_on: if is_hub {
                        vec![]
                    } else {
                        vec![format!("{}-c{}-n0", self.realm, c)]
                    },
                });
            }
        }
        Ok(out)
    }

    /// 生成 compose YAML（不落盘——测试断言拓扑结构）。
    pub fn render_yaml(&self) -> io::Result<String> {
        let mut services = serde_json::Map::new();
        for ct in self.containers()? {
            let mut service = json!({
                "image": "parrot-node:latest",
                "environment": {
                    "PARROT_CLUSTER": format!("c{}", ct.cluster),
                    "PARROT_NODES": ct.logical_nodes.to_string(),
                    "PARROT_REALM": self.realm,
                },
            });
            if ct.is_hub {
                let port = ct.port.ok_or_else(|| io::Error::other("hub has no port"))?;
                service["ports"] = json!([format!("{port}:{port}")]);
                service["command"] = json!(["parrot-node", "--role", "hub"]);
            } else {
                service["command"] = json!(["parrot-node", "--role", "worker"]);
                service["depends_on"] = json!(ct.depends_on);
            }
            services.insert(ct.name, service);
        }
        let document = json!({
            "x-parrot-model": {
                "scope": "topology-plan-only",
                "clusters": self.clusters,
                "nodes_per_cluster": self.nodes_per_cluster,
                "fold": self.fold,
                "realm": self.realm,
            },
            "services": services,
        });
        serde_json::to_string_pretty(&document)
            .map(|text| text + "\n")
            .map_err(io::Error::other)
    }

    /// 拓扑统计（G5 门禁记录用——不依赖 Docker 存在）。
    pub fn stats(&self) -> io::Result<TopologyStats> {
        let invalid = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
        if self.clusters == 0 || self.nodes_per_cluster == 0 {
            return Err(invalid("clusters and nodes_per_cluster must be positive"));
        }
        if !(1..=MAX_FOLD).contains(&self.fold) {
            return Err(invalid("fold must be in 1..=4"));
        }
        if self.clusters > usize::from(u16::MAX) - FIRST_HUB_PORT + 1 {
            return Err(invalid("hub ports exceed 65535"));
        }
        if self.realm.is_empty()
            || self.realm.len() > 32
            || !self
                .realm
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || self.realm.starts_with('-')
            || self.realm.ends_with('-')
        {
            return Err(invalid(
                "realm must contain 1..=32 lowercase ASCII letters, digits or inner hyphens",
            ));
        }
        if matches!(&self.output, Output::File(path) if path.as_os_str().is_empty()) {
            return Err(invalid("output path must not be empty"));
        }
        let containers = self
            .nodes_per_cluster
            .div_ceil(self.fold)
            .checked_mul(self.clusters)
            .filter(|&count| count <= MAX_COMPOSE_CONTAINERS)
            .ok_or_else(|| invalid("container count exceeds 10000"))?;
        let logical_nodes = self
            .nodes_per_cluster
            .checked_mul(self.clusters)
            .filter(|&count| count <= MAX_COMPOSE_LOGICAL_NODES)
            .ok_or_else(|| invalid("logical node count exceeds 1000000"))?;
        let rr_sessions = self
            .clusters
            .checked_mul(self.clusters - 1)
            .ok_or_else(|| invalid("RR session count overflow"))?
            / 2;
        Ok(TopologyStats {
            clusters: self.clusters,
            containers,
            logical_nodes,
            hubs: self.clusters,
            rr_sessions,
        })
    }

    /// 落盘（CLI 模式）。
    pub fn write_out(&self) -> std::io::Result<(TopologyStats, usize)> {
        let stats = self.stats()?;
        match &self.output {
            Output::Buffer => Ok((stats, 0)),
            Output::File(path) => {
                let y = self.render_yaml()?;
                std::fs::write(path, &y)?;
                Ok((stats, y.len()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S4-a：50×200 拓扑规模正确（折叠 4:1 → 2500 容器）
    #[test]
    fn s4_topology_scale() {
        let cfg = ComposeGenConfig::fifty_clusters();
        let s = cfg.stats().unwrap();
        assert_eq!(s.clusters, 50);
        assert_eq!(s.logical_nodes, 10_000);
        assert_eq!(s.containers, 2500);
        assert_eq!(s.hubs, 50);
        // RR full-mesh：C(50,2) = 1225
        assert_eq!(s.rr_sessions, 1225);
    }

    /// S4-b：折叠语义（200 节点/集群 ÷ 4 = 50 容器/集群，每容器 4 逻辑节点）
    #[test]
    fn s4_fold_semantics() {
        let cfg = ComposeGenConfig::fifty_clusters();
        assert_eq!(cfg.containers_per_cluster().unwrap(), 50);
        let containers = cfg.containers().unwrap();
        assert_eq!(containers.len(), 2_500);
        // 每集群首容器是 hub
        let hubs: Vec<_> = containers.iter().filter(|c| c.is_hub).collect();
        assert_eq!(hubs.len(), 50);
        // 非首容器依赖 hub
        assert!(containers[1].depends_on.contains(&"fed-c0-n0".to_string()));
        // 逻辑节点守恒：全部容器求和 == 50×200
        let total: usize = containers.iter().map(|c| c.logical_nodes).sum();
        assert_eq!(total, 10_000);
    }

    /// S4-c：非整除折叠（199 节点 ÷ 4 → 50 容器，末容器 3 节点）
    #[test]
    fn s4_fold_remainder() {
        let cfg = ComposeGenConfig {
            nodes_per_cluster: 199,
            ..ComposeGenConfig::fifty_clusters()
        };
        let containers = cfg.containers().unwrap();
        // 每集群 50 容器（div_ceil(199,4)=50）
        assert_eq!(containers.len(), 50 * 50);
        let total: usize = containers.iter().map(|c| c.logical_nodes).sum();
        assert_eq!(total, 199 * 50);
        // 末容器 3 逻辑节点
        assert_eq!(containers[49].logical_nodes, 3);
    }

    /// S4-d：解析完整结构，不以子串证明Compose字段类型正确。
    #[test]
    fn s4_yaml_structure() {
        let y = ComposeGenConfig::ci_small().render_yaml().unwrap();
        let value: serde_json::Value = serde_json::from_str(&y).unwrap();
        let services = value["services"].as_object().unwrap();
        assert_eq!(services.len(), 500);
        assert_eq!(services["fed-c0-n0"]["ports"], json!(["7000:7000"]));
        assert_eq!(services["fed-c0-n1"]["depends_on"], json!(["fed-c0-n0"]));
        assert_eq!(
            services["fed-c0-n0"]["environment"],
            json!({
                "PARROT_CLUSTER": "c0", "PARROT_NODES": "4", "PARROT_REALM": "fed"
            })
        );
    }

    /// S4-e：CI 缩减版（10 集群——CI 资源限制）
    #[test]
    fn s4_ci_small() {
        let s = ComposeGenConfig::ci_small().stats().unwrap();
        assert_eq!(s.clusters, 10);
        assert_eq!(s.containers, 500);
        assert_eq!(s.logical_nodes, 2_000);
    }
}
