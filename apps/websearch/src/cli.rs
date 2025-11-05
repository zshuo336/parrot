//! Validate the complete invocation before touching storage or starting services.
use std::{collections::HashSet, net::SocketAddr, path::PathBuf};

pub(super) const HELP: &str =
    "websearch [HTTP(S)-seed ...] [erl=IP:PORT] [ray=IP:PORT] [jvm=IP:PORT]
  --manifest FILE Run the local application host from this manifest
  --runtime-root DIR  Local gateway runtime root for --manifest (installed releases auto-detect)
  --gateway-process-limit N  Maximum owned gateway/compiler groups for --manifest (default 64)
  --pages N        Maximum fetch attempts, including failures (default 50000; N > 0)
  --sites N        Successful host[:port] target (default 100; N > 0)
  --maxdepth N     Link depth limit (default 10; 0..65535)
  --port N         Web port (default 8080; 0 asks the OS for a port)
  --data DIR       Local journal/identity directory (default ./data)
  --index-data DIR Absolute data directory on the JVM gateway; persisted for later runs
  --keep           Preserve existing index and discovered-URL set
  --serve-only     Serve the existing index, without crawling
  --bind IP:PORT   Wait for gateways to register (port 0 supported)
  --wait N         Registration deadline in seconds (default 60; 1..86400)
  --node-root DIR  Absolute artifact directory on the gateways
  --crawl-origin URL  Allow this exact scheme/host/port; repeat for multiple origins;
                      explicitly named IP origins may use private addresses
  --insecure-development  Allow unauthenticated remote gateways (development only)
  --validate-only  Validate arguments and exit before storage or network changes
  --help, -h       Show this help
  --              Treat all following arguments as seed URLs
Without --keep or --serve-only, a crawl starts a fresh index in the selected JVM data directory.
With no seeds, or after exhausting them before the target, built-in seeds are used.
With --crawl-origin, listed roots are default seeds and built-in refill is disabled.
Start gateways separately or use apps/websearch/run.sh.";

#[derive(Debug, Clone)]
pub(super) struct Options {
    pub manifest: Option<PathBuf>,
    pub runtime_root: Option<PathBuf>,
    pub gateway_process_limit: usize,
    pub explicit: HashSet<String>,
    pub seeds: Vec<String>,
    pub pages: u64,
    pub sites_target: usize,
    pub max_depth: u16,
    pub web_port: u16,
    pub data_dir: PathBuf,
    pub index_data: Option<String>,
    pub gw_addrs: Vec<(String, String)>,
    pub serve_only: bool,
    pub keep_data: bool,
    pub bind_addr: Option<SocketAddr>,
    pub wait_secs: u64,
    pub node_root: Option<PathBuf>,
    pub crawl_origins: Vec<url::Url>,
    pub insecure_development: bool,
    pub validate_only: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            manifest: None,
            runtime_root: None,
            gateway_process_limit: parrot_app::host::GatewayProcessBudget::DEFAULT_MAX_GROUPS,
            explicit: HashSet::new(),
            seeds: Vec::new(),
            pages: 50_000,
            sites_target: 100,
            max_depth: 10,
            web_port: 8080,
            data_dir: PathBuf::from("./data"),
            index_data: None,
            gw_addrs: Vec::new(),
            serve_only: false,
            keep_data: false,
            bind_addr: None,
            wait_secs: 60,
            node_root: None,
            crawl_origins: Vec::new(),
            insecure_development: false,
            validate_only: false,
        }
    }
}

fn seed(options: &mut Options, value: &str) -> Result<(), String> {
    let url = super::robots::crawl_url(value).map_err(|e| format!("invalid seed URL: {e}"))?;
    options.explicit.insert("seed".into());
    options.seeds.push(url.to_string());
    Ok(())
}

