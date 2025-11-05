//! Finite manifest → sandbox → real actor example, with explicit artifact trust.
//! `cargo run -p parrot-app --features wasm --example wasm_manifest -- FILE SHA256`
use parrot::thread::system::ThreadActorSystem;
use parrot_api::{address::ActorRefExt, types::BoxedActorRef};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
    host::{
        wasm::{WasmAsk, WasmTell},
        ThreadPropsSpawner,
    },
    manifest::{AppManifest, ComponentSpec},
    planner::{plan, LocalTopology},
};

struct NoGateway;
#[async_trait::async_trait]
impl GatewayFactory for NoGateway {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        Err(DeployError::Engine(
            "this example hosts local Wasm only".into(),
        ))
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [path, digest] = args.as_slice() else {
        return Err("usage: wasm_manifest FILE EXPECTED_SHA256 (use the documented fixture for echo/bump operations)".into());
    };
    let uri =
        url::Url::from_file_path(std::fs::canonicalize(path)?).map_err(|_| "invalid file path")?;
    let mut app = AppManifest::from_toml(
        r#"
name = "wasm-example"
version = "1.0.0"
[[components]]
name = "guest"
engine = "parrot"
artifact = { Wasm = { digest = "", uri = "" } }
[components.hooks]
on_start = "."
on_drain = "."
on_stop = "."
[components.config]
greeting = "hello from manifest"
"#,
    )?;
    app.components[0].artifact = parrot_app::manifest::ArtifactRef::Wasm {
        digest: digest.clone(),
        uri: uri.into(),
    };
    let engine = ThreadActorSystem::shared(Default::default());
    // Choose an explicit finite budget for this business example. Shared epoch
    // ticks measure elapsed scheduling time, not just guest CPU instructions.
    let spawner = ThreadPropsSpawner::new(engine.clone()).with_wasm_limits(
        parrot_wasm::WasmConfig {
            epoch_deadline: 1000,
            ..Default::default()
        },
        Default::default(),
    );
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    let run: Result<(), Box<dyn std::error::Error>> = async {
        context
            .assemble(
                &plan(&app, &LocalTopology)?,
                &deployer,
                &app,
                &Default::default(),
            )
            .await?;
        let actor = context.component_ref("guest").ok_or("missing guest")?;
        let greeting = actor
            .ask(WasmAsk {
                type_key: "config".into(),
                payload: b"greeting".to_vec(),
            })
            .await?;
        println!("{}", String::from_utf8(greeting.0)?);
        actor
            .ask(WasmTell {
                type_key: "bump".into(),
                payload: vec![7],
            })
            .await?;
        let counter = actor
            .ask(WasmAsk {
                type_key: "counter".into(),
                payload: vec![],
            })
            .await?;
        let bytes: [u8; 8] = counter
            .0
            .try_into()
            .map_err(|_| "counter must return 8 bytes")?;
        println!("counter={}", u64::from_le_bytes(bytes));
        Ok(())
    }
    .await;
    // Always retire resources, including on a failed business operation.
    let cleanup = context.teardown(&deployer).await;
    let shutdown = engine.shutdown_internal().await;
    let mut errors = Vec::new();
    if let Err(error) = run {
        errors.push(error.to_string());
    }
    if let Err(error) = cleanup {
        errors.push(error.to_string());
    }
    if let Err(error) = shutdown {
        errors.push(error.to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}
