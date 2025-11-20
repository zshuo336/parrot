use parrot_config::ParrotConfig;
use parrot_node::{configuration::NodeSettings, runtime::RunningNode};

fn settings() -> NodeSettings {
    let mut resolved = ParrotConfig::builder()
        .thread_shared_pool_size(1)
        .build()
        .unwrap();
    resolved.thread.shared_burst_workers_max = 0;
    NodeSettings {
        resolved,
        actors: Vec::new(),
        ready_file: None,
        wasm_epoch_ticks: None,
    }
}

#[test]
fn direct_settings_recheck_mutated_thread_values() {
    let mut settings = settings();
    settings.resolved.thread.shared_pool_size = 0;
    let error = settings.validate().unwrap_err().to_string();
    assert!(error.contains("thread.shared_pool_size"), "{error}");
}

#[test]
fn direct_settings_reject_unknown_and_duplicate_builtin_actors() {
    for actors in [vec!["unknown"], vec!["echo", "echo"]] {
        let mut settings = settings();
        settings.actors = actors.into_iter().map(str::to_owned).collect();
        let error = settings.validate().unwrap_err().to_string();
        assert!(error.contains("PARROT_ACTORS"), "{error}");
    }
    let mut settings = settings();
    settings.actors = vec!["kv".into(), "echo".into()];
    settings.validate().unwrap();
}

#[tokio::test]
async fn invalid_builtin_input_precedes_binding_and_process_singleton_publication() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    for actors in [vec!["unknown"], vec!["echo", "echo"]] {
        let mut settings = settings();
        settings.resolved.remote.node.bind = Some(occupied.local_addr().unwrap().to_string());
        settings.actors = actors.into_iter().map(str::to_owned).collect();
        let error = match RunningNode::start(&settings).await {
            Err(error) => error.to_string(),
            Ok(node) => {
                node.shutdown().await.unwrap();
                panic!("invalid builtin input started a node");
            }
        };
        assert!(error.contains("PARROT_ACTORS"), "{error}");
        assert!(parrot_node::NODE.get().is_none());
    }
}

#[tokio::test]
async fn invalid_thread_input_precedes_network_binding() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut settings = settings();
    settings.resolved.remote.node.bind = Some(occupied.local_addr().unwrap().to_string());
    settings.resolved.thread.shared_pool_size = 0;
    let error = match RunningNode::start(&settings).await {
        Err(error) => error.to_string(),
        Ok(node) => {
            node.shutdown().await.unwrap();
            panic!("invalid thread input started a node");
        }
    };
    assert!(error.contains("thread.shared_pool_size"), "{error}");
    assert!(occupied.local_addr().is_ok());
}
