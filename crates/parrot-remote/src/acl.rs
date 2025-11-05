//! E5 · Receptionist ACL（DEV_03 §6 / 06 §3.5）。
//!
//! Static authorization helpers. Prefixes match complete path segments.
//! ReceptionistFacade checks its injected principal at each operation; callers
//! authenticate standalone principals. System-owned receptionist gateways check
//! the local node, and network snapshots check the authorized physical peer.
//! Both use RemoteConfig::receptionist_acl, which denies all keys by default.
//! RouteAcl is not yet
//! wired into the default remote ingress, and these helpers do not establish
//! TLS identity, wire error mapping, or control-plane authorization.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// ACL 规则集（静态加载，进程内只读）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AclRules {
    /// Principal → allowed key prefixes, matched on segment boundaries.
    #[serde(default, deserialize_with = "deserialize_unique_rules")]
    pub roles: HashMap<String, Vec<String>>,
    /// Unmatched principal/key pairs are denied by default.
    #[serde(default = "default_true")]
    pub default_deny: bool,
}

impl Default for AclRules {
    fn default() -> Self {
        Self {
            roles: HashMap::new(),
            default_deny: true,
        }
    }
}

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclDecision {
    Allow,
    Deny,
}

impl AclRules {
    /// 从 JSON 文本加载。
    pub fn from_json(text: &str) -> Result<Self, String> {
        let rules: Self = serde_json::from_str(text).map_err(|e| format!("acl json parse: {e}"))?;
        rules.validate()?;
        Ok(rules)
    }

    /// Explicitly permit well-formed principals and keys without a grant.
    /// Invalid input is still denied; this does not authenticate the principal.
    pub fn permissive() -> Self {
        Self {
            default_deny: false,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for (role, prefixes) in &self.roles {
            if !valid_segments(role) {
                return Err(format!("invalid ACL principal: {role:?}"));
            }
            for prefix in prefixes {
                if key_prefix(prefix).is_none() {
                    return Err(format!("invalid ACL prefix for {role:?}: {prefix:?}"));
                }
            }
        }
        Ok(())
    }

    /// role + action scope 判定。
    ///
    /// `edge` permits `edge` and `edge/rpa`, but not `edge-admin/rpa`.
    /// A narrower `edge/public` grant permits only that subtree. Malformed
    /// identities, keys or directly constructed rules fail closed even when
    /// default_deny is explicitly disabled.
    pub fn check(&self, role: &str, key: &str) -> AclDecision {
        if !valid_segments(role) || !valid_segments(key) || self.validate().is_err() {
            return AclDecision::Deny;
        }
        let allowed = self
            .roles
            .get(role)
            .map(|prefixes| {
                prefixes
                    .iter()
                    .any(|p| key_prefix(p).is_some_and(|prefix| within(key, prefix)))
            })
            .unwrap_or(false);
        if allowed {
            AclDecision::Allow
        } else if self.default_deny {
            AclDecision::Deny
        } else {
            AclDecision::Allow
        }
    }
}

/// 订阅事件流的过滤形态：未授权 key 的事件不下发（`acl_subscribe_filtered`）。
impl AclRules {
    /// 事件是否可下发给 role（Registered/Unregistered 的 key 均校验）。
    pub fn event_visible(&self, role: &str, key: &str) -> bool {
        self.check(role, key) == AclDecision::Allow
    }
}

/// F8 · 路由前缀 ACL（DEV_05 §7 / 07 §10 增补）。
///
/// A policy for canonical parrot:// prefixes, with trusted realm/cluster
/// principals supplied by the caller. Cross-realm access defaults to denied.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteAcl {
    /// 前缀 → 允许的 realm/cluster 列表。
    #[serde(default, deserialize_with = "deserialize_unique_rules")]
    pub routes: HashMap<String, Vec<String>>,
    /// 无条目前缀的默认：true=跨 realm 拒绝（07 §10 安全默认）。
    #[serde(default = "default_true")]
    pub default_deny_cross_realm: bool,
}

impl Default for RouteAcl {
    fn default() -> Self {
        Self {
            routes: HashMap::new(),
            default_deny_cross_realm: true,
        }
    }
}

fn default_true() -> bool {
    true
}

impl RouteAcl {
    pub fn from_json(text: &str) -> Result<Self, String> {
        let rules: Self =
            serde_json::from_str(text).map_err(|e| format!("route acl json parse: {e}"))?;
        rules.validate()?;
        Ok(rules)
    }

    pub fn validate(&self) -> Result<(), String> {
        for (prefix, allowed) in &self.routes {
            if route_prefix(prefix).is_none() {
                return Err(format!("invalid route ACL prefix: {prefix:?}"));
            }
            for principal in allowed {
                if principal != "*" && !valid_segments(principal) {
                    return Err(format!("invalid route ACL principal: {principal:?}"));
                }
            }
        }
        Ok(())
    }

    /// 转发判定：`src_realm`（发送方 realm/cluster）→ `target_prefix`
    /// （路由前缀——最长匹配条目）。
    pub fn check_route(&self, src_realm: &str, target_prefix: &str) -> AclDecision {
        let Some(target) = route_prefix(target_prefix) else {
            return AclDecision::Deny;
        };
        if !valid_segments(src_realm) || self.validate().is_err() {
            return AclDecision::Deny;
        }
        let mut best: Option<(usize, bool)> = None;
        for (prefix, allowed) in &self.routes {
            let prefix = route_prefix(prefix).expect("validated prefix");
            if !within(target, prefix) {
                continue;
            }
            let permits = allowed.iter().any(|r| r == src_realm || r == "*");
            match &mut best {
                Some((length, allow)) if *length == prefix.len() => *allow &= permits,
                Some((length, _)) if *length > prefix.len() => {}
                _ => best = Some((prefix.len(), permits)),
            }
        }
        // An explicit rule overrides same-realm and permissive defaults.
        // Equal normalized prefixes (with/without trailing slash) intersect
        // grants, so HashMap iteration order cannot select a weaker policy.
        let allow = best
            .map(|(_, allow)| allow)
            .unwrap_or_else(|| within(target, src_realm) || !self.default_deny_cross_realm);
        if allow {
            AclDecision::Allow
        } else {
            AclDecision::Deny
        }
    }
}

