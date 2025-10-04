//! E7 · ParrotMqttBridge（DEV_03 §8 / 07 §4.5 裁定落地）。
//!
//! 结构：
//! ```text
//! ParrotMqttBridge（parrot 节点，topology_role=Border）：
//!   MQTT 侧：rumqttc AsyncClient（对接存量 EMQX/Mosquitto）
//!   parrot 侧：FrameLink（Transport 接入）
//!   映射：topic ↔ Receptionist key（"edge/rpa" ↔ "$parrot/edge/rpa"）；
//!         MQTT payload = 完整 Parrot 帧（透传）
//!   QoS：QoS0 ↔ TELL；QoS1 ↔ durable tell（E2 复用——桥即 CloudProxy 的
//!        MQTT 前端）
//! ```
//! keep-alive：MQTT 层与 Wire 心跳并存不冲突；桥重连退避取两者 max
//! （实现注意事项 §10.5）。

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
    /// QoS0 ↔ TELL：at-most-once，不落 WAL。
    Tell,
    /// QoS1 ↔ durable tell：at-least-once + 端侧去重（E2 WAL 复用）。
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
