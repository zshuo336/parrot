use std::{collections::HashSet, net::SocketAddr};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NodeSpec {
    pub name: String,
    pub dialect: &'static str,
    pub addr: SocketAddr,
}

pub(crate) struct Options {
    pub command: String,
    pub nodes: Vec<NodeSpec>,
    pub insecure: bool,
    pub rounds: u64,
    pub seconds: u64,
    pub interval: u64,
    pub concurrency: usize,
    pub web_port: u16,
}

impl Options {
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        let command = args.first().map(String::as_str).unwrap_or("help");
        let mut out = Self {
            command: command.into(),
            nodes: vec![],
            insecure: false,
            rounds: 5,
            seconds: 10,
            interval: 2,
            concurrency: 4,
            web_port: 8190,
        };
        if matches!(command, "help" | "--help" | "-h") {
            out.command = "help".into();
            return Ok(out);
        }
        anyhow::ensure!(
            ["status", "metrics", "ping", "ask", "watch", "trace", "load", "web"]
                .contains(&command),
            "unknown command: {command}"
        );
        let mut seen = HashSet::new();
        let mut i = 1;
        while i < args.len() {
            let arg = &args[i];
            i += 1;
            if arg == "--help" || arg == "-h" {
                out.command = "help".into();
                return Ok(out);
            }
            if arg == "--insecure-development" {
                anyhow::ensure!(seen.insert(arg.clone()), "duplicate option: {arg}");
                out.insecure = true;
                continue;
            }
            if arg.starts_with('-') {
                let (name, inline) = arg
                    .split_once('=')
                    .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
                let allowed = match name {
                    "--rounds" => matches!(command, "ping" | "ask"),
                    "--seconds" => matches!(command, "trace" | "load"),
                    "--interval" => command == "watch",
                    "--conc" => command == "load",
                    "--web-port" => command == "web",
                    _ => false,
                };
                anyhow::ensure!(allowed, "option {name} is not supported by {command}");
                anyhow::ensure!(seen.insert(name.to_owned()), "duplicate option: {name}");
                let value = match inline {
                    Some(v) => v,
                    None => {
                        let v = args
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("missing value for {name}"))?;
                        i += 1;
                        v
                    }
                };
                let n: u64 = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("{name} requires a positive integer"))?;
                let max = match name {
                    "--conc" => 128,
                    "--web-port" => 65535,
                    "--rounds" => 10000,
                    _ => 86400,
                };
                anyhow::ensure!(n > 0 && n <= max, "{name} must be in 1..={max}");
                match name {
                    "--rounds" => out.rounds = n,
                    "--seconds" => out.seconds = n,
                    "--interval" => out.interval = n,
                    "--conc" => out.concurrency = n as usize,
                    "--web-port" => out.web_port = n as u16,
                    _ => unreachable!(),
                }
            } else {
                // Preserve the documented quoted-space and comma-separated forms.
                for token in arg
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|s| !s.is_empty())
                {
                    let (key, value) = token
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("invalid node: {token}"))?;
                    let (kind, identity) = key
                        .split_once('@')
                        .map_or((key, None), |(k, n)| (k, Some(n)));
                    let (dialect, default_name) = match kind {
                        "erl" => ("erl", "erl-gw-1"),
                        "ray" => ("ray", "ray-gw-1"),
                        "jvm" => ("jvm", "jvm-search-1"),
                        _ => anyhow::bail!("unknown node kind: {key}"),
                    };
                    let name = identity.unwrap_or(default_name);
                    anyhow::ensure!(
                        !name.is_empty()
                            && name.len() <= 128
                            && name
                                .bytes()
                                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
                        "invalid node identity: {name}"
                    );
                    anyhow::ensure!(
                        seen.insert(kind.to_owned()),
                        "duplicate node dialect: {kind}"
                    );
                    anyhow::ensure!(
                        seen.insert(format!("node:{name}")),
                        "duplicate node identity: {name}"
                    );
                    let addr: SocketAddr = value.parse().map_err(|_| {
                        anyhow::anyhow!(
                            "{key} requires an IP:port address (bracket IPv6 addresses)"
                        )
                    })?;
                    anyhow::ensure!(
                        addr.port() != 0 && !addr.ip().is_unspecified(),
                        "{key} requires a concrete address and nonzero port"
                    );
                    out.nodes.push(NodeSpec {
                        name: name.into(),
                        dialect,
                        addr,
                    });
                }
            }
        }
        anyhow::ensure!(
            !out.nodes.is_empty(),
            "supply at least one erl=IP:port, ray=IP:port or jvm=IP:port node"
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(s: &[&str]) -> anyhow::Result<Options> {
        Options::parse(&s.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }
    #[test]
    fn help_requires_no_live_node() {
        for args in [vec![], vec!["help"], vec!["--help"], vec!["load", "--help"]] {
            assert_eq!(parse(&args).unwrap().command, "help");
        }
    }
    #[test]
    fn strict_numbers_never_silently_default_wrap_or_create_zero_work() {
        for args in [
            vec!["load", "--seconds", "0"],
            vec!["load", "--conc", "129"],
            vec!["web", "--web-port", "65536"],
            vec!["ping", "--rounds=no"],
            vec!["watch", "--interval"],
            vec!["trace", "--seconds", "18446744073709551616"],
            vec!["load", "--seconds", "1", "--seconds=2"],
            vec!["load", "--rounds=1"],
            vec!["wat"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        let opts = parse(&["load", "ray=127.0.0.1:2", "--seconds=1", "--conc", "128"]).unwrap();
        assert_eq!(opts.concurrency, 128);
        assert_eq!(opts.seconds, 1);
    }
    #[test]
    fn documented_node_forms_are_supported_but_typos_and_duplicates_fail() {
        let opts = parse(&["status", "erl=127.0.0.1:1 ray=127.0.0.1:2,jvm=[::1]:3"]).unwrap();
        assert_eq!(opts.nodes.len(), 3);
        for nodes in [
            "er1=127.0.0.1:1",
            "erl=bad",
            "erl=0.0.0.0:1",
            "erl=127.0.0.1:0",
            "erl=127.0.0.1:1 erl=127.0.0.1:2",
            "oops",
        ] {
            assert!(parse(&["status", nodes]).is_err(), "{nodes}");
        }
    }
    #[test]
    fn custom_identity_preserves_the_explicit_dialect_and_rejects_ambiguity() {
        let options = parse(&["ask", "jvm@search-custom=127.0.0.1:2"]).unwrap();
        assert_eq!(options.nodes[0].name, "search-custom");
        assert_eq!(options.nodes[0].dialect, "jvm");
        for nodes in [
            "jvm@=127.0.0.1:2",
            "jvm@bad/name=127.0.0.1:2",
            "jvm@a=127.0.0.1:2 jvm@b=127.0.0.1:3",
            "jvm@a=127.0.0.1:2 ray@a=127.0.0.1:3",
        ] {
            assert!(parse(&["ask", nodes]).is_err(), "{nodes}");
        }
    }
}
