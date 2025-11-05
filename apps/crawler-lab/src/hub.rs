//! Adapter for the shared business kernel, trusted ABI component or WIT guest.
//! Networking stays in the host; Tick results contain the actual business work.
use crate::cli;
use crawler_lab_core::{Config, FrontierEntry, Hub, Tick, MAX_BYTES};
use parrot::thread::system::ThreadActorSystem;
use parrot_api::{address::ActorRefExt, types::BoxedActorRef};
use parrot_app::{
    assemble::ParrotSpawner,
    host::{dylib::DylibAsk, wasm::WasmAsk, ThreadPropsSpawner},
    manifest::ArtifactRef,
};
use std::sync::Arc;

enum Runtime {
    Embedded(Hub),
    Component {
        actor: BoxedActorRef,
        engine: Option<Arc<ThreadActorSystem>>,
        wasm: bool,
    },
}
pub struct BusinessHub {
    runtime: Runtime,
    ticks: u64,
    fetched: u64,
    form: &'static str,
}
impl BusinessHub {
    pub async fn new(form: cli::Hub, config: Config) -> Result<Self, String> {
        config.validate().map_err(|e| e.to_string())?;
        let (runtime, form) = match form {
            cli::Hub::Embedded => (
                Runtime::Embedded(Hub::new(config).map_err(|e| e.to_string())?),
                "embedded",
            ),
            selected => {
                let (path, digest, wasm) = match selected {
                    cli::Hub::Dylib { path, digest } => (path, digest, false),
                    cli::Hub::Wasm { path, digest } => (path, digest, true),
                    cli::Hub::Embedded => unreachable!(),
                };
                let path = std::fs::canonicalize(path).map_err(|e| format!("hub artifact: {e}"))?;
                let uri = url::Url::from_file_path(path)
                    .map_err(|_| "hub artifact path is not a local file")?
                    .to_string();
                let artifact = if wasm {
                    ArtifactRef::Wasm {
                        uri,
                        digest: digest.clone(),
                    }
                } else {
                    ArtifactRef::Dylib {
                        uri,
                        digest: digest.clone(),
                        abi: 1,
                    }
                };
                let engine = ThreadActorSystem::shared(Default::default());
                let spawner = ThreadPropsSpawner::new(engine.clone()).with_wasm_limits(
                    parrot_wasm::WasmConfig {
                        // Maximum admitted batch: 256 pages plus 65,536 outlinks.
                        // Validated through the actual guest, not the embedded kernel.
                        fuel_per_message: 1_000_000_000,
                        epoch_deadline: 100,
                        memory_limit_mb: 128,
                    },
                    Default::default(),
                );
                let spawner = if wasm {
                    spawner
                } else {
                    // SAFETY: this CLI path requires the operator's explicit
                    // --trust-native-sha256 authorization of code and dependencies.
                    unsafe { spawner.with_trusted_dylib(&digest) }.map_err(|e| e.to_string())?
                };
                let mut settings = toml::Table::new();
                settings.insert(
                    "settings".into(),
                    toml::Value::String(serde_json::to_string(&config).map_err(|e| e.to_string())?),
                );
                let spawned = spawner
                    .spawn_artifact(&artifact, &["/user/crawler".into()], Some(&settings))
                    .await;
                let actor = match spawned {
                    Ok(mut actors) => actors.pop().ok_or("hub factory returned no actor")?,
                    Err(error) => {
                        let cleanup = spawner.retry_pending_cleanup().await;
                        let shutdown = engine.shutdown_internal().await;
                        return Err(format!(
                            "hub spawn: {error}; cleanup: {cleanup:?}; shutdown: {shutdown:?}"
                        ));
                    }
                };
                (
                    Runtime::Component {
                        actor,
                        engine: Some(engine),
                        wasm,
                    },
                    if wasm { "wasm" } else { "dylib" },
                )
            }
        };
        Ok(Self {
            runtime,
            ticks: 0,
            fetched: 0,
            form,
        })
    }
    /// The assembling context owns this actor and its shared engine.
    pub fn assembled(actor: BoxedActorRef, wasm: bool, form: &'static str) -> Self {
        Self {
            runtime: Runtime::Component {
                actor,
                engine: None,
                wasm,
            },
            ticks: 0,
            fetched: 0,
            form,
        }
    }
    async fn ask(&mut self, key: &str, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        match &mut self.runtime {
            Runtime::Embedded(hub) => hub
                .handle(key, &payload, MAX_BYTES)
                .map_err(|e| e.to_string()),
            Runtime::Component { actor, wasm, .. } => {
                if *wasm {
                    actor
                        .ask(WasmAsk {
                            type_key: key.into(),
                            payload,
                        })
                        .await
                        .map(|r| r.0)
                        .map_err(|e| e.to_string())
                } else {
                    actor
                        .ask(DylibAsk {
                            type_key: key.into(),
                            payload,
                        })
                        .await
                        .map(|r| r.0)
                        .map_err(|e| e.to_string())
                }
            }
        }
    }
    pub async fn tick(&mut self, entries: &[FrontierEntry]) -> Result<Tick, String> {
        let sequence = self.ticks.checked_add(1).ok_or("hub sequence overflow")?;
        let request =
            crawler_lab_core::encode_request(sequence, entries).map_err(|e| e.to_string())?;
        let reply = self.ask("crawl/Tick", request).await?;
        let tick = Tick::decode(&reply).map_err(|e| e.to_string())?;
        if tick.sequence != sequence || tick.fetched != self.fetched + tick.pages.len() as u64 {
            return Err("hub returned inconsistent progress".into());
        }
        self.ticks = sequence;
        self.fetched = tick.fetched;
        Ok(tick)
    }
    pub async fn close(mut self) -> Result<(), String> {
        let expected = [self.ticks.to_le_bytes(), self.fetched.to_le_bytes()].concat();
        let progress = self.ask("crawl/Progress", vec![]).await;
        let mut errors = Vec::new();
        match progress {
            Ok(actual) if actual == expected => println!(
                "[lab] hub business: form={} ticks={} fetched={}",
                self.form, self.ticks, self.fetched
            ),
            other => errors.push(format!("hub progress mismatch: {other:?}")),
        }
        match self.runtime {
            Runtime::Embedded(mut hub) => hub.drain(),
            Runtime::Component {
                actor,
                engine: Some(engine),
                ..
            } => {
                if let Err(error) = actor.stop().await {
                    errors.push(format!("hub stop: {error}"));
                }
                if let Err(error) = engine.shutdown_internal().await {
                    errors.push(format!("hub engine shutdown: {error}"));
                }
            }
            Runtime::Component { engine: None, .. } => {}
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
