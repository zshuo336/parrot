use parrot_api::actor::{ActorConfig, EmptyConfig};
use std::time::Duration;

/// Custom actor configuration with default values
#[derive(Debug, Clone, PartialEq)]
pub struct CustomActorConfig {
    pub name: String,
    pub timeout: Duration,
    pub retries: u32,
    pub is_active: bool,
}

impl ActorConfig for CustomActorConfig {}

impl Default for CustomActorConfig {
    fn default() -> Self {
        Self {
            name: "default-actor".to_string(),
            timeout: Duration::from_secs(30),
            retries: 3,
            is_active: true,
        }
    }
}

/// Partial custom configuration where only some fields are modified
#[derive(Debug, Clone, PartialEq)]
pub struct PartialCustomConfig {
    pub name: String,
    pub timeout: Duration,
    pub retries: u32,
    pub is_active: bool,
}

impl ActorConfig for PartialCustomConfig {}

impl Default for PartialCustomConfig {
    fn default() -> Self {
        Self {
            name: "partial-default".to_string(),
            timeout: Duration::from_secs(10),
            retries: 5,
            is_active: false,
        }
    }
}

impl PartialCustomConfig {
    /// Create a configuration with a custom name; other fields use default values
    pub fn with_name(name: &str) -> Self {
        let mut config = Self::default();
        config.name = name.to_string();
        config
    }

    /// Create a configuration with a custom timeout; other fields use default values
    pub fn with_timeout(timeout: Duration) -> Self {
        let mut config = Self::default();
        config.timeout = timeout;
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_config_default() {
        let config = EmptyConfig::default();
        // EmptyConfig should be zero-sized or very small
        assert!(
            std::mem::size_of_val(&config) <= std::mem::size_of::<usize>(),
            "EmptyConfig should be zero-sized or very small"
        );
    }

    #[test]
    fn test_custom_config_default() {
        let config = CustomActorConfig::default();
        // Verify default values
        assert_eq!(config.name, "default-actor");
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert_eq!(config.retries, 3);
        assert_eq!(config.is_active, true);
    }

    #[test]
    fn test_partial_custom_config_defaults_and_overrides() {
        // Default PartialCustomConfig
        let default_config = PartialCustomConfig::default();
        assert_eq!(default_config.name, "partial-default");
        assert_eq!(default_config.timeout, Duration::from_secs(10));
        assert_eq!(default_config.retries, 5);
        assert_eq!(default_config.is_active, false);

        // Override name only
        let named_config = PartialCustomConfig::with_name("custom-name");
        assert_eq!(named_config.name, "custom-name");
        assert_eq!(named_config.timeout, Duration::from_secs(10));
        assert_eq!(named_config.retries, 5);
        assert_eq!(named_config.is_active, false);

        // Override timeout only
        let timeout_config = PartialCustomConfig::with_timeout(Duration::from_secs(60));
        assert_eq!(timeout_config.name, "partial-default");
        assert_eq!(timeout_config.timeout, Duration::from_secs(60));
        assert_eq!(timeout_config.retries, 5);
        assert_eq!(timeout_config.is_active, false);
    }

    #[test]
    fn test_multiple_config_instances_are_independent() {
        // Create multiple instances with different settings
        let config1 = PartialCustomConfig::with_name("instance1");
        let config2 = PartialCustomConfig::with_name("instance2");
        let config3 = PartialCustomConfig::with_timeout(Duration::from_secs(5));

        // Verify each instance holds its own values
        assert_eq!(config1.name, "instance1");
        assert_eq!(config2.name, "instance2");
        assert_eq!(config3.timeout, Duration::from_secs(5));

        // Ensure no cross-instance interference
        assert_ne!(config1.name, config2.name);
        assert_ne!(config3.timeout, config1.timeout);
    }

    #[test]
    fn test_manual_custom_config_creation() {
        // Manually create a fully custom configuration
        let custom_config = CustomActorConfig {
            name: "fully-custom".to_string(),
            timeout: Duration::from_secs(120),
            retries: 10,
            is_active: false,
        };

        // Verify assigned values
        assert_eq!(custom_config.name, "fully-custom");
        assert_eq!(custom_config.timeout, Duration::from_secs(120));
        assert_eq!(custom_config.retries, 10);
        assert_eq!(custom_config.is_active, false);

        // Compare with default to ensure they differ
        let default_config = CustomActorConfig::default();
        assert_ne!(custom_config, default_config);
    }
}