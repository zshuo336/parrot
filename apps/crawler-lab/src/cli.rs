use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

pub const USAGE: &str = "Usage: crawler-lab --insecure-development erl=host:port ray=host:port jvm=host:port [options]\n       crawler-lab --insecure-development --bind IP:port [options]\n       crawler-lab --insecure-development --manifest FILE [--bind IP:port] [options]\nOptions:\n  --manifest FILE  assemble and own all components declared in the manifest\n  --query-ready FILE --query-done FILE  wait for external query completion before teardown\n  --pages N   synthetic document space, 0..1000000 (default 500)\n  --depth N   maximum link depth, 0..65535 (default 2)\n  --fanout N  links per page, 0..256 (default 3)\n  --batch N   records per request, 1..256 (default 32)\n  --wait N    registry startup or manifest query seconds, 1..3600 (default 60)\n  --hub-dylib FILE --trust-native-sha256 HEX64  execute explicitly trusted native business\n  --hub-wasm FILE --hub-sha256 HEX64  execute sandboxed business\n  --help, -h  print this help\nPlain TCP gateways require --insecure-development. Bind accepts port 0. Manifest workload defaults come from component settings; only explicit flags override them.\n";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    fn parse(text: &str) -> Result<Self, String> {
        if let Ok(address) = text.parse::<SocketAddr>() {
            if address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
                return Err(format!("invalid destination: {text}"));
            }
            return Ok(Self {
                host: address.ip().to_string(),
                port: address.port(),
            });
        }
        let (host, port) = text
            .split_once(':')
            .ok_or_else(|| format!("expected host:port: {text}"))?;
        let normalized = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        if normalized.is_empty()
            || normalized.len() > 253
            || normalized.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
        {
            return Err(format!("invalid destination host: {host}"));
        }
        let port = number("destination port", port, 1, 65535)? as u16;
        Ok(Self {
            host: normalized,
            port,
        })
    }
}

#[derive(Debug)]
pub enum Mode {
    Manifest {
        path: String,
        bind: SocketAddr,
        query: Option<(String, String)>,
        wait_secs: u64,
    },
    Direct([Endpoint; 3]),
    Registry {
        bind: SocketAddr,
        wait_secs: u64,
    },
}

#[derive(Debug)]
pub struct Options {
    pub mode: Mode,
    pub pages: u64,
    pub depth: u16,
    pub fanout: u64,
    pub batch: usize,
    pub hub: Hub,
    pub workload_overrides: [bool; 4],
}

#[derive(Debug)]
pub enum Hub {
    Embedded,
    Dylib { path: String, digest: String },
    Wasm { path: String, digest: String },
}

fn number(name: &str, value: &str, min: u64, max: u64) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{name} requires an unsigned integer"));
    }
    let value = value
        .parse::<u64>()
        .map_err(|_| format!("{name} is too large"))?;
    if !(min..=max).contains(&value) {
        return Err(format!("{name} must be between {min} and {max}"));
    }
    Ok(value)
}

