//! RemoteNode 组装（05 §7 POC）：本地 parrot 系统 + 帧链路 + ingress 泵。

use crate::ingress::handle_frame;
use crate::remote_ref::{CallbackRegistry, RemoteActorRef};
use crate::transport::FrameSender;
use parrot::system::ParrotActorSystem;
use std::sync::Arc;

/// 一个远程节点端点。
pub struct RemoteEndpoint {
    /// 本地 parrot 系统（Arc 共享给 ingress 泵）。
    pub local: Arc<ParrotActorSystem>,
    pub callbacks: Arc<CallbackRegistry>,
    pub(crate) sender: FrameSender,
}

impl RemoteEndpoint {
    /// 指向对端节点上某路径的远程 ref。
    pub fn remote_ref(&self, path: impl Into<String>) -> RemoteActorRef {
        RemoteActorRef::new(path, self.sender.clone(), self.callbacks.clone())
    }
    pub fn sender(&self) -> FrameSender {
        self.sender.clone()
    }
    /// 模拟断连：fail 全部挂起 ask。
    pub fn simulate_disconnect(&self) -> usize {
        self.callbacks.fail_all("connection lost")
    }
}

/// 起一个节点：本地 parrot 系统（thread 引擎注册）+ ingress 泵。
pub async fn spawn_endpoint(link: crate::transport::FrameLink) -> RemoteEndpoint {
    let local = ParrotActorSystem::new(parrot_api::system::ActorSystemConfig::default())
        .await
        .unwrap();
    let ts = parrot::thread::system::ThreadActorSystem::shared(
        parrot::thread::config::ThreadActorSystemConfig::default(),
    );
    local
        .register_thread_system("main".into(), ts.clone(), true)
        .await
        .unwrap();
    let local = Arc::new(local);

    let callbacks = Arc::new(CallbackRegistry::new());
    let mut link = link;
    let local_c = local.clone();
    let cb_c = callbacks.clone();
    let sender_c = link.sender.clone();

    tokio::spawn(async move {
        while let Some(f) = link.incoming.recv().await {
            handle_frame(f, &local_c, &cb_c, &sender_c).await;
        }
    });

    RemoteEndpoint { local, callbacks, sender: link.sender }
}

/// 快速组装内存双节点（测试用）。
pub async fn endpoint_pair() -> (RemoteEndpoint, RemoteEndpoint) {
    let (la, lb) = crate::transport::memory_pair();
    let a = spawn_endpoint(la).await;
    let b = spawn_endpoint(lb).await;
    (a, b)
}
