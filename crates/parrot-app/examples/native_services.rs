//! Finite discovery and business ASK using the repository messagingcomp fixture.
use parrot::thread::ThreadActorSystem;
use parrot_api::{address::ActorRefExt, types::BoxedActorRef};
use parrot_app::{
    assemble::ParrotSpawner,
    host::{dylib::*, ThreadPropsSpawner},
    ArtifactRef,
};
use parrot_remote::{acl::AclRules, receptionist::ReceptionistFacade};
use std::sync::Arc;

async fn call(
    actor: &BoxedActorRef,
    command: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let result = actor
        .ask(DylibAsk {
            type_key: command.into(),
            payload,
        })
        .await?
        .0;
    if result.get(..4) != Some(0u32.to_le_bytes().as_slice()) {
        return Err(format!("native operation {command}: {result:?}").into());
    }
    Ok(result[4..].to_vec())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [file, flag, digest] = args.as_slice() else {
        return Err(
            "usage: native_services MESSAGINGCOMP_FILE --trust-native-sha256 EXPECTED_SHA256"
                .into(),
        );
    };
    if flag != "--trust-native-sha256" {
        return Err("explicit native trust flag required".into());
    }
    let digest = digest.to_str().ok_or("digest must be UTF-8")?.to_owned();
    let uri =
        url::Url::from_file_path(std::fs::canonicalize(file)?).map_err(|_| "invalid file path")?;
    let engine = ThreadActorSystem::shared(Default::default());
    engine.set_receptionist_gateway(Arc::new(
        ReceptionistFacade::new("native-example").with_acl(AclRules::permissive()),
    ));
    let run: Result<(), Box<dyn std::error::Error>> = async {
        // SAFETY: the operator explicitly trusts this artifact and dependencies
        // to obey the ABI's native pointer, threading and unload obligations.
        let spawner =
            unsafe { ThreadPropsSpawner::new(engine.clone()).with_trusted_dylib(&digest) }?
                .with_dylib_services(vec![
                    NativeServiceGrant {
                        source_path: "/user/provider".into(),
                        artifact_digest: digest.clone(),
                        key: "native/business".into(),
                        operation: NativeServiceOperation::Register,
                    },
                    NativeServiceGrant {
                        source_path: "/user/consumer".into(),
                        artifact_digest: digest.clone(),
                        key: "native/business".into(),
                        operation: NativeServiceOperation::Subscribe,
                    },
                ])?
                .with_dylib_messaging(vec![NativeMessageGrant {
                    source_path: "/user/consumer".into(),
                    artifact_digest: digest.clone(),
                    destination: "/user/provider".into(),
                    request_type_key: "bin:parrot.dylib.Ask#v1".into(),
                    operation: NativeMessageOperation::Ask {
                        reply_type_key: "bin:parrot.dylib.Reply#v1".into(),
                    },
                }])?;
        let artifact = ArtifactRef::Dylib {
            digest,
            uri: uri.into(),
            abi: 1,
        };
        let config: toml::Table = toml::from_str(
            "request='bin:parrot.dylib.Ask#v1'\nexpected='bin:parrot.dylib.Reply#v1'",
        )?;
        let provider = spawner
            .spawn_artifact(&artifact, &["/user/provider".into()], Some(&config))
            .await?
            .remove(0);
        let consumer = spawner
            .spawn_artifact(&artifact, &["/user/consumer".into()], Some(&config))
            .await?
            .remove(0);
        call(&provider, "service:register", vec![]).await?;
        let payload = parrot_api::message::serde_remote_serialize(&DylibAsk {
            type_key: "echo".into(),
            payload: b"hello from discovered native service".to_vec(),
        })?;
        let response = call(&consumer, "service:discover", payload).await?;
        if response.get(..4) != Some(0u32.to_le_bytes().as_slice()) {
            return Err(format!("discovered ASK failed: {response:?}").into());
        }
        let reply: DylibReply = parrot_api::message::serde_remote_deserialize(&response[4..])?;
        if reply.0 != b"hello from discovered native service" {
            return Err("unexpected native business reply".into());
        }
        println!("{}", String::from_utf8(reply.0)?);
        Ok(())
    }
    .await;
    let shutdown = engine.shutdown_internal().await;
    match (run, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(cleanup)) => Err(format!("{error}; shutdown: {cleanup}").into()),
    }
}
