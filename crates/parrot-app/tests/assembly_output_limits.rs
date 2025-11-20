#[path = "support/manifest_output.rs"]
mod support;

use parrot_api::types::BoxedActorRef;
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner},
    planner::{plan_with_limits, LocalTopology, Plan},
    AppManifest, ApplicationLimits, ComponentSpec,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use support::*;

#[derive(Default)]
struct Probe {
    configured: AtomicUsize,
    started: AtomicUsize,
}
#[async_trait::async_trait]
impl ParrotSpawner for Probe {
    fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        self.configured.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn spawn_props(
        &self,
        _: &str,
        _: &[String],
        _: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        Err(DeployError::Engine("assembly output probe entered".into()))
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl GatewayFactory for Probe {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        self.configured.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("unexpected gateway")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("unexpected gateway")
    }
}
async fn assemble(
    context: &mut AssemblingContext,
    probe: &Probe,
    m: &AppManifest,
    plan: &Plan,
) -> Result<(), DeployError> {
    context
        .assemble(
            plan,
            &LocalDeployer {
                topology: &LocalTopology,
                gateway_factory: probe,
                parrot_spawner: probe,
            },
            m,
            &Default::default(),
        )
        .await
}
fn untouched(probe: &Probe) {
    assert_eq!(probe.configured.load(Ordering::SeqCst), 0);
    assert_eq!(probe.started.load(Ordering::SeqCst), 0);
}
fn entered(result: Result<(), DeployError>, probe: &Probe) {
    assert!(
        matches!(result, Err(DeployError::Engine(text)) if text.contains("assembly output probe entered"))
    );
    assert_eq!(probe.configured.load(Ordering::SeqCst), 2);
    assert_eq!(probe.started.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn amplified_comparison_rejects_before_host_callbacks_and_context_remains_reusable() {
    let m = repeated_headers();
    let limits = ApplicationLimits::default();
    let plan = plan_with_limits(&m, &LocalTopology, &limits).unwrap();
    let probe = Probe::default();
    let mut context = AssemblingContext::new_with_limits(limits);
    let result = assemble(&mut context, &probe, &m, &plan).await;
    budget(result, "source bytes");
    untouched(&probe);
    let m = manifest();
    let plan = plan_with_limits(&m, &LocalTopology, &limits).unwrap();
    entered(assemble(&mut context, &probe, &m, &plan).await, &probe);
}

#[tokio::test]
async fn exact_component_output_policy_and_one_byte_less_apply_before_configure() {
    let mut m = repeated_headers();
    m.components[0]
        .config
        .as_mut()
        .unwrap()
        .insert("text".into(), "中文".into());
    let bytes = toml::to_string(&m.components[0]).unwrap().len() as u64;
    let limits = ApplicationLimits {
        max_source_bytes: bytes,
        ..Default::default()
    };
    let plan = plan_with_limits(&m, &LocalTopology, &limits).unwrap();
    let probe = Probe::default();
    entered(
        assemble(
            &mut AssemblingContext::new_with_limits(limits),
            &probe,
            &m,
            &plan,
        )
        .await,
        &probe,
    );
    let short = ApplicationLimits {
        max_source_bytes: bytes - 1,
        ..limits
    };
    let probe = Probe::default();
    budget(
        assemble(
            &mut AssemblingContext::new_with_limits(short),
            &probe,
            &m,
            &plan,
        )
        .await,
        "source bytes",
    );
    untouched(&probe);
}

#[tokio::test]
async fn forged_plan_output_is_bounded_independently_of_the_manifest() {
    let m = manifest();
    let limits = ApplicationLimits::default();
    let mut plan = plan_with_limits(&m, &LocalTopology, &limits).unwrap();
    plan.order[0].spec.config = repeated_headers().components.remove(0).config;
    let probe = Probe::default();
    budget(
        assemble(
            &mut AssemblingContext::new_with_limits(limits),
            &probe,
            &m,
            &plan,
        )
        .await,
        "source bytes",
    );
    untouched(&probe);
}

#[tokio::test]
async fn admitted_custom_depth_and_special_values_preserve_plan_identity() {
    let mut m = manifest();
    m.components[0].config = Some(nested_table(65));
    m.components[0].config.as_mut().unwrap().extend(
        toml::from_str::<toml::Table>(
            "nan=nan\npositive=inf\nnegative=-inf\nzero=-0.0\nwhen=2026-11-13T00:00:00Z\nempty={}",
        )
        .unwrap(),
    );
    let limits = ApplicationLimits {
        max_config_depth: 65,
        ..Default::default()
    };
    let mut plan = plan_with_limits(&m, &LocalTopology, &limits).unwrap();
    let probe = Probe::default();
    entered(
        assemble(
            &mut AssemblingContext::new_with_limits(limits),
            &probe,
            &m,
            &plan,
        )
        .await,
        &probe,
    );
    plan.order[0]
        .spec
        .config
        .as_mut()
        .unwrap()
        .insert("zero".into(), 0.0.into());
    let probe = Probe::default();
    let result = assemble(
        &mut AssemblingContext::new_with_limits(limits),
        &probe,
        &m,
        &plan,
    )
    .await;
    assert!(
        matches!(result, Err(DeployError::Config(text)) if text.contains("plan differs from manifest"))
    );
    untouched(&probe);
}
