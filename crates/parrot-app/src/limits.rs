//! Host-owned admission budgets. A manifest cannot grant itself more resources.
//! Counts and encoded/expanded byte budgets bound metadata work; they are not an
//! RSS limit on actor code, a topology provider, or an allocator under pressure.
use crate::manifest::{AppManifest, ArtifactRef, ComponentSpec, InstancePolicy};
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplicationLimits {
    pub max_source_bytes: u64,
    pub max_components: u64,
    pub max_dependencies: u64,
    pub max_wires: u64,
    pub max_instances: u64,
    /// JSON-encoded model bytes, measured without materializing a second model.
    pub max_metadata_bytes: u64,
    /// Retained validation error text plus enum/string headers.
    pub max_diagnostic_bytes: u64,
    pub max_config_depth: u64,
    pub max_config_values: u64,
    /// Sum of concrete path and selected node string bytes plus String headers.
    pub max_expanded_bytes: u64,
    /// Checked on a trusted provider's returned Vec, before sorting or cloning.
    pub max_candidates: u64,
    pub max_candidate_bytes: u64,
}
impl Default for ApplicationLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 4 * 1024 * 1024,
            max_components: 1024,
            max_dependencies: 16_384,
            max_wires: 16_384,
            max_instances: 16_384,
            max_metadata_bytes: 16 * 1024 * 1024,
            max_diagnostic_bytes: 64 * 1024,
            max_config_depth: 64,
            max_config_values: 131_072,
            max_expanded_bytes: 16 * 1024 * 1024,
            max_candidates: 4096,
            max_candidate_bytes: 4 * 1024 * 1024,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    #[error("application {resource} budget exceeded: requested {requested}, limit {limit}")]
    Exceeded {
        resource: &'static str,
        requested: u64,
        limit: u64,
    },
    #[error("application {0} accounting overflow")]
    Overflow(&'static str),
    #[error("application metadata serialization failed: {0}")]
    Serialization(String),
    #[error("application allocation failed for {0}")]
    Allocation(&'static str),
}
pub(crate) fn check(
    resource: &'static str,
    requested: u64,
    limit: u64,
) -> Result<(), AdmissionError> {
    if requested > limit {
        Err(AdmissionError::Exceeded {
            resource,
            requested,
            limit,
        })
    } else {
        Ok(())
    }
}
pub(crate) fn add(
    resource: &'static str,
    total: &mut u64,
    amount: u64,
    limit: u64,
) -> Result<(), AdmissionError> {
    *total = total
        .checked_add(amount)
        .ok_or(AdmissionError::Overflow(resource))?;
    check(resource, *total, limit)
}
fn product(resource: &'static str, a: u64, b: u64) -> Result<u64, AdmissionError> {
    a.checked_mul(b).ok_or(AdmissionError::Overflow(resource))
}

impl ApplicationLimits {
    /// This check precedes graph construction, error cloning and instance expansion.
    pub fn check_manifest(&self, manifest: &AppManifest) -> Result<(), AdmissionError> {
        check(
            "components",
            manifest.components.len() as u64,
            self.max_components,
        )?;
        check("wires", manifest.wiring.len() as u64, self.max_wires)?;
        check(
            "mirrors",
            manifest.mirrors.len() as u64,
            self.max_wires.min(64),
        )?;
        let (mut instances, mut dependencies, mut expanded, mut values) = (0, 0, 0, 0);
        for component in &manifest.components {
            add(
                "instances",
                &mut instances,
                component.instances.instance_count() as u64,
                self.max_instances,
            )?;
            add(
                "dependencies",
                &mut dependencies,
                component.deps.len() as u64,
                self.max_dependencies,
            )?;
            add(
                "expanded bytes",
                &mut expanded,
                self.path_bytes(component)?,
                self.max_expanded_bytes,
            )?;
            self.component_values(component, &mut values)?;
        }
        if let Some(config) = &manifest.config_overlay {
            self.table(config, 1, &mut values)?;
        }
        self.check_metadata(manifest)
    }
    pub(crate) fn check_component(&self, component: &ComponentSpec) -> Result<(), AdmissionError> {
        check(
            "instances",
            component.instances.instance_count() as u64,
            self.max_instances,
        )?;
        self.path_bytes(component)?;
        self.component_values(component, &mut 0)?;
        self.check_metadata(component)
    }
    fn component_values(
        &self,
        component: &ComponentSpec,
        values: &mut u64,
    ) -> Result<(), AdmissionError> {
        if let Some(config) = &component.config {
            self.table(config, 1, values)?;
        }
        for artifact in std::iter::once(&component.artifact).chain(component.alt_artifact.iter()) {
            if let ArtifactRef::PyModule {
                runtime_env: Some(value),
                ..
            } = artifact
            {
                self.value(value, 1, values)?;
            }
        }
        Ok(())
    }

    fn table(
        &self,
        table: &toml::Table,
        depth: u64,
        values: &mut u64,
    ) -> Result<(), AdmissionError> {
        check("config depth", depth, self.max_config_depth)?;
        add("config values", values, 1, self.max_config_values)?;
        for value in table.values() {
            self.value(value, depth + 1, values)?;
        }
        Ok(())
    }
    fn value(
        &self,
        value: &toml::Value,
        depth: u64,
        values: &mut u64,
    ) -> Result<(), AdmissionError> {
        check("config depth", depth, self.max_config_depth)?;
        match value {
            toml::Value::Table(table) => self.table(table, depth, values),
            toml::Value::Array(array) => {
                add("config values", values, 1, self.max_config_values)?;
                for value in array {
                    self.value(value, depth + 1, values)?;
                }
                Ok(())
            }
            _ => add("config values", values, 1, self.max_config_values),
        }
    }
    pub(crate) fn check_metadata(
        &self,
        value: &impl serde::Serialize,
    ) -> Result<(), AdmissionError> {
        struct Counter {
            bytes: u64,
            limit: u64,
            failure: Option<AdmissionError>,
        }
        impl Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if let Err(error) = add(
                    "metadata bytes",
                    &mut self.bytes,
                    bytes.len() as u64,
                    self.limit,
                ) {
                    self.failure = Some(error);
                    return Err(std::io::Error::other(
                        "application metadata budget exceeded",
                    ));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut counter = Counter {
            bytes: 0,
            limit: self.max_metadata_bytes,
            failure: None,
        };
        let result = serde_json::to_writer(&mut counter, value);
        if let Some(error) = counter.failure {
            return Err(error);
        }
        result.map_err(|error| AdmissionError::Serialization(error.to_string()))
    }
    /// Exact path expansion charge in O(log n), without constructing n strings.
    pub(crate) fn path_bytes(&self, component: &ComponentSpec) -> Result<u64, AdmissionError> {
        let n = component.instances.instance_count() as u64;
        check("instances", n, self.max_instances)?;
        let header = std::mem::size_of::<String>() as u64;
        let prefix = (component.name.len() as u64)
            .checked_add(6 + header)
            .ok_or(AdmissionError::Overflow("expanded bytes"))?;
        let mut bytes = product("expanded bytes", n, prefix)?;
        if matches!(
            component.instances,
            InstancePolicy::Pool(_) | InstancePolicy::Sharded(_)
        ) {
            add("expanded bytes", &mut bytes, n, self.max_expanded_bytes)?; // '-'
            let (mut start, mut digits) = (0u64, 1u64);
            let mut end = 10u64;
            while start < n {
                add(
                    "expanded bytes",
                    &mut bytes,
                    product("expanded bytes", n.min(end) - start, digits)?,
                    self.max_expanded_bytes,
                )?;
                start = end;
                end = end.saturating_mul(10);
                digits += 1;
            }
        }
        check("expanded bytes", bytes, self.max_expanded_bytes)?;
        Ok(bytes)
    }
}

impl ApplicationLimits {
    /// Recheck a public/mutable Plan at consumption, including caller-supplied
    /// paths and node names. A planner's earlier check is not an authority token.
    pub fn check_plan(&self, plan: &crate::planner::Plan) -> Result<(), AdmissionError> {
        check(
            "planned components",
            plan.order.len() as u64,
            self.max_components,
        )?;
        let (mut instances, mut expanded, mut nodes, mut paths) = (0, 0, 0, 0);
        let (mut dependencies, mut values) = (0, 0);
        for component in &plan.order {
            add(
                "instances",
                &mut instances,
                component.spec.instances.instance_count() as u64,
                self.max_instances,
            )?;
            add(
                "dependencies",
                &mut dependencies,
                component.spec.deps.len() as u64,
                self.max_dependencies,
            )?;
            self.component_values(&component.spec, &mut values)?;
            let mut supplied_paths = 0;
            if let Some(shards) = &component.shard_plan {
                add(
                    "planned paths",
                    &mut paths,
                    shards.len() as u64,
                    self.max_instances,
                )?;
                for path in shards {
                    add(
                        "expanded bytes",
                        &mut supplied_paths,
                        path.len() as u64 + std::mem::size_of::<String>() as u64,
                        self.max_expanded_bytes,
                    )?;
                }
            }
            add(
                "expanded bytes",
                &mut expanded,
                supplied_paths.max(self.path_bytes(&component.spec)?),
                self.max_expanded_bytes,
            )?;
            add(
                "planned nodes",
                &mut nodes,
                component.nodes.len() as u64,
                self.max_instances,
            )?;
            for node in &component.nodes {
                add(
                    "expanded bytes",
                    &mut expanded,
                    node.len() as u64 + std::mem::size_of::<String>() as u64,
                    self.max_expanded_bytes,
                )?;
            }
        }
        self.check_metadata(plan)
    }
}
