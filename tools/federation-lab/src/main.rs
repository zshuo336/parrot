//! Local directory checks and topology planning; network scale gates remain separate.
use federation_lab::composegen::{ComposeGenConfig, Output};
use std::{collections::HashSet, ffi::OsString, io::Write, path::PathBuf};
mod twin_cli;

fn main() -> std::process::ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("federation-lab: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

fn run(args: Vec<OsString>) -> Result<(), String> {
    let command = args.first().map(|arg| arg.to_str());
    let rest = args.get(1..).unwrap_or_default();
    match command {
        Some(Some("composegen")) => {
            let cfg = parse_compose(rest)?;
            match cfg.output {
                Output::Buffer => {
                    let text = cfg.render_yaml().map_err(|error| error.to_string())?;
                    std::io::stdout()
                        .lock()
                        .write_all(text.as_bytes())
                        .map_err(|error| error.to_string())?;
                }
                Output::File(_) => {
                    let (stats, bytes) = cfg.write_out().map_err(|error| error.to_string())?;
                    eprintln!("composegen: {stats:?}, {bytes} bytes (topology plan; network runtime not certified)");
                }
            }
            Ok(())
        }
        Some(Some("twin")) => {
            let args = rest
                .iter()
                .map(|arg| {
                    arg.clone()
                        .into_string()
                        .map_err(|_| "twin options must be UTF-8".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            twin_cli::run(&args)
        }
        Some(Some("twin-app")) => {
            let path = parse_manifest(rest)?;
            let manifest = parrot_app::manifest::AppManifest::from_file(&path)
                .map_err(|error| format!("manifest {}: {error}", path.display()))?;
            let result = federation_lab::twin_app::twin_app(&manifest);
            if !result.passed {
                return Err(format!("app model failed: {:?}", result.failures));
            }
            println!(
                "App model passed [{}]: components={} wires={} topo={:?}",
                result.app, result.components, result.wires, result.topo_order
            );
            println!("  node map: {:?}", result.node_map);
            println!("  upgrade matrix: {:?}", result.upgrade_matrix);
            Ok(())
        }
        None | Some(Some("help" | "--help" | "-h")) if rest.is_empty() => {
            println!("federation-lab: local directory checks and topology planning");
            println!("  composegen [--clusters N] [--nodes-per-cluster N] [--fold 1..4] [--realm NAME] [--out FILE]");
            println!("  twin [--clusters N] [--nodes N] [--sample N] [--shards N] [--realm NAME]");
            println!("  twin-app [--manifest FILE]");
            Ok(())
        }
        _ => Err("unknown command or extra arguments; run federation-lab help".into()),
    }
}

fn parse_compose(args: &[OsString]) -> Result<ComposeGenConfig, String> {
    let mut cfg = ComposeGenConfig::fifty_clusters();
    let mut seen = HashSet::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let flag = flag.to_str().ok_or("option names must be UTF-8")?;
        if ![
            "--clusters",
            "--nodes-per-cluster",
            "--fold",
            "--realm",
            "--out",
        ]
        .contains(&flag)
        {
            return Err(format!("unknown composegen option: {flag}"));
        }
        if !seen.insert(flag) {
            return Err(format!("duplicate option: {flag}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        if value.is_empty() || value.to_str().is_some_and(|text| text.starts_with("--")) {
            return Err(format!("missing value for {flag}"));
        }
        if flag == "--out" {
            cfg.output = Output::File(PathBuf::from(value));
            continue;
        }
        let value = value
            .to_str()
            .ok_or_else(|| format!("{flag} must be UTF-8"))?;
        if flag == "--realm" {
            cfg.realm = value.to_string();
            continue;
        }
        if !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!("{flag} requires an unsigned decimal integer"));
        }
        let value = value
            .parse()
            .map_err(|_| format!("{flag} integer overflow"))?;
        match flag {
            "--clusters" => cfg.clusters = value,
            "--nodes-per-cluster" => cfg.nodes_per_cluster = value,
            "--fold" => cfg.fold = value,
            _ => unreachable!("validated option"),
        }
    }
    cfg.stats().map_err(|error| error.to_string())?;
    Ok(cfg)
}

fn parse_manifest(args: &[OsString]) -> Result<PathBuf, String> {
    match args {
        [] => Ok(PathBuf::from("apps/crawler-lab/crawler.app.toml")),
        [flag, path]
            if flag == "--manifest"
                && !path.is_empty()
                && !path.to_str().is_some_and(|text| text.starts_with("--")) =>
        {
            Ok(PathBuf::from(path))
        }
        _ => Err("twin-app accepts exactly one optional --manifest <path>".into()),
    }
}