fn within(target: &str, prefix: &str) -> bool {
    target == prefix
        || target
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

// No implicit percent decoding or dot-segment normalization in authorization:
// reject ambiguous inputs rather than granting before a later interpretation.
fn valid_segments(value: &str) -> bool {
    !value.is_empty()
        && !value.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '%' | '?' | '#' | '*' | '\\')
        })
        && value.split('/').all(|s| !matches!(s, "" | "." | ".."))
}

fn key_prefix(value: &str) -> Option<&str> {
    let prefix = value.strip_suffix('/').unwrap_or(value);
    valid_segments(prefix).then_some(prefix)
}

fn route_prefix(value: &str) -> Option<&str> {
    key_prefix(value.strip_prefix("parrot://")?)
}

fn deserialize_unique_rules<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Rules;
    impl<'de> serde::de::Visitor<'de> for Rules {
        type Value = HashMap<String, Vec<String>>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an ACL map with unique keys")
        }
        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut rules = HashMap::new();
            while let Some((key, value)) = map.next_entry::<String, Vec<String>>()? {
                match rules.entry(key) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(value);
                    }
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate ACL key: {:?}",
                            entry.key()
                        )));
                    }
                }
            }
            Ok(rules)
        }
    }
    deserializer.deserialize_map(Rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> AclRules {
        AclRules::from_json(
            r#"{
              "roles": {
                "edge-device": ["edge"],
                "cloud-orch": ["cloud", "media"],
                "jvm-gw": ["jvm"]
              },
              "default_deny": true
            }"#,
        )
        .unwrap()
    }

    // acl_register_denied：越权 scope → Forbidden
    #[test]
    fn acl_register_denied() {
        let r = rules();
        assert_eq!(r.check("edge-device", "edge/rpa"), AclDecision::Allow);
        assert_eq!(r.check("edge-device", "cloud/admin"), AclDecision::Deny);
        assert_eq!(r.check("unknown-role", "edge/x"), AclDecision::Deny);
        // 前缀匹配："media" 放行 "media/livekit"
        assert_eq!(r.check("cloud-orch", "media/livekit"), AclDecision::Allow);
    }

    // acl_subscribe_filtered：事件流按 role 过滤
    #[test]
    fn acl_subscribe_filtered() {
        let r = rules();
        assert!(r.event_visible("edge-device", "edge/sensor"));
        assert!(!r.event_visible("edge-device", "cloud/secret"));
    }

    // 无 ACL（permissive）→ 全放行
    #[test]
    fn acl_permissive_when_absent() {
        let r = AclRules::permissive();
        assert_eq!(r.check("anyone", "any/thing"), AclDecision::Allow);
    }

    // 语义边界：空 scope / 无斜杠 key
    #[test]
    fn acl_edge_keys() {
        let r = rules();
        assert_eq!(r.check("edge-device", ""), AclDecision::Deny);
        assert_eq!(r.check("edge-device", "edge"), AclDecision::Allow); // scope 即 "edge"
    }

    // ── F8 · 路由前缀 ACL ────────────────────────────────

    fn route_rules() -> RouteAcl {
        RouteAcl::from_json(
            r#"{
              "routes": {
                "parrot://eu-1/": ["eu-realm", "cn-realm"],
                "parrot://eu-1/admin/": ["eu-realm"]
              },
              "default_deny_cross_realm": true
            }"#,
        )
        .unwrap()
    }

    // cross_realm_denied（07 §11 P5 出口判据）：无授权 realm → 拒绝
    #[test]
    fn cross_realm_denied() {
        let r = route_rules();
        // 未列 realm 访问受保护前缀 → 拒
        assert_eq!(
            r.check_route("us-realm", "parrot://eu-1/user/x"),
            AclDecision::Deny
        );
        // 收窄子前缀：admin 只允许 eu
        assert_eq!(
            r.check_route("cn-realm", "parrot://eu-1/admin/secret"),
            AclDecision::Deny
        );
        // 授权放行
        assert_eq!(
            r.check_route("cn-realm", "parrot://eu-1/user/x"),
            AclDecision::Allow
        );
        assert_eq!(
            r.check_route("eu-realm", "parrot://eu-1/admin/ops"),
            AclDecision::Allow
        );
    }

    // 无条目前缀：跨 realm 默认拒；缺少来源身份也拒绝
    #[test]
    fn route_acl_default_policy() {
        let r = route_rules();
        assert_eq!(r.check_route("", "parrot://us-1/x"), AclDecision::Deny);
        // 跨 realm 无条目 → 默认拒绝
        assert_eq!(
            r.check_route("us-realm", "parrot://ap-1/x"),
            AclDecision::Deny
        );
        // 显式关闭默认拒绝（可信内网形态）
        let open =
            RouteAcl::from_json(r#"{"routes": {}, "default_deny_cross_realm": false}"#).unwrap();
        assert_eq!(
            open.check_route("us-realm", "parrot://ap-1/x"),
            AclDecision::Allow
        );
        // 通配放行
        let wild = RouteAcl::from_json(
            r#"{"routes": {"parrot://x/": ["*"]}, "default_deny_cross_realm": true}"#,
        )
        .unwrap();
        assert_eq!(wild.check_route("any", "parrot://x/y"), AclDecision::Allow);
    }
}
