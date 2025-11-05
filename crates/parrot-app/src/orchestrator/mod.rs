//! E 阶段（DEV_09 §3.5）：Orchestrator——应用级生命周期编排。
//!
//! - [`supervisor`]（E1）：AppSupervisor——desired/observed 差分调和
//!   （diff 五形态 + 幂等；注入 Clock；desired 持久化 trait）
//! - [`rollout`]（E2）：RolloutTracker——升级状态机（显式 enum + 转移表，
//!   全弧含 Rollback；HotSwap/Rolling/Recreate 三策略）
//! - [`health`]（E3）：HealthWatch——链接差分 + 状态轮询 → observed 更新

#[cfg(unix)]
pub mod file_state;
pub mod health;
pub mod mirror;
#[cfg(unix)]
pub use mirror::MirrorSupervisor;
pub mod rollout;
pub mod state;
pub mod supervisor;
#[cfg(unix)]
pub use file_state::FileStateStore;

pub use health::{HealthWatch, LinkDiff, StatusPoll};
pub use rollout::{RolloutAction, RolloutError, RolloutEvent, RolloutPhase, RolloutTracker};
pub use state::{DesiredStateRecord, PendingUpgrade};
pub use supervisor::{
    default_node, desired_instances, AppSupervisor, Clock, DesiredStateStore, FakeClock,
    MemStateStore, ObservedNode, ObservedState, ReconcileAction, ReconcileReport, SystemClock,
};
