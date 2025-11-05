//! Exact component incarnations deployed by this invocation.
//! A transport failure is not proof that a deployment did not take effect.
use parrot_remote::{admin_v2::v2_err, RemoteActorSystem, RemoteError};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct Components(Vec<(String, String)>);

impl Components {
    /// Call after the initial stop succeeds (or confirms absence), before Deploy.
    /// Even an invalid/lost deployment receipt needs a retirement attempt.
    pub(crate) fn deploying(&mut self, node: &str, name: &str) {
        self.0.push((node.to_owned(), name.to_owned()));
    }

    /// HTTP and business work must finish first; keep the client alive until all
    /// independent nodes have replied or reached their admin request deadline.
    pub(crate) async fn retire(self, client: &Arc<RemoteActorSystem>) -> Result<(), String> {
        let mut tasks = tokio::task::JoinSet::new();
        for (node, name) in self.0 {
            let client = client.clone();
            tasks.spawn(async move {
                match client.stop_component(&node, &format!("/user/{name}")).await {
                    Ok(())
                    | Err(RemoteError::AdminRejected {
                        code: v2_err::COMPONENT_NOT_FOUND,
                        ..
                    }) => {
                        println!("[ws] retired {node}/{name}");
                        Ok(())
                    }
                    Err(error) => Err(format!(
                        "component retirement failed: {node}/{name}: {error}"
                    )),
                }
            });
        }
        let mut failures = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(error),
                Err(error) => failures.push(format!("component retirement task failed: {error}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            failures.sort();
            Err(failures.join("; "))
        }
    }
}
