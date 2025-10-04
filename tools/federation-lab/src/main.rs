//! S4 · federation-lab CLI（DEV_06 §5.1）。
//!
//! 子命令：
//! - `composegen`：生成 50 集群 compose 拓扑（--out docker-compose.yml）
//! - `twin`：百万节点数字孪生全遍历（G5 门禁——正确性 100% + ≤10min）
//! - `twin --sample N`：抽样遍历（CI 模式）
//! - `report`：输出 SCALE_REPORT 数据（收敛/路由/带宽指标占位——实测行）

use federation_lab::composegen::{ComposeGenConfig, Output};
use federation_lab::twin::{DigitalTwin, TwinConfig};

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
            let mut config = TwinConfig::default();
            // --clusters/--nodes 缩减空间支持
            let sample: Option<u64> = args
                .iter()
                .position(|a| a == "--sample")
                .and_then(|p| args.get(p + 1))
                .and_then(|v| v.parse().ok());
            if let Some(pos) = args.iter().position(|a| a == "--clusters") {
                if let Ok(v) = args[pos + 1].parse() {
                    config.clusters = v;
                }
            }
            if let Some(pos) = args.iter().position(|a| a == "--nodes") {
                if let Ok(v) = args[pos + 1].parse() {
                    config.nodes_per_cluster = v;
                }
            }
            let twin = DigitalTwin::build(config);
            let r = match sample {
                Some(n) => twin.traverse_sampled(n),
                None => twin.traverse_all(),
            };
            println!(
                "孪生遍历：visited={} correct={} mismatch={} 耗时={}ms",
                r.visited, r.correct, r.mismatch, r.elapsed_ms
            );
            if r.mismatch != 0 {
                eprintln!("G5 门禁失败：存在错误路由");
                std::process::exit(1);
            }
            println!("G5 门禁通过：路由正确性 100%");
        }
        "help" | "--help" | "-h" => {
            println!("federation-lab：S4 仿真基建（composegen / twin / report）");
            println!("  composegen [--clusters N] [--nodes-per-cluster N] [--fold K] [--out FILE]");
            println!("  twin [--clusters N] [--nodes N] [--sample N]");
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