pub fn parse(args: &[String]) -> Result<Option<Options>, String> {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        return Ok(None);
    }
    let (mut pages, mut depth, mut fanout, mut batch, mut wait_secs) = (500, 2, 3, 32, 60);
    let mut bind = None;
    let (mut manifest, mut ready, mut done) = (None, None, None);
    let mut destinations = HashMap::new();
    let mut seen = HashSet::new();
    let mut insecure = false;
    let (mut native, mut wasm, mut trust, mut digest) = (None, None, None, None);
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg.starts_with('-') {
            if !seen.insert(arg.as_str()) {
                return Err(format!("duplicate option {arg}"));
            }
            if arg == "--insecure-development" {
                insecure = true;
                index += 1;
                continue;
            }
            if !matches!(
                arg.as_str(),
                "--manifest"
                    | "--query-ready"
                    | "--query-done"
                    | "--pages"
                    | "--depth"
                    | "--fanout"
                    | "--batch"
                    | "--wait"
                    | "--bind"
                    | "--hub-dylib"
                    | "--hub-wasm"
                    | "--trust-native-sha256"
                    | "--hub-sha256"
            ) {
                return Err(format!("unknown option {arg}"));
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("missing value for {arg}"))?;
            match arg.as_str() {
                "--manifest" => manifest = Some(value.clone()),
                "--query-ready" => ready = Some(value.clone()),
                "--query-done" => done = Some(value.clone()),
                "--pages" => pages = number(arg, value, 0, 1_000_000)?,
                "--depth" => depth = number(arg, value, 0, u16::MAX.into())?,
                "--fanout" => fanout = number(arg, value, 0, 256)?,
                "--batch" => batch = number(arg, value, 1, 256)?,
                "--wait" => wait_secs = number(arg, value, 1, 3600)?,
                "--hub-dylib" => native = Some(value.clone()),
                "--hub-wasm" => wasm = Some(value.clone()),
                "--trust-native-sha256" => trust = Some(value.clone()),
                "--hub-sha256" => digest = Some(value.clone()),
                "--bind" => {
                    bind =
                        Some(value.parse::<SocketAddr>().map_err(|_| {
                            "--bind requires IP:port (port 0 is allowed)".to_string()
                        })?)
                }
                _ => unreachable!(),
            }
            index += 2;
        } else {
            let (name, address) = arg
                .split_once('=')
                .ok_or_else(|| format!("unknown argument {arg}"))?;
            if !matches!(name, "erl" | "ray" | "jvm") {
                return Err(format!("unknown gateway {name}"));
            }
            if destinations
                .insert(name, Endpoint::parse(address)?)
                .is_some()
            {
                return Err(format!("duplicate gateway {name}"));
            }
            index += 1;
        }
    }
    let mode = if let Some(path) = manifest {
        if path.is_empty() || !destinations.is_empty() {
            return Err("--manifest requires a path and no direct gateways".into());
        }
        let query = match (ready, done) {
            (None, None) => None,
            (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() && a != b => Some((a, b)),
            _ => {
                return Err(
                    "query phase requires distinct --query-ready and --query-done paths".into(),
                )
            }
        };
        Mode::Manifest {
            path,
            bind: bind.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap()),
            query,
            wait_secs,
        }
    } else if ready.is_some() || done.is_some() {
        return Err("query phase requires --manifest".into());
    } else if let Some(bind) = bind {
        if !destinations.is_empty() {
            return Err("--bind cannot be combined with direct gateway addresses".into());
        }
        Mode::Registry { bind, wait_secs }
    } else {
        if seen.contains("--wait") {
            return Err("--wait requires --bind".into());
        }
        let mut next = |name| {
            destinations
                .remove(name)
                .ok_or_else(|| format!("direct mode requires {name}=host:port, or use --bind"))
        };
        let endpoints = [next("erl")?, next("ray")?, next("jvm")?];
        if endpoints.iter().collect::<HashSet<_>>().len() != 3 {
            return Err("gateway destinations must be distinct".into());
        }
        Mode::Direct(endpoints)
    };
    if !insecure {
        return Err("plain TCP gateways require explicit --insecure-development".into());
    }
    let valid_digest = |value: String| -> Result<String, String> {
        if value.len() != 64 || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("hub SHA-256 must contain exactly 64 hexadecimal digits".into());
        }
        Ok(value.to_ascii_lowercase())
    };
    let hub = match (native, wasm, trust, digest) {
        (None, None, None, None) => Hub::Embedded,
        (Some(path), None, Some(digest), None) if !path.is_empty() => Hub::Dylib { path, digest: valid_digest(digest)? },
        (None, Some(path), None, Some(digest)) if !path.is_empty() => Hub::Wasm { path, digest: valid_digest(digest)? },
        _ => return Err("select one hub: --hub-dylib FILE --trust-native-sha256 HEX64, or --hub-wasm FILE --hub-sha256 HEX64".into()),
    };
    Ok(Some(Options {
        mode,
        pages,
        depth: depth as u16,
        fanout,
        batch: batch as usize,
        hub,
        workload_overrides: ["--pages", "--depth", "--fanout", "--batch"].map(|k| seen.contains(k)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn numeric_limits_and_zero_workload_are_preserved() {
        let options = parse(&args(&[
            "--insecure-development",
            "--bind",
            "[::1]:0",
            "--pages",
            "0",
            "--depth",
            "65535",
            "--fanout",
            "0",
            "--batch",
            "256",
            "--wait",
            "3600",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            (options.pages, options.depth, options.fanout, options.batch),
            (0, 65535, 0, 256)
        );
        assert!(matches!(
            options.mode,
            Mode::Registry {
                wait_secs: 3600,
                ..
            }
        ));
    }
    #[test]
    fn endpoint_normalization_rejects_duplicates_and_keeps_ipv6() {
        assert_eq!(
            Endpoint::parse("EXAMPLE.org.:42").unwrap(),
            Endpoint::parse("example.org:42").unwrap()
        );
        assert_eq!(Endpoint::parse("[::1]:42").unwrap().host, "::1");
        assert!(parse(&args(&[
            "--insecure-development",
            "erl=LOCALHOST:42",
            "ray=localhost.:42",
            "jvm=localhost:43"
        ]))
        .is_err());
    }
}

#[cfg(test)]
mod manifest_tests {
    use super::*;
    fn parse_args(values: &[&str]) -> Result<Option<Options>, String> {
        parse(&values.iter().map(|v| v.to_string()).collect::<Vec<_>>())
    }
    #[test]
    fn manifest_defaults_do_not_turn_into_cli_overrides() {
        let o = parse_args(&["--insecure-development", "--manifest", "crawler.app.toml"])
            .unwrap()
            .unwrap();
        assert!(matches!(o.mode, Mode::Manifest { .. }));
        assert_eq!(o.workload_overrides, [false; 4]);
        let o = parse_args(&[
            "--insecure-development",
            "--manifest",
            "crawler.app.toml",
            "--pages",
            "0",
            "--batch",
            "1",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(o.workload_overrides, [true, false, false, true]);
        assert_eq!((o.pages, o.batch), (0, 1));
    }
    #[test]
    fn query_phase_rejects_incomplete_or_ambiguous_arguments() {
        for suffix in [
            vec!["--query-ready", "a"],
            vec!["--query-ready", "a", "--query-done", "a"],
            vec!["erl=localhost:1"],
        ] {
            let mut a = vec!["--insecure-development", "--manifest", "m"];
            a.extend(suffix);
            assert!(parse_args(&a).is_err());
        }
        assert!(parse_args(&[
            "--insecure-development",
            "--bind",
            "127.0.0.1:0",
            "--query-ready",
            "a",
            "--query-done",
            "b"
        ])
        .is_err());
    }
}