pub(super) fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
    let mut options = Options::default();
    let mut seen = HashSet::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(None);
        }
        if arg == "--" {
            for value in args {
                seed(&mut options, &value)?;
            }
            break;
        }
        if arg.starts_with("--") {
            if arg != "--crawl-origin" && !seen.insert(arg.clone()) {
                return Err(format!("duplicate option {arg}"));
            }
            match arg.as_str() {
                "--insecure-development" => options.insecure_development = true,
                "--validate-only" => options.validate_only = true,
                "--keep" => options.keep_data = true,
                "--serve-only" => options.serve_only = true,
                "--pages"
                | "--sites"
                | "--maxdepth"
                | "--port"
                | "--data"
                | "--bind"
                | "--wait"
                | "--node-root"
                | "--crawl-origin"
                | "--index-data"
                | "--manifest"
                | "--runtime-root"
                | "--gateway-process-limit" => {
                    let value = args
                        .next()
                        .filter(|s| !s.is_empty() && !s.starts_with("--"))
                        .ok_or_else(|| format!("{arg} requires a value"))?;
                    let invalid = || format!("invalid value for {arg}");
                    match arg.as_str() {
                        "--crawl-origin" => {
                            let url = url::Url::parse(&value).map_err(|_| invalid())?;
                            super::robots::crawl_url(&value).map_err(|_| invalid())?;
                            if url.path() != "/"
                                || url.query().is_some()
                                || url.fragment().is_some()
                                || url.port() == Some(0)
                            {
                                return Err("--crawl-origin requires an HTTP(S) origin without a path, query or fragment".into());
                            }
                            if options
                                .crawl_origins
                                .iter()
                                .any(|other| other.origin() == url.origin())
                            {
                                return Err("duplicate --crawl-origin".into());
                            }
                            options.explicit.insert("--crawl-origin".into());
                            options.crawl_origins.push(url);
                        }
                        "--pages" => options.pages = value.parse().map_err(|_| invalid())?,
                        "--sites" => options.sites_target = value.parse().map_err(|_| invalid())?,
                        "--maxdepth" => options.max_depth = value.parse().map_err(|_| invalid())?,
                        "--port" => options.web_port = value.parse().map_err(|_| invalid())?,
                        "--wait" => options.wait_secs = value.parse().map_err(|_| invalid())?,
                        "--manifest" => options.manifest = Some(PathBuf::from(value)),
                        "--gateway-process-limit" => {
                            let limit = value.parse().map_err(|_| invalid())?;
                            parrot_app::host::GatewayProcessBudget::new(limit)
                                .map_err(|e| e.to_string())?;
                            options.gateway_process_limit = limit;
                        }
                        "--runtime-root" => {
                            let path = PathBuf::from(value);
                            if !path.is_absolute() {
                                return Err("--runtime-root requires an absolute local path".into());
                            }
                            options.runtime_root = Some(path);
                        }
                        "--data" => options.data_dir = PathBuf::from(value),
                        "--index-data" => {
                            validate_index_data(&value)?;
                            options.index_data = Some(value);
                        }
                        "--node-root" => {
                            let path = PathBuf::from(value);
                            if !path.is_absolute() {
                                return Err(
                                    "--node-root requires an absolute gateway artifact path".into(),
                                );
                            }
                            options.node_root = Some(path);
                        }
                        "--bind" => options.bind_addr = Some(value.parse().map_err(|_| invalid())?),
                        _ => unreachable!(),
                    }
                }
                _ => return Err(format!("unknown option {arg}; use --help")),
            }
        } else if let Some((tag, value)) = arg
            .split_once('=')
            .filter(|(tag, _)| matches!(*tag, "erl" | "ray" | "jvm"))
        {
            if !seen.insert(tag.to_string()) {
                return Err(format!("duplicate gateway {tag}"));
            }
            let address: SocketAddr = value
                .parse()
                .map_err(|_| format!("{tag} requires IP:PORT"))?;
            if address.port() == 0 || address.ip().is_unspecified() {
                return Err(format!(
                    "{tag} requires a concrete address with a nonzero port"
                ));
            }
            options
                .gw_addrs
                .push((tag.to_string(), address.to_string()));
        } else {
            seed(&mut options, &arg)?;
        }
    }
    if options.pages == 0 || options.sites_target == 0 || options.wait_secs == 0 {
        return Err("--pages, --sites and --wait must be greater than zero".into());
    }
    // Instant arithmetic has a platform-dependent finite range. This is a
    // registration deadline, not a duration for an indefinitely running app.
    if options.wait_secs > 86_400 {
        return Err("--wait must not exceed 86400 seconds".into());
    }
    if options.serve_only {
        if !options.seeds.is_empty() {
            return Err("--serve-only does not accept crawl seeds".into());
        }
        options.keep_data = true;
    }
    if !options.crawl_origins.is_empty() {
        for value in &options.seeds {
            if !options.crawl_origins.iter().any(|origin| {
                origin.origin()
                    == super::robots::crawl_url(value)
                        .expect("validated seed")
                        .origin()
            }) {
                return Err("crawl seed is outside --crawl-origin".into());
            }
        }
        if options.seeds.is_empty() && !options.serve_only {
            options
                .seeds
                .extend(options.crawl_origins.iter().map(ToString::to_string));
        }
    }
    if options.runtime_root.is_some() && options.manifest.is_none() {
        return Err("--runtime-root requires --manifest".into());
    }
    if seen.contains("--gateway-process-limit") && options.manifest.is_none() {
        return Err("--gateway-process-limit requires --manifest".into());
    }
    options.explicit.extend(seen);
    Ok(Some(options))
}

