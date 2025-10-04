//! E5 · Receptionist ACL（DEV_03 §6 / 06 §3.5）。
//!
//! 静态配置（`acl.json`——serde_json 白名单内；"静态 yaml"的语义以 JSON 承载，
//! 格式裁定记 DECISIONS_DEPENDENCIES 变更流程）：role（证书 CN 绑定）→
//! 允许 register/subscribe 的 scope 前缀。
//!
//! 执行点：Receptionist 注册/订阅入口（REPLY_ERR code 13 Forbidden）。
//! P3 静态表；动态 ACL/权限 actor 化在 P4+（红线）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// ACL 规则集（静态加载，进程内只读）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AclRules {
    /// role → 允许的 scope 前缀列表（key 的第一段匹配）。
    pub roles: HashMap<String, Vec<String>>,
    /// 未匹配 role 的默认策略：false=拒绝（白名单模式——默认安全）。
    #[serde(default)]
    pub default_deny: bool,
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
        serde_json::from_str(text).map_err(|e| format!("acl json parse: {e}"))
    }

    /// 空 ACL（全部走 default_deny=false → 全放行——测试/无 ACL 部署形态）。
    pub fn permissive() -> Self {
        Self::default()
    }

    /// role + action scope 判定。
    ///
    /// `key` 形如 "{scope}/{name}"——scope 是第一段。role 允许列表中任一
    /// 前缀是 scope 的前缀即放行（"edge" 匹配 "edge/rpa" 与 "edge/sub/x"）。
    pub fn check(&self, role: &str, key: &str) -> AclDecision {
        let scope = key.split('/').next().unwrap_or("");
        let allowed = self
            .roles
            .get(role)
            .map(|prefixes| prefixes.iter().any(|p| scope.starts_with(p.as_str())))
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
/// Directory 下发路由前缀附带 ACL：`{prefix, allow: [realm/cluster]}`；
/// border/hub 转发前校验——**跨 realm 默认拒绝**。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteAcl {
    /// 前缀 → 允许的 realm/cluster 列表。
    pub routes: HashMap<String, Vec<String>>,
    /// 无条目前缀的默认：true=跨 realm 拒绝（07 §10 安全默认）。
    #[serde(default = "default_true")]
    pub default_deny_cross_realm: bool,
}

fn default_true() -> bool {
    true
}

impl RouteAcl {
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| format!("route acl json parse: {e}"))
    }

    /// 转发判定：`src_realm`（发送方 realm/cluster）→ `target_prefix`
    /// （路由前缀——最长匹配条目）。
    pub fn check_route(&self, src_realm: &str, target_prefix: &str) -> AclDecision {
        // 最长前缀条目
        let hit = self
            .routes
            .iter()
            .filter(|(p, _)| target_prefix.starts_with(p.as_str()))
            .max_by_key(|(p, _)| p.len());
        match hit {
            Some((_, allow)) => {
                if allow.iter().any(|r| r == src_realm || r == "*") {
                    AclDecision::Allow
                } else {
                    AclDecision::Deny
                }
            }
            None => {
                // 无条目：同 realm 直通；跨 realm 按默认策略
                let same =
                    target_prefix.starts_with(&format!("{src_realm}/")) || src_realm.is_empty();
                if same || !self.default_deny_cross_realm {
                    AclDecision::Allow
                } else {
                    AclDecision::Deny
                }
            }
        }
    }
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

    // 无条目前缀：跨 realm 默认拒；无 realm 单集群直通
    #[test]
    fn route_acl_default_policy() {
        let r = route_rules();
        // 无 realm 标识（单集群/本地域）→ 直通
        assert_eq!(r.check_route("", "parrot://us-1/x"), AclDecision::Allow);
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
