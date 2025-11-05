//! # parrot-app
//!
//! Parrot 应用体系（DEV_09 A 阶段）：声明式多引擎应用模型。
//!
//! - [`manifest`]：AppManifest 数据模型 + 校验器 + TOML 双形态（A1）
//! - `planner`：依赖 DAG 拓扑排序 + placement 过滤（A2）
//! - `assemble`：AssemblingContext 依赖序装配 + 失败回滚（A3）
//! - `cli`：`parrot-app run/replay` 共用本地组装宿主（需host feature）
//!
//! 分层铁律（09 §2.2）：不依赖 `parrot` 主 crate——应用包只依赖
//! parrot-api + parrot-app；引擎访问经 parrot-remote 协议面。

pub mod debug;
mod disposal;
pub mod durable;

pub mod assemble;
pub mod cleanup;
#[cfg(feature = "host")]
pub mod cli;
#[cfg(feature = "host")]
pub mod host;
pub mod limits;
pub mod manifest;
pub mod mirror;
pub mod orchestrator;
pub mod planner;
pub mod test_support;
mod wiring;

pub use assemble::{
    apply_overlay, hook_phase, DeployError, DeployResult, GatewayFactory, LocalDeployer,
    ParrotAppHook, ParrotSpawner,
};
pub use limits::{AdmissionError, ApplicationLimits};
pub use manifest::{
    component_config_flat, is_valid_semver, validate, validate_with_limits, AppLoadError,
    AppManifest, ArtifactRef, ComponentHooks, ComponentSpec, EngineKind, InstancePolicy,
    ManifestError, PlacementConstraint, UpgradePolicy, WireSpec,
};
pub use planner::{
    plan, plan_with_limits, CandidateNode, LocalTopology, Plan, PlanError, PlannedComponent,
    TopologyView,
};
