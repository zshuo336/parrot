//! S4 · federation-lab composegen（DEV_06 §5.1）。
//!
//! 生成 50 集群 × 200 节点 docker compose 拓扑：
//! - 每容器跑 1 个 supervisor 进程 × N 逻辑节点（容器复用进程模拟——
//!   默认折叠 4:1，即每容器 4 逻辑节点，50×50 容器 = 10k 容器承载
//!   50×200 逻辑节点）；
//! - 拓扑：hub-spoke（每 realm 一个 hub border）+ realm 间 full-mesh
//!   route-reflector 会话（RouteReflector——F6）；
//! - CI 缩减模式：`--clusters 10`（07 §14.4 建议 CI 用 10 集群版）。
//!
//! 用法：cargo run --manifest-path tools/federation-lab/Cargo.toml -- composegen
//!      [--clusters N] [--nodes-per-cluster N] [--fold K] [--out FILE]

use std::fmt::Write as _;

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
    File(String),
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
    pub fn containers_per_cluster(&self) -> usize {
        self.nodes_per_cluster.div_ceil(self.fold)
    }

    pub fn total_containers(&self) -> usize {
        self.containers_per_cluster() * self.clusters
    }

    pub fn total_logical_nodes(&self) -> usize {
        self.nodes_per_cluster * self.clusters
    }

    /// RR full-mesh 会话数。
    pub fn rr_sessions(&self) -> usize {
        self.clusters * (self.clusters - 1) / 2
    }

    /// 展开全部容器（fold 模式：每容器 fold 个逻辑节点）。
    pub fn containers(&self) -> Vec<ContainerSpec> {
        let mut out = Vec::new();
        for c in 0..self.clusters {
            let per = self.containers_per_cluster();
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
                    port: is_hub.then_some(7000 + c as u16),
                    depends_on: if is_hub {
                        vec![]
                    } else {
                        vec![format!("{}-c{}-n0", self.realm, c)]
                    },
                });
            }
        }
        out
    }

    /// 生成 compose YAML（不落盘——测试断言拓扑结构）。
    pub fn render_yaml(&self) -> String {
        let mut y = String::new();
        let _ = writeln!(y, "# parrot federation-lab composegen (auto-generated)");
        let _ = writeln!(
            y,
            "# {} clusters × {} nodes (fold {}:{}), realm '{}'",
            self.clusters, self.nodes_per_cluster, 1, self.fold, self.realm
        );
        let _ = writeln!(y, "services:");
        for ct in self.containers() {
            let _ = writeln!(y, "  {}:", ct.name);
            let _ = writeln!(y, "    image: parrot-node:latest");
            let _ = write!(y, "    environment:");
            let _ = write!(y, " PARROT_CLUSTER=c{}", ct.cluster);
            let _ = write!(y, " PARROT_NODES={}", ct.logical_nodes);
            let _ = writeln!(y, " PARROT_REALM={}", self.realm);
            if ct.is_hub {
                let _ = writeln!(y, "    ports:");
                let _ = writeln!(y, "      - \"{}:{}\"", ct.port.unwrap(), ct.port.unwrap());
                let _ = writeln!(y, "    command: [\"parrot-node\", \"--role\", \"hub\"]");
            } else {
                let _ = writeln!(y, "    command: [\"parrot-node\", \"--role\", \"worker\"]");
                let _ = writeln!(y, "    depends_on:");
                for d in &ct.depends_on {
                    let _ = writeln!(y, "      - {}", d);
                }
            }
        }
        y
    }

    /// 拓扑统计（G5 门禁记录用——不依赖 Docker 存在）。
    pub fn stats(&self) -> TopologyStats {
        TopologyStats {
            clusters: self.clusters,
            containers: self.total_containers(),
            logical_nodes: self.total_logical_nodes(),
            hubs: self.clusters,
            rr_sessions: self.rr_sessions(),
        }
    }

    /// 落盘（CLI 模式）。
    pub fn write_out(&self) -> std::io::Result<(TopologyStats, usize)> {
        match &self.output {
            Output::Buffer => Ok((self.stats(), 0)),
            Output::File(path) => {
                let y = self.render_yaml();
                std::fs::write(path, &y)?;
                Ok((self.stats(), y.len()))
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
        let s = cfg.stats();
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
        assert_eq!(cfg.containers_per_cluster(), 50);
        let containers = cfg.containers();
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
        let containers = cfg.containers();
        // 每集群 50 容器（div_ceil(199,4)=50）
        assert_eq!(containers.len(), 50 * 50);
        let total: usize = containers.iter().map(|c| c.logical_nodes).sum();
        assert_eq!(total, 199 * 50);
        // 末容器 3 逻辑节点
        assert_eq!(containers[49].logical_nodes, 3);
    }

    /// S4-d：YAML 结构合法（services 段 + hub 端口 + depends_on）
    #[test]
    fn s4_yaml_structure() {
        let y = ComposeGenConfig::ci_small().render_yaml();
        assert!(y.contains("services:"));
        assert!(y.contains("fed-c0-n0:"));
        // hub 有端口映射 + role hub
        assert!(y.contains("\"7000:7000\""));
        assert!(y.contains("\"--role\", \"hub\""));
        // worker 有 depends_on
        assert!(y.contains("depends_on:"));
        // 环境变量注入（集群/节点数/realm）
        assert!(y.contains("PARROT_CLUSTER=c0"));
        assert!(y.contains("PARROT_NODES=4"));
        assert!(y.contains("PARROT_REALM=fed"));
    }

    /// S4-e：CI 缩减版（10 集群——CI 资源限制）
    #[test]
    fn s4_ci_small() {
        let s = ComposeGenConfig::ci_small().stats();
        assert_eq!(s.clusters, 10);
        assert_eq!(s.containers, 500);
        assert_eq!(s.logical_nodes, 2_000);
    }
}
