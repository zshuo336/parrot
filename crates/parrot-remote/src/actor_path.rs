//! Lossless qualification of engine-local service addresses.

fn native_actix(path: &str) -> bool {
    path.starts_with("actix://")
        && path.len() > "actix://".len()
        && crate::mirror::canonical(&path["actix:/".len()..])
}

/// Native Actix addresses use a reserved, non-canonical absolute tail so they
/// cannot alias a canonical `/actix/...` actor. No URL decoding is performed.
pub(crate) fn split(path: &str) -> Option<(&str, &str)> {
    if path.len() > 4096 {
        return None;
    }
    let rest = path.strip_prefix("parrot://")?;
    let (node, _) = rest.split_once('/')?;
    parrot_config::endpoint::validate_node_id(node).ok()?;
    let tail = &rest[node.len()..];
    if let Some(native) = tail.strip_prefix('/') {
        if native_actix(native) {
            return Some((node, native));
        }
    }
    (tail != "/" && crate::mirror::canonical(tail)).then_some((node, tail))
}

pub(crate) fn qualify(node: &str, path: &str) -> Option<String> {
    if path.len() > 4096 {
        return None;
    }
    let qualified = if native_actix(path) {
        format!("parrot://{node}/{path}")
    } else if path != "/" && crate::mirror::canonical(path) {
        format!("parrot://{node}{path}")
    } else {
        path.to_owned()
    };
    (split(&qualified)?.0 == node).then_some(qualified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualification_is_lossless_and_keeps_engine_namespaces_distinct() {
        for (local, wire) in [
            ("/user/echo", "parrot://node/user/echo"),
            (
                "actix://Echo/instance",
                "parrot://node/actix://Echo/instance",
            ),
            ("/actix/Echo/instance", "parrot://node/actix/Echo/instance"),
            (
                "/actix:/Echo/instance",
                "parrot://node/actix:/Echo/instance",
            ),
            (
                "actix://pkg::Echo<Msg>/id",
                "parrot://node/actix://pkg::Echo<Msg>/id",
            ),
            ("actix://Echo/%2fid", "parrot://node/actix://Echo/%2fid"),
        ] {
            assert_eq!(qualify("node", local).as_deref(), Some(wire));
            assert_eq!(qualify("node", wire).as_deref(), Some(wire));
            assert_eq!(split(wire), Some(("node", local)));
        }
    }

    #[test]
    fn invalid_or_foreign_addresses_never_become_local_registrations() {
        for invalid in [
            "",
            "/",
            "actix://",
            "actix:///Echo/id",
            "actix://Echo//id",
            "actix://Echo/../id",
            "actix://Echo/./id",
            "actix://Echo/id?x",
            "actix://Echo/id#x",
            "actix://Echo/\nid",
            "/actix://Echo/id",
            "other://Echo/id",
            "parrot://other/user/echo",
            "parrot://other/actix://Echo/id",
            "parrot:///actix://Echo/id",
            "parrot://node/actix:///Echo/id",
            "parrot://node/actix://Echo/../id",
            "parrot://node/actix://Echo//id",
        ] {
            assert_eq!(qualify("node", invalid), None, "{invalid:?}");
        }
        assert_eq!(qualify("invalid/node", "actix://Echo/id"), None);
    }

    #[test]
    fn limit_includes_node_and_native_scheme() {
        for prefix in ["/user/", "actix://Echo/"] {
            let overhead = "parrot://node".len() + usize::from(prefix.starts_with("actix://"));
            let local = format!("{prefix}{}", "x".repeat(4096 - overhead - prefix.len()));
            let qualified = qualify("node", &local).unwrap();
            assert_eq!(qualified.len(), 4096);
            assert_eq!(split(&qualified), Some(("node", local.as_str())));
            assert_eq!(qualify("node", &(local + "x")), None);
            assert_eq!(split(&(qualified + "x")), None);
        }
    }
}
