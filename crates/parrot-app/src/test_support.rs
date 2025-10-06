//! G2/M5（DEV_09 §3.7）：测试公共装配入口。
//!
//! `test_assemble(manifest)` —— 一行完成 plan + AssemblingContext 装配，
//! 返回 refs 视图 + teardown。装配类集成测试的公共收敛点：
//! 断言逻辑零改动，装配样板不再逐测试复制。

use crate::assemble::{
    AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner,
};
use crate::manifest::AppManifest;
use crate::planner::{self, TopologyView};
use parrot_api::types::BoxedActorRef;

/// 装配结果（refs 只读视图 + teardown——消费 ctx 逆序停用）。
pub struct Assembled {
    ctx: AssemblingContext,
    deployer: LocalDeployerStorage,
}

/// 部署器三元组持有（teardown 需再借 LocalDeployer——生命周期由存储保）。
struct LocalDeployerStorage {
    topology: Box<dyn TopologyView + Send + Sync>,
    gateway_factory: Box<dyn GatewayFactory + Send + Sync>,
    parrot_spawner: Box<dyn ParrotSpawner + Send + Sync>,
}

impl Assembled {
    /// 单组件首实例引用便捷取用。
    pub fn first(&self, comp: &str) -> Option<BoxedActorRef> {
        self.ctx
            .component_refs(comp)
            .and_then(|r| r.first())
            .map(|r| r.clone_boxed())
    }

    /// 组件全部实例引用。
    pub fn refs(&self, comp: &str) -> Option<&[BoxedActorRef]> {
        self.ctx.component_refs(comp)
    }

    /// 已启动组件序（依赖序——断言装配序用）。
    pub fn started(&self) -> &[String] {
        self.ctx.started()
    }

    /// 组件实例路径。
    pub fn paths(&self, comp: &str) -> Option<&[String]> {
        self.ctx.component_paths(comp)
    }

    /// 逆序停用（装配的镜像回卷——原 deployer 三元组）。
    pub async fn teardown(mut self) -> Result<(), DeployError> {
        let d = LocalDeployer {
            topology: &*self.deployer.topology,
            gateway_factory: &*self.deployer.gateway_factory,
            parrot_spawner: &*self.deployer.parrot_spawner,
        };
        self.ctx.teardown(&d).await
    }
}

/// 测试装配入口：manifest → plan → assemble（注入部署器三元组）。
///
/// 形态收敛：与 `cmd_run` 同构（同 plan/AssemblingContext/overlay 语义）。
pub async fn test_assemble(
    manifest: &AppManifest,
    topology: Box<dyn TopologyView + Send + Sync>,
    gateway_factory: Box<dyn GatewayFactory + Send + Sync>,
    parrot_spawner: Box<dyn ParrotSpawner + Send + Sync>,
    cfg: &parrot_config::ParrotConfig,
) -> Result<Assembled, DeployError> {
    let plan = planner::plan(manifest, &*topology)
        .map_err(|e| DeployError::Config(format!("plan: {e}")))?;
    let deployer = LocalDeployer {
        topology: &*topology,
        gateway_factory: &*gateway_factory,
        parrot_spawner: &*parrot_spawner,
    };
    let mut ctx = AssemblingContext::new();
    ctx.assemble(&plan, &deployer, manifest, cfg).await?;
    Ok(Assembled {
        ctx,
        deployer: LocalDeployerStorage {
            topology,
            gateway_factory,
            parrot_spawner,
        },
    })
}

/// 便捷重载（cfg 默认构造——多数测试无配置切面诉求）。
pub async fn test_assemble_default_cfg(
    manifest: &AppManifest,
    topology: Box<dyn TopologyView + Send + Sync>,
    gateway_factory: Box<dyn GatewayFactory + Send + Sync>,
    parrot_spawner: Box<dyn ParrotSpawner + Send + Sync>,
) -> Result<Assembled, DeployError> {
    test_assemble(
        manifest,
        topology,
        gateway_factory,
        parrot_spawner,
        &parrot_config::ParrotConfig::default(),
    )
    .await
}
