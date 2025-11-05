//! S4 · federation-lab CLI（DEV_06 §5.1）。
//!
//! 子命令：
//! - `composegen`：生成 50 集群 compose 拓扑（--out docker-compose.yml）
//! - `twin`：数字孪生本地目录契约（分批注册/提交/查询）
//! - `twin --sample N`：抽样遍历（CI 模式）
//! - `report`：输出 SCALE_REPORT 数据（收敛/路由/带宽指标占位——实测行）

use federation_lab::composegen::{ComposeGenConfig, Output};
mod twin_cli;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");
    match cmd {
        "composegen" => {
            let mut cfg = ComposeGenConfig::fifty_clusters();
            parse_common(
                &args,
                &mut cfg.clusters,
                &mut cfg.nodes_per_cluster,
                &mut cfg.fold,
            );
            if let Some(pos) = args.iter().position(|a| a == "--out") {
                cfg.output = Output::File(args[pos + 1].clone());
            }
            match cfg.write_out() {
                Ok((stats, bytes)) => {
                    println!("composegen 完成：{stats:?}（{bytes}B）");
                }
                Err(e) => {
                    eprintln!("写盘失败：{e}");
                    std::process::exit(1);
                }
            }
        }
        "twin" => {
            if let Err(error) = twin_cli::run(&args[2..]) {
                eprintln!("twin failed: {error}");
                std::process::exit(2);
            }
        }
        "twin-app" => {
            // F4（DEV_09 §3.6）：App 孪生门禁——Manifest 输入
            let path = args
                .iter()
                .position(|a| a == "--manifest")
                .and_then(|p| args.get(p + 1))
                .cloned()
                .unwrap_or_else(|| "crawler.app.toml".into());
            let m = match parrot_app::manifest::AppManifest::from_file(std::path::Path::new(&path))
            {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("manifest 读取失败（{path}）：{e}");
                    std::process::exit(1);
                }
            };
            let r = federation_lab::twin_app::twin_app(&m);
            println!(
                "App 孪生门禁 [{}]：components={} wires={} topo={:?}",
                r.app, r.components, r.wires, r.topo_order
            );
            println!("  节点映射：{:?}", r.node_map);
            println!("  升级矩阵：{:?}", r.upgrade_matrix);
            if !r.passed {
                for f in &r.failures {
                    eprintln!("门禁失败：{f}");
                }
                std::process::exit(1);
            }
            println!("App 孪生门禁通过（≤100 组件 / DAG / 全分支枚举）");
        }
        "help" | "--help" | "-h" => {
            println!("federation-lab：S4 仿真基建（composegen / twin / report / twin-app）");
            println!("  composegen [--clusters N] [--nodes-per-cluster N] [--fold K] [--out FILE]");
            println!("  twin [--clusters N] [--nodes N] [--sample N] [--shards N] [--realm NAME]");
            println!("  twin-app [--manifest FILE]");
        }
        other => {
            eprintln!("未知子命令：{other}（federation-lab help 查看）");
            std::process::exit(1);
        }
    }
}

fn parse_common(
    args: &[String],
    clusters: &mut usize,
    nodes_per_cluster: &mut usize,
    fold: &mut usize,
) {
    let flag = |name: &str| -> Option<usize> {
        args.iter()
            .position(|a| a == name)
            .and_then(|p| args.get(p + 1))
            .and_then(|v| v.parse().ok())
    };
    if let Some(v) = flag("--clusters") {
        *clusters = v;
    }
    if let Some(v) = flag("--nodes-per-cluster") {
        *nodes_per_cluster = v;
    }
    if let Some(v) = flag("--fold") {
        *fold = v;
    }
}
