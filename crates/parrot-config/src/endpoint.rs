//! Network-independent validation of configured seed endpoints.
use std::net::{IpAddr, Ipv6Addr, SocketAddr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedEndpoint {
    pub scheme: &'static str,
    pub node_id: String,
    /// False for bare host:port discovery endpoints.
    pub identity_is_explicit: bool,
    /// None for memory endpoints, which are addressed by node identity.
    pub host: Option<String>,
    pub port: u16,
}

pub fn transport_scheme(value: &str) -> Result<&'static str, String> {
    match value {
        "tcp" => Ok("tcp"),
        "quic" => Ok("quic"),
        "mem" => Ok("mem"),
        _ => Err("scheme must be tcp, quic or mem".into()),
    }
}

pub fn validate_node_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > u16::MAX as usize
        || value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '@' | '?' | '#'))
    {
        return Err("node identity must be nonempty, fit the wire length, and contain no whitespace, control or path delimiters".into());
    }
    Ok(())
}

impl SeedEndpoint {
    /// Parse scheme://node@host:port, host:port, or mem://node.
    /// Hostnames remain unresolved so parsing never blocks on DNS.
    pub fn parse(value: &str, default_scheme: &str) -> Result<Self, String> {
        let (scheme, rest) = value.split_once("://").unwrap_or((default_scheme, value));
        let scheme = transport_scheme(scheme)?;
        if scheme == "mem" {
            validate_node_id(rest)?;
            return Ok(Self {
                scheme,
                node_id: rest.into(),
                identity_is_explicit: true,
                host: None,
                port: 0,
            });
        }
        let (identity, address) = rest.split_once('@').unwrap_or((rest, rest));
        validate_node_id(identity)?;
        let (host, port) = if let Some(bracketed) = address.strip_prefix('[') {
            let (host, port) = bracketed
                .split_once("]:")
                .ok_or("IPv6 seed must use [address]:port")?;
            let ip: Ipv6Addr = host.parse().map_err(|_| "invalid IPv6 seed address")?;
            (ip.to_string(), port)
        } else {
            let (host, port) = address.rsplit_once(':').ok_or("seed requires host:port")?;
            let labels = host.strip_suffix('.').unwrap_or(host);
            if host.is_empty()
                || host.len() > 253
                || labels.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                })
            {
                return Err("invalid seed hostname; IPv6 addresses require brackets".into());
            }
            (host.to_owned(), port)
        };
        if port.is_empty() || !port.bytes().all(|c| c.is_ascii_digit()) {
            return Err("invalid seed port".into());
        }
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or("seed port must be in 1..=65535")?;
        Ok(Self {
            scheme,
            node_id: identity.into(),
            identity_is_explicit: rest.contains('@'),
            host: Some(host),
            port,
        })
    }

    pub fn socket_addr(&self) -> Option<SocketAddr> {
        self.host
            .as_ref()?
            .parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, self.port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supported_endpoint_forms_preserve_identity() {
        for (input, scheme, identity, host, port) in [
            (
                "127.0.0.1:123",
                "tcp",
                "127.0.0.1:123",
                Some("127.0.0.1"),
                123,
            ),
            ("quic://peer@[::1]:456", "quic", "peer", Some("::1"), 456),
            (
                "tcp://peer@localhost:789",
                "tcp",
                "peer",
                Some("localhost"),
                789,
            ),
            (
                "tcp://api.example.org.:443",
                "tcp",
                "api.example.org.:443",
                Some("api.example.org."),
                443,
            ),
            ("mem://peer", "mem", "peer", None, 0),
        ] {
            let endpoint = SeedEndpoint::parse(input, "tcp").unwrap();
            assert_eq!(
                (
                    endpoint.scheme,
                    endpoint.node_id.as_str(),
                    endpoint.host.as_deref(),
                    endpoint.port
                ),
                (scheme, identity, host, port)
            );
        }
        assert_eq!(SeedEndpoint::parse("peer", "mem").unwrap().node_id, "peer");
        assert_eq!(
            SeedEndpoint::parse("localhost:443", "quic").unwrap().scheme,
            "quic"
        );
    }
    #[test]
    fn malformed_endpoints_are_rejected_before_resolution() {
        for value in [
            "",
            "mem://",
            "mem://a/b",
            "tcp://@127.0.0.1:1",
            "tcp://a@b@localhost:1",
            "udp://localhost:1",
            "tcp://localhost",
            "tcp://localhost:0",
            "tcp://localhost:65536",
            "tcp://localhost:+80",
            "tcp://localhost:80/path",
            "tcp://user:password@host:80/",
            "tcp://::1:80",
            "tcp://[invalid]:80",
            "tcp://-host:80",
            "tcp://host..local:80",
            "tcp://host:80?query",
            "tcp://host:80#fragment",
            "tcp://host :80",
            "mem://hello\n",
        ] {
            assert!(
                SeedEndpoint::parse(value, "tcp").is_err(),
                "accepted {value:?}"
            );
        }
    }
}
