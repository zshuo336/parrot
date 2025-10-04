//! 职责：MemoryTransport——进程内 duplex 双工对载体（测试/嵌入用，05 §2.3）。
//!
//! 全局端点表按 node_id 注册：connect 侧查表把 server 半边投递给目标节点，
//! 目标节点 accept() 取出后走与 tcp 完全一致的握手时序（run_connection）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};

use tokio::io::DuplexStream;
use tokio::sync::mpsc;

use crate::error::RemoteError;
use crate::frame::Frame;
use crate::handshake::HandshakeBody;
use crate::node::NodeAddr;
use crate::transport::{
    run_connection, ConnParams, ConnSide, ConnectionHandle, FrameSender, OnDisconnect, Transport,
};

type InboundItem = (Frame, FrameSender, String);
type Offer = mpsc::Sender<DuplexStream>;

/// 进程内单例端点表：node_id → accept 侧投递通道。
fn hub() -> &'static Mutex<HashMap<String, Offer>> {
    static HUB: OnceLock<Mutex<HashMap<String, Offer>>> = OnceLock::new();
    HUB.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct MemoryTransport {
    handshake: HandshakeBody,
    inbound: mpsc::Sender<InboundItem>,
    on_disconnect: OnDisconnect,
    shutdown: tokio::sync::watch::Sender<bool>,
    node_id: String,
    accept_tx: Offer,
    accept_rx: tokio::sync::Mutex<mpsc::Receiver<DuplexStream>>,
}

impl MemoryTransport {
    pub fn new(
        handshake: HandshakeBody,
        inbound: mpsc::Sender<InboundItem>,
        on_disconnect: OnDisconnect,
        shutdown: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        let (accept_tx, accept_rx) = mpsc::channel(16);
        let node_id = handshake.node_id.clone();
        // 同 node_id 后注册覆盖（测试进程内重复构造以最后为准）
        hub().lock().unwrap().insert(node_id.clone(), accept_tx.clone());
        Self {
            handshake,
            inbound,
            on_disconnect,
            shutdown,
            node_id,
            accept_tx,
            accept_rx: tokio::sync::Mutex::new(accept_rx),
        }
    }
}

impl Drop for MemoryTransport {
    fn drop(&mut self) {
        // 端点表清理（仅当仍指向本实例的通道）
        let mut g = hub().lock().unwrap();
        if let Some(tx) = g.get(&self.node_id) {
            if tx.same_channel(&self.accept_tx) {
                g.remove(&self.node_id);
            }
        }
    }
}

#[async_trait::async_trait]
impl Transport for MemoryTransport {
    async fn connect(&self, addr: &NodeAddr) -> Result<ConnectionHandle, RemoteError> {
        let target = {
            let g = hub().lock().unwrap();
            g.get(&addr.node_id).cloned().ok_or_else(|| {
                RemoteError::Transport(format!("mem endpoint {} not found", addr.node_id))
            })?
        };
        let (client, server) = tokio::io::duplex(64 * 1024);
        target.send(server).await.map_err(|_| {
            RemoteError::Transport(format!("mem peer {} not accepting", addr.node_id))
        })?;
        run_connection(
            client,
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: self.handshake.clone(),
                inbound: self.inbound.clone(),
                on_disconnect: self.on_disconnect.clone(),
                shutdown: self.shutdown.subscribe(),
            },
        )
        .await
    }

    async fn listen(&self, _bind: SocketAddr) -> Result<(), RemoteError> {
        Ok(()) // mem 载体无绑定语义（端点随 new() 注册）
    }

    async fn accept(&self) -> Result<ConnectionHandle, RemoteError> {
        let server = {
            let mut rx = self.accept_rx.lock().await;
            match rx.recv().await {
                Some(s) => s,
                None => return Err(RemoteError::Transport("mem accept side closed".into())),
            }
        };
        run_connection(
            server,
            ConnParams {
                side: ConnSide::Accept,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: self.handshake.clone(),
                inbound: self.inbound.clone(),
                on_disconnect: self.on_disconnect.clone(),
                shutdown: self.shutdown.subscribe(),
            },
        )
        .await
    }

    fn scheme(&self) -> &'static str {
        "mem"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::frame_type;
    use std::sync::Arc;
    use std::time::Duration;

    // 两个 MemoryTransport 经 duplex 握手并交换一帧（与 tcp 测试同构）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mem_connect_handshake_roundtrip() {
        let (in_a, _rx_a) = mpsc::channel::<InboundItem>(64);
        let (in_b, mut rx_b) = mpsc::channel::<InboundItem>(64);
        let noop: OnDisconnect = Arc::new(|_| {});
        let (sd_a, _) = tokio::sync::watch::channel(false);
        let (sd_b, _) = tokio::sync::watch::channel(false);
        let ma = MemoryTransport::new(
            HandshakeBody {
                node_id: "mem-a-t1".into(),
                ..Default::default()
            },
            in_a,
            noop.clone(),
            sd_a,
        );
        let mb = MemoryTransport::new(
            HandshakeBody {
                node_id: "mem-b-t1".into(),
                ..Default::default()
            },
            in_b,
            noop,
            sd_b,
        );
        // 并行 accept/connect（串行 await 会死锁：accept 等投递）
        let accept_fut = tokio::spawn(async move { mb.accept().await });
        let conn_a = ma
            .connect(&NodeAddr::mem("mem-b-t1"))
            .await
            .expect("mem connect");
        let conn_b = accept_fut.await.unwrap().expect("mem accept");
        assert_eq!(conn_a.node_id, "mem-b-t1");
        assert_eq!(conn_b.node_id, "mem-a-t1");

        // a → b 一帧
        conn_a
            .sender
            .send(Frame::ask(
                1,
                "/u/echo",
                "bin:x::Ping",
                bytes::Bytes::from_static(b"P"),
                None,
            ))
            .await
            .unwrap();
        let (got, _, _) = tokio::time::timeout(Duration::from_secs(3), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.header.frame_type, frame_type::ASK);
        assert_eq!(got.path, "/u/echo");
    }
}