pub(super) fn validate_index_data(path: &str) -> Result<(), String> {
    if path.len() > 4096 || path.contains('\0') || !std::path::Path::new(path).is_absolute() {
        return Err("--index-data requires an absolute gateway path of at most 4096 UTF-8 bytes without NUL".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Result<Option<Options>, String> {
        parse(values.iter().map(|s| s.to_string()))
    }

    #[test]
    fn remote_index_path_is_literal_bounded_and_has_no_local_io_requirement() {
        let path = "/not-created/quotes\" slash\\ newline\n中文";
        assert_eq!(
            args(&["--index-data", path])
                .unwrap()
                .unwrap()
                .index_data
                .as_deref(),
            Some(path)
        );
        for path in ["", "relative", "/bad\0path"] {
            assert!(validate_index_data(path).is_err());
        }
        assert!(validate_index_data(&format!("/{}", "a".repeat(4095))).is_ok());
        assert!(validate_index_data(&format!("/{}", "a".repeat(4096))).is_err());
        assert!(validate_index_data(&format!("/{}", "中".repeat(1366))).is_err());
    }

    #[test]
    fn restricted_crawl_origin_is_explicit_and_normalized() {
        let o = args(&["--crawl-origin", "HTTP://127.0.0.1:80"])
            .unwrap()
            .unwrap();
        assert_eq!(o.seeds, ["http://127.0.0.1/"]);
        assert_eq!(o.crawl_origins[0].as_str(), "http://127.0.0.1/");
        assert!(args(&["--crawl-origin", "http://[::1]:1234", "http://[::1]:1234/a"]).is_ok());
        assert!(
            args(&["--crawl-origin", "http://127.0.0.1", "--serve-only"])
                .unwrap()
                .unwrap()
                .seeds
                .is_empty()
        );
        assert!(args(&[]).unwrap().unwrap().crawl_origins.is_empty());
    }

    #[test]
    fn multiple_origins_validate_all_seeds_and_preserve_default_order() {
        let flags = [
            "--crawl-origin",
            "http://127.0.0.1:1234",
            "--crawl-origin",
            "https://example.test",
        ];
        let o = args(&flags).unwrap().unwrap();
        assert_eq!(o.crawl_origins.len(), 2);
        assert_eq!(o.seeds, ["http://127.0.0.1:1234/", "https://example.test/"]);
        let mut explicit = flags.to_vec();
        explicit.extend(["https://example.test/page", "http://127.0.0.1:1234/second"]);
        assert_eq!(
            args(&explicit).unwrap().unwrap().seeds,
            ["https://example.test/page", "http://127.0.0.1:1234/second"]
        );
        explicit.push("http://127.0.0.1:4321/outside");
        assert!(args(&explicit).is_err());
        assert!(args(&[
            "--crawl-origin",
            "HTTP://EXAMPLE.test:80",
            "--crawl-origin",
            "http://example.test/"
        ])
        .is_err());
    }

    #[test]
    fn restricted_origin_rejects_ambiguous_scope_and_outside_seeds() {
        for value in [
            "file:///tmp",
            "http://user:pass@127.0.0.1",
            "http://127.0.0.1/path",
            "http://127.0.0.1/?q",
            "http://127.0.0.1/#fragment",
            "http://127.0.0.1:0",
        ] {
            assert!(args(&["--crawl-origin", value]).is_err(), "{value}");
        }
        for value in [
            "http://127.0.0.1:81/a",
            "https://127.0.0.1/a",
            "http://example.com/a",
        ] {
            assert!(
                args(&["--crawl-origin", "http://127.0.0.1", value]).is_err(),
                "{value}"
            );
        }
        assert!(args(&["--crawl-origin"]).is_err());
        assert!(args(&[
            "--crawl-origin",
            "http://127.0.0.1",
            "--crawl-origin",
            "http://127.0.0.1"
        ])
        .is_err());
    }

    #[test]
    fn documented_defaults_and_zero_depth_are_supported() {
        let o = args(&[]).unwrap().unwrap();
        assert_eq!(
            (o.pages, o.sites_target, o.max_depth, o.web_port),
            (50_000, 100, 10, 8080)
        );
        let o = args(&[
            "--maxdepth",
            "0",
            "--port",
            "0",
            "--bind",
            "[::1]:0",
            "--pages",
            "18446744073709551615",
        ])
        .unwrap()
        .unwrap();
        assert_eq!((o.max_depth, o.web_port, o.pages), (0, 0, u64::MAX));
        assert_eq!(o.bind_addr.unwrap().port(), 0);
    }

    #[test]
    fn gateway_process_policy_is_positive_bounded_and_manifest_only() {
        assert_eq!(args(&[]).unwrap().unwrap().gateway_process_limit, 64);
        assert!(args(&["--gateway-process-limit", "2"]).is_err());
        for value in ["0", "-1", "abc", "18446744073709551615"] {
            assert!(args(&["--manifest", "app.toml", "--gateway-process-limit", value]).is_err());
        }
        assert_eq!(
            args(&["--manifest", "app.toml", "--gateway-process-limit", "2"])
                .unwrap()
                .unwrap()
                .gateway_process_limit,
            2
        );
        assert!(args(&[
            "--manifest",
            "app.toml",
            "--gateway-process-limit",
            "2",
            "--gateway-process-limit",
            "3"
        ])
        .is_err());
    }

    #[test]
    fn every_value_option_rejects_missing_empty_and_option_as_value() {
        for option in [
            "--pages",
            "--gateway-process-limit",
            "--sites",
            "--maxdepth",
            "--port",
            "--data",
            "--bind",
            "--wait",
            "--node-root",
        ] {
            for tail in [vec![option], vec![option, ""], vec![option, "--keep"]] {
                assert!(args(&tail).is_err(), "{tail:?}");
            }
        }
    }

    #[test]
    fn invalid_numbers_and_addresses_are_errors_not_panics() {
        for values in [
            vec!["--pages", "0"],
            vec!["--pages", "-1"],
            vec!["--pages", "18446744073709551616"],
            vec!["--sites", "0"],
            vec!["--wait", "0"],
            vec!["--wait", "18446744073709551615"],
            vec!["--port", "65536"],
            vec!["--maxdepth", "65536"],
            vec!["--bind", "bad"],
            vec!["jvm=localhost:123"],
            vec!["jvm=127.0.0.1:0"],
            vec!["erl=0.0.0.0:1"],
            vec!["--node-root", "relative/artifacts"],
        ] {
            assert!(args(&values).is_err(), "{values:?}");
        }
    }

    #[test]
    fn keep_and_serve_only_are_real_options_and_help_has_no_run() {
        assert!(args(&["--keep"]).unwrap().unwrap().keep_data);
        assert!(args(&["--serve-only"]).unwrap().unwrap().keep_data);
        assert!(args(&["--help"]).unwrap().is_none());
        assert!(args(&["-h"]).unwrap().is_none());
        assert!(args(&["--serve-only", "http://example.test/"]).is_err());
    }

    #[test]
    fn unknown_options_and_duplicates_are_not_silently_ignored() {
        for values in [
            vec!["--unknown"],
            vec!["--spawn-gateways"],
            vec!["--artifacts", "http://example.test"],
            vec!["--keep", "--keep"],
            vec!["--port", "1", "--port", "2"],
            vec!["erl=127.0.0.1:1", "erl=127.0.0.1:2"],
        ] {
            assert!(args(&values).is_err(), "{values:?}");
        }
    }

    #[test]
    fn seeds_are_validated_and_normalized_before_side_effects() {
        let o = args(&["jvm=[::1]:1234", "--", "HTTP://Example.TEST:80/a#part"])
            .unwrap()
            .unwrap();
        assert_eq!(o.seeds, ["http://example.test/a"]);
        assert_eq!(o.gw_addrs, [("jvm".to_string(), "[::1]:1234".to_string())]);
        for value in [
            "a-local-file",
            "ftp://example.test",
            "http://u:p@example.test",
            "--unknown",
        ] {
            assert!(args(&["--", value]).is_err(), "{value}");
        }
    }
}
