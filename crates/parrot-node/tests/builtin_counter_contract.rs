use parrot_api::{address::ActorRef, errors::ActorError, types::BoxedMessage};
use parrot_node::{configuration::NodeSettings, runtime::RunningNode, NGetTotal, NInc, NTotal};
use parrot_remote::admin_v2::{
    AdminArtifactRef, AdminInstancePolicy, AdminReplyV2, ComponentDeploy, ComponentExecutor,
};
use std::time::Duration;

async fn total(actor: &dyn ActorRef, message: BoxedMessage) -> Result<u64, ActorError> {
    let reply = actor
        .send_with_timeout(message, Some(Duration::from_secs(3)))
        .await?;
    Ok(reply.downcast::<NTotal>().expect("counter total reply").0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overflow_rejection_preserves_counter_and_subsequent_progress() {
    let settings = NodeSettings::from_environment(|key| match key {
        "PARROT_NODE_ID" => Some("counter-boundary".into()),
        "PARROT_SCHEME" => Some("mem".into()),
        "PARROT_ACTORS" => Some("counter".into()),
        _ => None,
    })
    .unwrap();
    let node = RunningNode::start(&settings).await.unwrap();
    let executor = parrot_node::NodeComponentExecutor::new(node.thread.clone());
    let reply = executor
        .deploy(
            1,
            &ComponentDeploy {
                name: "counter-deployed".into(),
                version: "1.0.0".into(),
                artifact: AdminArtifactRef::Props {
                    factory: "deploy.counter".into(),
                },
                instances: AdminInstancePolicy::Singleton,
                config: None,
            },
        )
        .await;
    assert!(matches!(reply, AdminReplyV2::Deployed { .. }), "{reply:?}");
    for path in ["/user/counter", "/user/counter-deployed"] {
        let actor = node.thread.get_actor_ref(path).unwrap();
        check_counter(actor.as_ref()).await;
    }
    node.shutdown().await.unwrap();
}

async fn check_counter(actor: &dyn ActorRef) {
    assert_eq!(
        total(actor, Box::new(NInc(u64::MAX - 1))).await.unwrap(),
        u64::MAX - 1
    );
    for increment in [2, u64::MAX] {
        let error = total(actor, Box::new(NInc(increment))).await.unwrap_err();
        assert!(
            matches!(error, ActorError::MessageHandlingError(ref detail) if detail == "counter total overflow"),
            "overflow must be a business rejection: {error:?}"
        );
        assert_eq!(
            total(actor, Box::new(NGetTotal)).await.unwrap(),
            u64::MAX - 1
        );
    }
    for increment in [1, 0] {
        assert_eq!(
            total(actor, Box::new(NInc(increment))).await.unwrap(),
            u64::MAX
        );
    }
    assert!(matches!(
        total(actor, Box::new(NInc(1))).await,
        Err(ActorError::MessageHandlingError(detail)) if detail == "counter total overflow"
    ));
    assert_eq!(total(actor, Box::new(NGetTotal)).await.unwrap(), u64::MAX);
}
