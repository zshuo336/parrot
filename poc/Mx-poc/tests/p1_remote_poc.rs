//! RC1/RC2/RC6/RC8（05 §10 测试矩阵）：帧 + 双节点端到端。

use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use remote_poc::*;

// ---------- 测试 actor：ping/echo ----------

struct EchoActor {
    hits: u64,
}

impl Actor for EchoActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.hits += 1;
            if let Some(Ping(n)) = m.downcast_ref::<Ping>() {
                return Ok(Box::new(Pong(*n)) as BoxedMessage);
            }
            if let Some(Add(a, b)) = m.downcast_ref::<Add>() {
                return Ok(Box::new(a + b) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unsupported".into(),
            ))
        })
    }


    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// 在节点上 spawn echo actor。
async fn spawn_echo(ep: &remote_poc::RemoteEndpoint, path: &str) {
    let ts: std::sync::Arc<ThreadActorSystem> = ep.local.get_thread_system("main").unwrap();
    ts.spawn_at::<EchoActor>(EchoActor { hits: 0 }, path, None, Default::default())
        .await
        .unwrap();
}

// ---------- RC1: 帧编解码 golden ----------

#[test]
fn rc1_frame_codec_golden() {
    for (name, frame, bytes) in golden_vectors() {
        let mut buf = bytes::BytesMut::new();
        frame.encode(&mut buf);
        assert_eq!(&buf[..], &bytes[..], "golden re-encode mismatch: {name}");
        let mut dec_buf = bytes::BytesMut::from(&bytes[..]);
        let decoded = Frame::decode(&mut dec_buf).unwrap().unwrap();
        assert_eq!(decoded, frame);
        assert!(dec_buf.is_empty(), "no trailing bytes");
    }

    let (_, _, bytes) = golden_vectors().remove(0);
    let mut half = bytes::BytesMut::from(&bytes[..10]);
    assert!(Frame::decode(&mut half).unwrap().is_none(), "half frame must return None");

    let mut stuck = bytes::BytesMut::new();
    let f1 = Frame::ask(7, "/a", "bin:u:Ping", vec![1, 2]);
    let f2 = Frame::tell("/b", "bin:u:Add", vec![3]);
    f1.encode(&mut stuck);
    f2.encode(&mut stuck);
    let d1 = Frame::decode(&mut stuck).unwrap().unwrap();
    let d2 = Frame::decode(&mut stuck).unwrap().unwrap();
    assert_eq!((d1.correlation_id, d2.correlation_id), (7, 0));
    assert_eq!((d1.frame_type, d2.frame_type), (0x10, 0x13));
    assert!(stuck.is_empty());
}

// ---------- RC2: 双节点内存链路端到端 ----------

#[tokio::test]
async fn rc2_remote_ask_tell_over_memory_link() {
    CodecRegistry::reset();
    install_poc_messages();

    let (a, b) = endpoint_pair().await;
    spawn_echo(&b, "/user/echo").await;

    // A 通过远程 ref ask B 的 actor（远程消息落到真实 parrot actor handler）
    let remote = a.remote_ref("/user/echo");
    let pong = remote.send(Box::new(Ping(42))).await.unwrap();
    assert_eq!(pong.downcast_ref::<Pong>().unwrap(), &Pong(42));

    let sum = remote.send(Box::new(Add(20, 22))).await.unwrap();
    assert_eq!(*sum.downcast_ref::<u64>().unwrap(), 42);

    // tell（无回执）
    remote.deliver(Box::new(Ping(1))).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // 不存在路径 → ActorNotFound
    let ghost = a.remote_ref("/user/ghost");
    let err = ghost.send(Box::new(Ping(1))).await.unwrap_err();
    match err {
        parrot_api::errors::ActorError::ActorNotFound(_) => {}
        other => panic!("expect ActorNotFound, got {other:?}"),
    }

    // 未注册类型 → NotRemotable（不出网）
    #[derive(Debug)]
    struct Secret;
    let err2 = remote.send(Box::new(Secret)).await.unwrap_err();
    assert!(err2.to_string().contains("not remotable"), "got {err2:?}");
}

// ---------- RC6: 超时语义 ----------

#[tokio::test]
async fn rc6_timeout_semantics() {
    CodecRegistry::reset();
    install_poc_messages();

    struct SlowActor {
        delay_ms: u64,
    }
    impl Actor for SlowActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn receive_message<'a>(
            &'a mut self,
            m: BoxedMessage,
            _c: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if let Some(Ping(n)) = m.downcast_ref::<Ping>() {
                    if self.delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
                    }
                    return Ok(Box::new(Pong(*n)) as BoxedMessage);
                }
                Err(parrot_api::errors::ActorError::MessageHandlingError("unsupported".into()))
            })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    let (a, b) = endpoint_pair().await;
    let ts = b.local.get_thread_system("main").unwrap();
    ts.spawn_at::<SlowActor>(SlowActor { delay_ms: 400 }, "/user/slow", None, Default::default())
        .await
        .unwrap();

    let remote = a.remote_ref("/user/slow");
    // 无超时：正常返回
    let pong = remote.send(Box::new(Ping(1))).await.unwrap();
    assert_eq!(pong.downcast_ref::<Pong>().unwrap().0, 1);

    // 超时 < 处理时长 → Timeout 错误
    let start = std::time::Instant::now();
    let err = remote
        .send_with_timeout(Box::new(Ping(2)), Some(std::time::Duration::from_millis(50)))
        .await
        .unwrap_err();
    match &err {
        parrot_api::errors::ActorError::TimeoutDetail(_) => {}
        other => panic!("expect TimeoutDetail, got {other:?}"),
    }
    assert!(start.elapsed() < std::time::Duration::from_millis(300), "timeout should fire early");
}

// ---------- RC8: TCP 传输（真实网络栈） ----------

#[tokio::test]
async fn rc8_remote_over_real_tcp() {
    CodecRegistry::reset();
    install_poc_messages();

    // 先 bind 拿地址（accept 后台），客户端再 connect —— 无时序死锁
    let (addr, rx) = tcp_listen().await.unwrap();
    let server_task = tokio::spawn(async move {
        let link = rx.await.expect("accept");
        let server = spawn_endpoint(link).await;
        spawn_echo(&server, "/user/echo").await;
        server
    });
    // 给服务端 spawn echo 的时间
    let clink = tcp_connect(addr).await.unwrap();
    let client = spawn_endpoint(clink).await;
    let _server = server_task.await.unwrap();

    let remote = client.remote_ref("/user/echo");
    for i in 0..100u64 {
        let pong = remote.send(Box::new(Ping(i))).await.unwrap();
        assert_eq!(pong.downcast_ref::<Pong>().unwrap().0, i);
    }
    let sum = remote.send(Box::new(Add(1_000_000, 1))).await.unwrap();
    assert_eq!(*sum.downcast_ref::<u64>().unwrap(), 1_000_001);
}

// ---------- 路径 UTF-8 往返（跨语言锚点） ----------

#[test]
fn path_format_utf8_roundtrip() {
    let f = Frame::ask(1, "/user/节点A", "bin:u:Ping", vec![]);
    let mut buf = bytes::BytesMut::new();
    f.encode(&mut buf);
    let mut dec = buf;
    let out = Frame::decode(&mut dec).unwrap().unwrap();
    assert_eq!(out.path, "/user/节点A");
}
