//! Finite trusted native manifest example. This executes code in the host process.
//! `cargo run -p parrot-app --features dylib --example dylib_manifest -- FILE --trust-native-sha256 SHA256`
use parrot::thread::system::ThreadActorSystem;
use parrot_api::{address::ActorRefExt, types::BoxedActorRef};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
    host::{dylib::DylibAsk, ThreadPropsSpawner},
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
            "this example hosts local trusted native code only".into(),
        ))
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [path, flag, digest] = args.as_slice() else {
        return Err("usage: dylib_manifest FILE --trust-native-sha256 EXPECTED_SHA256 (use the documented hostcomp fixture)".into());
    };
    if flag != "--trust-native-sha256" {
        return Err("explicit native trust flag required".into());
    }
    let uri =
        url::Url::from_file_path(std::fs::canonicalize(path)?).map_err(|_| "invalid file path")?;
    let mut app = AppManifest::from_toml(
        r#"
name = "native-example"
version = "1.0.0"
[[components]]
name = "guest"
engine = "parrot"
artifact = { Dylib = { digest = "", uri = "", abi = 1 } }
[components.hooks]
on_start = "."
on_drain = "."
on_stop = "."
[components.config]
greeting = "hello from manifest"
"#,
    )?;
    app.components[0].artifact = parrot_app::manifest::ArtifactRef::Dylib {
        abi: 1,
        digest: digest.clone(),
        uri: uri.into(),
    };
    let engine = ThreadActorSystem::shared(Default::default());
    // SAFETY: the operator explicitly trusts this exact artifact AND its
    // dependencies to satisfy the ABI loader's native code/hook contract.
    let spawner = unsafe { ThreadPropsSpawner::new(engine.clone()).with_trusted_dylib(digest) }?;
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
        let reply = actor
            .ask(DylibAsk {
                type_key: "echo".into(),
                payload: b"hello from native manifest".to_vec(),
            })
            .await?;
        println!("{}", String::from_utf8(reply.0)?);
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
