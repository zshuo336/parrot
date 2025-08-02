use parrot_api::actor::{ActorConfig, EmptyConfig};
use parrot_api::supervisor::{DefaultStrategy, SupervisorStrategyType};

#[cfg(test)]
mod tests {
    use super::*;
    
    // Test default implementation of EmptyConfig
    #[test]
    fn test_empty_config_default() {
        let config = EmptyConfig::default();
        // Verify that EmptyConfig implements Default trait
        assert!(std::any::Any::type_id(&config) == std::any::TypeId::of::<EmptyConfig>());
    }

    // Test default values of a custom actor config
    #[test]
    fn test_custom_config_default() {
        // Define a custom actor config struct
        #[derive(Debug, Default)]
        struct CustomActorConfig {
            timeout: std::time::Duration,
            max_retries: u32,
        }

        impl ActorConfig for CustomActorConfig {}

        // Use default constructor
        let config = CustomActorConfig::default();

        // Verify default values
        assert_eq!(config.timeout, std::time::Duration::from_secs(0));
        assert_eq!(config.max_retries, 0);

        // Test custom values
        let custom_config = CustomActorConfig {
            timeout: std::time::Duration::from_secs(10),
            max_retries: 3,
        };

        assert_eq!(custom_config.timeout, std::time::Duration::from_secs(10));
        assert_eq!(custom_config.max_retries, 3);
    }

    // Test config usage in system creation
    #[test]
    fn test_config_in_system_creation() {
        // Define a system config struct with default values
        #[derive(Debug, Default)]
        struct TestSystemConfig {
            name: String,
            supervision_strategy: SupervisorStrategyType,
        }

        // Create default config
        let config = TestSystemConfig::default();

        // Verify default name is empty
        assert_eq!(config.name, "");

        // Verify default strategy is DefaultStrategy
        if let SupervisorStrategyType::Default(_strategy) = config.supervision_strategy {
            // Default strategy is present, as expected
        } else {
            panic!("Expected DefaultStrategy as default");
        }

        // Use custom values
        let custom_config = TestSystemConfig {
            name: "test-system".to_string(),
            supervision_strategy: SupervisorStrategyType::Default(DefaultStrategy::RestartOnFailure),
        };

        assert_eq!(custom_config.name, "test-system");
    }
}