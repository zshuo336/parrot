//! MQTT topic mapping and an owned ingress bridge to Parrot actors.
//!
//! [`MqttBridge`] connects to an existing broker; it is not an MQTT broker.
//! One configured topic binds one producer/session to one receiver path. QoS0
//! forwards ordinary Wire TELL frames without replay. QoS1 requires a stable
//! durable envelope, persists idempotent acceptance, and awaits transactional
//! business completion before publishing a Wire receipt and sending MQTT PUBACK.
//! Broker ACLs provide publisher identity isolation; MQTT packet IDs do not.
//! The small mapping helpers below do not themselves provide any delivery guarantee.

/// MQTT option and packet types from the exact client used by this bridge.
/// Local consumers should use this re-export instead of mixing client sources.
pub use rumqttc as client;

mod bridge;
pub use bridge::{BridgeConfig, BridgeError, BridgeHandle, BridgeStatus, MqttBridge};

use std::time::Duration;

/// topic ↔ Receptionist key 双向映射（07 §4.5）。
///
/// key "edge/rpa" ↔ topic "$parrot/edge/rpa"（前缀隔离——桥订阅只收
/// `$parrot/` 命名空间，存量业务 topic 不受扰）。
pub const TOPIC_PREFIX: &str = "$parrot/";

/// key → topic（注册发布面）。
pub fn key_to_topic(key: &str) -> String {
    format!("{TOPIC_PREFIX}{key}")
}

/// topic → key（非 `$parrot/` 前缀返回 None——桥外消息不路由）。
pub fn topic_to_key(topic: &str) -> Option<&str> {
    topic.strip_prefix(TOPIC_PREFIX)
}

/// QoS 策略映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QosMode {
    /// QoS0 maps to TELL; the bridge does not replay it.
    Tell,
    /// Intended durable mode; only the owned bridge implements storage/receipts.
    DurableTell,
}

impl QosMode {
    pub fn from_mqtt(qos: u8) -> Self {
        if qos == 0 {
            QosMode::Tell
        } else {
            QosMode::DurableTell
        }
    }

    pub fn to_mqtt(self) -> u8 {
        match self {
            QosMode::Tell => 0,
            QosMode::DurableTell => 1,
        }
    }
}

/// 桥重连退避：MQTT 层与 Wire 层退避取 max（§10.5）。
pub fn bridge_backoff(mqtt_ms: u64, wire_ms: u64) -> Duration {
    Duration::from_millis(mqtt_ms.max(wire_ms).min(30_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    // mqtt_topic_key_mapping：双向注册/发现
    #[test]
    fn mqtt_topic_key_mapping() {
        assert_eq!(key_to_topic("edge/rpa"), "$parrot/edge/rpa");
        assert_eq!(topic_to_key("$parrot/edge/rpa"), Some("edge/rpa"));
        // 桥外命名空间不路由
        assert_eq!(topic_to_key("home/livingroom/temp"), None);
        assert_eq!(topic_to_key("$parrotx/edge"), None);
        // 根 key
        assert_eq!(topic_to_key("$parrot/k"), Some("k"));
    }

    // QoS 映射
    #[test]
    fn qos_mapping() {
        assert_eq!(QosMode::from_mqtt(0), QosMode::Tell);
        assert_eq!(QosMode::from_mqtt(1), QosMode::DurableTell);
        assert_eq!(QosMode::from_mqtt(2), QosMode::DurableTell); // 降级 QoS1
        assert_eq!(QosMode::Tell.to_mqtt(), 0);
        assert_eq!(QosMode::DurableTell.to_mqtt(), 1);
    }

    // 退避取 max + 上限 30s（§10.5）
    #[test]
    fn backoff_max_cap() {
        assert_eq!(bridge_backoff(1000, 5000), Duration::from_millis(5000));
        assert_eq!(bridge_backoff(60000, 1000), Duration::from_millis(30000));
    }
}
