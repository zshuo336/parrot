//! # parrot-app
//!
//! Parrot 应用体系（DEV_09 A 阶段）：声明式多引擎应用模型。
//!
//! - [`manifest`]：AppManifest 数据模型 + 校验器 + TOML 双形态（A1）
//! - `planner`：依赖 DAG 拓扑排序 + placement 过滤（A2）
//! - `assemble`：AssemblingContext 依赖序装配 + 失败回滚（A3）
//! - `cli`：`parrot-app run` 本地四引擎组装宿主（A4）
//!
//! 分层铁律（09 §2.2）：不依赖 `parrot` 主 crate——应用包只依赖
//! parrot-api + parrot-app；引擎访问经 parrot-remote 协议面。

pub mod assemble;
#[cfg(feature = "host")]
pub mod host;
pub mod manifest;
pub mod planner;

pub use assemble::{
    apply_overlay, hook_phase, DeployError, DeployResult, GatewayFactory, LocalDeployer,
    ParrotAppHook, ParrotSpawner,
};
pub use manifest::{
    component_config_flat, is_valid_semver, validate, AppLoadError, AppManifest, ArtifactRef,
    ComponentHooks, ComponentSpec, EngineKind, InstancePolicy, ManifestError, PlacementConstraint,
    UpgradePolicy, WireSpec,
};
pub use planner::{
    plan, CandidateNode, LocalTopology, Plan, PlanError, PlannedComponent, TopologyView,
};
