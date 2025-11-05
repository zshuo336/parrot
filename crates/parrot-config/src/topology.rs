//! Originating business traffic policy. Node roles still govern relay endpoints.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TopologyMode {
    Hub,
    Mesh,
    #[default]
    Hybrid,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TopologySection {
    pub mode: Option<TopologyMode>,
    /// Hub: direct fallback when no live hub exists. Hybrid: relay fallback
    /// when direct discovery fails. Mesh always requires a direct connection.
    pub relay_fallback: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopologyResolved {
    pub mode: TopologyMode,
    pub relay_fallback: bool,
}

impl TopologySection {
    pub(crate) fn with_fallback(self, other: Self) -> Self {
        Self {
            mode: self.mode.or(other.mode),
            relay_fallback: self.relay_fallback.or(other.relay_fallback),
        }
    }

    pub(crate) fn resolve(self) -> TopologyResolved {
        TopologyResolved {
            mode: self.mode.unwrap_or_default(),
            relay_fallback: self.relay_fallback.unwrap_or(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParrotConfig;

    #[test]
    fn defaults_and_fieldwise_precedence_preserve_false() {
        let defaults = ParrotConfig::default().build().unwrap().topology;
        assert_eq!(defaults.mode, TopologyMode::Hybrid);
        assert!(defaults.relay_fallback);
        for mode in [TopologyMode::Hub, TopologyMode::Mesh, TopologyMode::Hybrid] {
            for fallback in [false, true] {
                let file = ParrotConfig {
                    topology: TopologySection {
                        mode: Some(mode),
                        relay_fallback: Some(!fallback),
                    },
                    ..Default::default()
                };
                let mut code = ParrotConfig::default();
                code.topology.relay_fallback = Some(fallback);
                let effective = code.with_fallback(file).build().unwrap().topology;
                assert_eq!(effective.mode, mode);
                assert_eq!(effective.relay_fallback, fallback);
            }
        }
    }

    #[test]
    fn toml_accepts_only_the_documented_modes_and_keys() {
        for (name, mode) in [
            ("hub", TopologyMode::Hub),
            ("mesh", TopologyMode::Mesh),
            ("hybrid", TopologyMode::Hybrid),
        ] {
            let config: ParrotConfig = toml::from_str(&format!(
                "[topology]\nmode = \"{name}\"\nrelay_fallback = false\n"
            ))
            .unwrap();
            let effective = config.build().unwrap().topology;
            assert_eq!(effective.mode, mode);
            assert!(!effective.relay_fallback);
        }
        for source in [
            "[topology]\nmode = \"other\"",
            "[topology]\nmode = 1",
            "[topology]\nrelay_fallback = \"false\"",
            "[topology]\nrelay_fallbak = false",
        ] {
            assert!(toml::from_str::<ParrotConfig>(source).is_err(), "{source}");
        }
    }
}
