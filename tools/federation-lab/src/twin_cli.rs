use federation_lab::twin::{DigitalTwin, TwinConfig};
use std::collections::HashSet;

/// Strict parsing keeps malformed or empty workloads from producing a success
/// report. The twin library validates ranges before constructing any state.
pub fn run(args: &[String]) -> Result<(), String> {
    let (config, sample) = parse(args)?;
    let twin = DigitalTwin::build(config)?;
    let count = sample.unwrap_or(twin.total_nodes());
    let report = twin.traverse_sampled(count)?;
    println!(
        "本地目录遍历：visited={} correct={} unreachable={} mismatch={} 耗时={}ms peak_nodes={}",
        report.visited,
        report.correct,
        report.unreachable,
        report.mismatch,
        report.elapsed_ms,
        report.peak_materialized_nodes
    );
    if !report.passed(count) {
        return Err("目录契约失败：访问计数或解析结果不符合预期".into());
    }
    println!(
        "本地目录契约通过：{count}/{} 地址，单副本分片、分批物化验证",
        twin.total_nodes()
    );
    Ok(())
}

fn parse(args: &[String]) -> Result<(TwinConfig, Option<u64>), String> {
    let mut config = TwinConfig::default();
    let mut sample = None;
    let mut seen = HashSet::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if !["--clusters", "--nodes", "--shards", "--realm", "--sample"].contains(&flag.as_str()) {
            return Err(format!("unknown twin option: {flag}"));
        }
        if !seen.insert(flag) {
            return Err(format!("duplicate option: {flag}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        let invalid = || format!("invalid value for {flag}: {value:?}");
        match flag.as_str() {
            "--clusters" => config.clusters = value.parse().map_err(|_| invalid())?,
            "--nodes" => config.nodes_per_cluster = value.parse().map_err(|_| invalid())?,
            "--shards" => config.directory_shards = value.parse().map_err(|_| invalid())?,
            "--realm" => config.realm = value.clone(),
            "--sample" => sample = Some(value.parse().map_err(|_| invalid())?),
            _ => unreachable!("validated option"),
        }
    }
    let total = config.total_nodes()?;
    if sample.is_some_and(|n| n == 0 || n > total) {
        return Err(format!("sample must be in 1..={total}"));
    }
    Ok((config, sample))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_arguments_fail_instead_of_using_default_workloads() {
        for input in [
            vec!["--clusters"],
            vec!["--nodes"],
            vec!["--sample"],
            vec!["--nodes", "wrong"],
            vec!["--sample", "wrong"],
            vec!["--sample", "0"],
            vec!["--nodes", "0"],
            vec!["--clusters", "0"],
            vec!["--shards", "0"],
            vec!["--shards", "257"],
            vec!["--realm", "a/b"],
            vec!["--clustrs", "1"],
            vec!["--nodes", "-1"],
            vec!["--nodes", "1", "--nodes", "2"],
            vec!["--clusters", "1", "--nodes", "1", "--sample", "2"],
            vec!["--nodes", "18446744073709551616"],
            vec!["stray"],
        ] {
            let args = input.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert!(parse(&args).is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn valid_options_preserve_requested_scope() {
        let args = [
            "--clusters",
            "3",
            "--nodes",
            "9",
            "--shards",
            "2",
            "--realm",
            "test-1",
            "--sample",
            "4",
        ]
        .map(String::from);
        let (cfg, sample) = parse(&args).unwrap();
        assert_eq!(cfg.total_nodes().unwrap(), 27);
        assert_eq!(cfg.directory_shards, 2);
        assert_eq!(cfg.realm, "test-1");
        assert_eq!(sample, Some(4));
    }
}
