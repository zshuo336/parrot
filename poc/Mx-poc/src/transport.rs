//! L1 传输（05 §2）：帧链路抽象。
//!   FrameLink = 发送端 + 入站帧队列。
//! 内存形态（确定性测试）/ TCP 形态（真实网络栈）同一抽象。

use crate::frame::Frame;
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// 连接发送端（克隆共享；内部 mpsc 串行化写侧）。
#[derive(Clone)]
pub struct FrameSender {
    pub(crate) tx: mpsc::Sender<Frame>,
}

impl FrameSender {
    pub async fn send(&self, f: Frame) -> Result<(), String> {
        self.tx.send(f).await.map_err(|_| "writer dropped".to_string())
    }
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// 链路一侧：发送端 + 入站帧接收队列。
pub struct FrameLink {
    pub sender: FrameSender,
    pub incoming: mpsc::Receiver<Frame>,
}

// ---------------- 内存形态 ----------------

/// 内存链路对：a.sender 发的帧进 b.incoming，反之亦然。
pub fn memory_pair() -> (FrameLink, FrameLink) {
    let (tx_a_to_b, rx_a_to_b) = mpsc::channel(1024);
    let (tx_b_to_a, rx_b_to_a) = mpsc::channel(1024);
    (
        FrameLink { sender: FrameSender { tx: tx_a_to_b }, incoming: rx_b_to_a },
        FrameLink { sender: FrameSender { tx: tx_b_to_a }, incoming: rx_a_to_b },
    )
}

// ---------------- TCP 形态 ----------------

/// TCP 服务端：bind 后立即返回 (地址, link 接收器)；accept 在后台进行，
/// 客户端可立刻连接（连接建立时序：bind→addr 公布→connect/accept 并行）。
pub async fn tcp_listen() -> Result<(std::net::SocketAddr, tokio::sync::oneshot::Receiver<FrameLink>), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                stream.set_nodelay(true).ok();
                let _ = tx.send(stream_to_link(stream));
            }
            Err(_) => { /* rx 被 drop，客户端将超时 */ }
        }
    });
    Ok((addr, rx))
}

/// 兼容用法：阻塞等一个连接（需客户端已并发发起 connect）。
pub async fn tcp_listen_once() -> Result<(std::net::SocketAddr, FrameLink), String> {
    let (addr, mut rx) = tcp_listen().await?;
    let link = tokio::time::timeout(std::time::Duration::from_secs(10), &mut rx)
        .await
        .map_err(|_| "accept timeout".to_string())?
        .map_err(|_| "accept failed".to_string())?;
    Ok((addr, link))
}

/// TCP 客户端：连接并返回链路。
pub async fn tcp_connect(addr: std::net::SocketAddr) -> Result<FrameLink, String> {
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| "connect timeout".to_string())?
    .map_err(|e| e.to_string())?;
    stream.set_nodelay(true).ok();
    Ok(stream_to_link(stream))
}

/// 流 → 链路：读任务解帧入 incoming；写任务消费 sender 的帧。
fn stream_to_link(stream: tokio::net::TcpStream) -> FrameLink {
    let (mut rd, mut wr) = stream.into_split();
    let (tx_out, mut rx_out) = mpsc::channel::<Frame>(1024);
    let (tx_in, rx_in) = mpsc::channel::<Frame>(1024);

    // 写任务
    tokio::spawn(async move {
        while let Some(f) = rx_out.recv().await {
            let mut buf = BytesMut::new();
            f.encode(&mut buf);
            if wr.write_all(&buf).await.is_err() {
                break;
            }
        }
    });
    // 读任务
    tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(64 * 1024);
        loop {
            match rd.read_buf(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            while let Ok(Some(frame)) = Frame::decode(&mut buf) {
                if tx_in.send(frame).await.is_err() {
                    return;
                }
            }
        }
    });

    FrameLink { sender: FrameSender { tx: tx_out }, incoming: rx_in }
}
