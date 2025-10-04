//! RemoteNode 组装（05 §7 POC）：本地 parrot 系统 + 帧链路 + ingress 泵。
//!
//! P4 联邦扩展：
//! - `spawn_endpoint_dual`：同节点注册 thread + actix 双引擎（federation facade
//!   遍历查表，远程帧可命中任一引擎的 actor）。
//! - `spawn_endpoint_routed`：hub 端点带 PrefixRouter，未命中本地的帧按前缀
//!   转发到其它网关链路（网关两两互访的中继机制）。

use crate::ingress::{handle_frame_routed, PrefixRouter};
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
    /// P4：前缀路由表（普通端点为 None）。
    pub router: Option<Arc<PrefixRouter>>,
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
    spawn_endpoint_inner(link, None, false).await
}

/// P4：双引擎节点（thread + actix 同注册）。
pub async fn spawn_endpoint_dual(link: crate::transport::FrameLink) -> RemoteEndpoint {
    spawn_endpoint_inner(link, None, true).await
}

/// P4：带前缀路由的 hub 端点。
pub async fn spawn_endpoint_routed(
    link: crate::transport::FrameLink,
    router: Arc<PrefixRouter>,
) -> RemoteEndpoint {
    spawn_endpoint_inner(link, Some(router), false).await
}

async fn spawn_endpoint_inner(
    link: crate::transport::FrameLink,
    router: Option<Arc<PrefixRouter>>,
    dual_engine: bool,
) -> RemoteEndpoint {
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

    if dual_engine {
        let actix_sys = parrot::actix::system::ActixActorSystem::new()
            .await
            .unwrap();
        local
            .register_actix_system("actix".into(), actix_sys, false)
            .await
            .unwrap();
    }

    let local = Arc::new(local);

    let callbacks = Arc::new(CallbackRegistry::new());
    let mut link = link;
    let local_c = local.clone();
    let cb_c = callbacks.clone();
    let sender_c = link.sender.clone();
    let router_c = router.clone();

    tokio::spawn(async move {
        while let Some(f) = link.incoming.recv().await {
            handle_frame_routed(f, &local_c, &cb_c, &sender_c, router_c.as_ref()).await;
        }
    });

    RemoteEndpoint { local, callbacks, sender: link.sender, router }
}

/// 快速组装内存双节点（测试用）。
pub async fn endpoint_pair() -> (RemoteEndpoint, RemoteEndpoint) {
    let (la, lb) = crate::transport::memory_pair();
    let a = spawn_endpoint(la).await;
    let b = spawn_endpoint(lb).await;
    (a, b)
}
