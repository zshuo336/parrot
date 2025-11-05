//! B5（DEV_09 §5.2 MG1-4/MG11 jvm 侧）Rust 驱动真实 JVM 网关子进程的
//! admin-v2 全链集成测试。
//!
//! 流程：spawn `java -cp <jar> parrot.protocol.jvm.ParrotGatewayMain 0` →
//! 解析 `PARROT_JVM_PORT=<n>` → 原始 TCP 握手 → SYSTEM_EVENT(0x03) 四命令
//! → 校验 0x04 回执（encode/decode 走 parrot-remote admin_v2——与 Scala
//! AdminV2Codec 互为字节级对账 = MG11 契约）。
//!
//! `make test-jvm-gateway` 构建依赖并执行真实 JVM 用例。默认 Rust 测试将
//! 此外部依赖用例明确记为 ignored；显式执行时缺依赖或启动失败必须失败。
//! 不依赖 JVM 的启动超时/异常清理回归仍在默认测试中执行。

use std::process::Stdio;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use parrot_remote::admin::{decode_sys_event, SysEvent};
use parrot_remote::admin_v2::{
    encode_admin_cmd_v2, AdminArtifactRef, AdminCommandV2, AdminInstancePolicy, AdminReplyV2,
    ComponentDeploy,
};
use parrot_remote::frame::{frame_type, Frame, FrameHeader, PROTOCOL_VERSION, SEQ_NONE};
use parrot_remote::handshake::HandshakeBody;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

type R<T> = Result<T, String>;

async fn start_jvm() -> R<(u16, Child)> {
    let cp = jvm_classpath()?;
    let mut command = Command::new("java");
    command
        .arg("-cp")
        .arg(&cp)
        .arg("parrot.protocol.jvm.ParrotGatewayMain")
        .arg("0")
        .arg("120")
        .stderr(Stdio::inherit());
    let mut child = spawn_owned(&mut command)?;
    let port = wait_for_gateway(&mut child, Duration::from_secs(15)).await?;
    Ok((port, child))
}

fn spawn_owned(command: &mut Command) -> R<Child> {
    // Install ownership at spawn, including timeout, malformed output and panic.
    command
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("gateway process could not start: {error}"))
}

async fn wait_for_gateway(child: &mut Child, timeout: Duration) -> R<u16> {
    let startup = async {
        let stdout = child.stdout.take().ok_or("gateway stdout unavailable")?;
        // Bound both the whole startup transcript and its total duration;
        // a newline-free writer must not allocate indefinitely.
        let mut reader = BufReader::new(stdout.take(64 * 1024));
        let mut line = String::new();
        loop {
            line.clear();
            let count = reader
                .read_line(&mut line)
                .await
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Err(
                    "gateway exited or exceeded startup output limit before announcing a port"
                        .into(),
                );
            }
            if let Some(value) = line.strip_prefix("PARROT_JVM_PORT=") {
                return value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| "gateway announced an invalid port".into());
            }
        }
    };
    let result = tokio::time::timeout(timeout, startup)
        .await
        .unwrap_or_else(|_| Err("gateway startup deadline exceeded".into()));
    if result.is_err() {
        // kill() also waits/reaps; do not rely on a future runtime teardown.
        child
            .kill()
            .await
            .map_err(|error| format!("gateway startup failed and cleanup failed: {error}"))?;
    }
    result
}

fn jvm_classpath() -> R<std::ffi::OsString> {
    if let Some(cp) = std::env::var_os("PARROT_JVM_CP") {
        return if cp.is_empty() {
            Err("PARROT_JVM_CP is empty".into())
        } else {
            Ok(cp)
        };
    }
    let jar = std::env::var_os("PARROT_JVM_JAR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../interop/jvm/target/parrot-protocol-jvm-0.1.0.jar")
        });
    if !jar.is_file() {
        return Err(format!(
            "JVM jar missing: {}; run make test-jvm-gateway",
            jar.display()
        ));
    }
    let deps = std::fs::read_to_string(jar.parent().unwrap().join("cp.txt")).map_err(|error| {
        format!("JVM dependency classpath missing: {error}; run make test-jvm-gateway")
    })?;
    // The positive deployment fixture is compiled by Maven's testCompile.
    let mut paths = vec![jar.parent().unwrap().join("test-classes"), jar];
    paths.extend(std::env::split_paths(deps.trim()));
    std::env::join_paths(paths).map_err(|error| error.to_string())
}

// ── 原始帧客户端（握手 + SYSTEM_EVENT admin 往返）──────────────────

struct RawClient {
    stream: TcpStream,
    buf: BytesMut,
}

fn admin_frame(cmd: &AdminCommandV2) -> Frame {
    let req_id = cmd.req_id();
    Frame {
        header: FrameHeader {
            frame_len: 0,
            version: PROTOCOL_VERSION,
            frame_type: frame_type::SYSTEM_EVENT,
            flags: 0,
            correlation_id: req_id,
            hop_count: 0,
            hop_limit: 8,
            seq: SEQ_NONE,
        },
        path: "parrot://rust-b5-test/_admin".into(),
        type_key: String::new(),
        payload: encode_admin_cmd_v2(cmd),
    }
}

impl RawClient {
    async fn connect(port: u16) -> R<Self> {
        tokio::time::timeout(Duration::from_secs(15), Self::connect_inner(port))
            .await
            .map_err(|_| "connect/handshake deadline exceeded".to_string())?
    }

    async fn connect_inner(port: u16) -> R<Self> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| e.to_string())?;
        let hs_body = HandshakeBody {
            node_id: "rust-b5-test".into(),
            capabilities: 1,
            max_frame_len: 16 * 1024 * 1024,
            ..Default::default()
        };
        let mut body = BytesMut::new();
        hs_body.encode_tlv(&mut body).map_err(|e| e.to_string())?;
        let hs = Frame {
            header: FrameHeader {
                frame_len: 0,
                version: PROTOCOL_VERSION,
                frame_type: frame_type::HANDSHAKE,
                flags: 0,
                correlation_id: 1,
                hop_count: 0,
                hop_limit: 8,
                seq: SEQ_NONE,
            },
            path: String::new(),
            type_key: "__handshake__".into(),
            payload: body.freeze(),
        };
        let mut enc = BytesMut::new();
        hs.encode(&mut enc).map_err(|e| e.to_string())?;
        stream.write_all(&enc).await.map_err(|e| e.to_string())?;
        let mut c = Self {
            stream,
            buf: BytesMut::new(),
        };
        let ack = c.next_frame(Duration::from_secs(15)).await?;
        assert_eq!(
            ack.header.frame_type,
            frame_type::HANDSHAKE_ACK,
            "expect HANDSHAKE_ACK"
        );
        Ok(c)
    }

    async fn next_frame(&mut self, timeout: Duration) -> R<Frame> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(f) = Frame::decode(&mut self.buf).map_err(|e| e.to_string())? {
                return Ok(f);
            }
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remain.is_zero() {
                return Err("frame wait timeout".into());
            }
            let n = tokio::time::timeout(remain, self.stream.read_buf(&mut self.buf))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("connection closed".into());
            }
        }
    }

    async fn send(&mut self, f: Frame) -> R<()> {
        let mut enc = BytesMut::new();
        f.encode(&mut enc).map_err(|e| e.to_string())?;
        tokio::time::timeout(Duration::from_secs(15), self.stream.write_all(&enc))
            .await
            .map_err(|_| "frame write deadline exceeded".to_string())?
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// admin-v2 命令往返：SYSTEM_EVENT(tag 0x03) → 0x04 回执。
    async fn admin(&mut self, cmd: &AdminCommandV2) -> R<AdminReplyV2> {
        tokio::time::timeout(Duration::from_secs(15), self.admin_inner(cmd))
            .await
            .map_err(|_| "admin roundtrip deadline exceeded".to_string())?
    }

    async fn admin_inner(&mut self, cmd: &AdminCommandV2) -> R<AdminReplyV2> {
        self.send(admin_frame(cmd)).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            let back = tokio::time::timeout(remain, self.next_frame(Duration::from_secs(15)))
                .await
                .map_err(|e| e.to_string())??;
            if back.header.frame_type != frame_type::SYSTEM_EVENT {
                continue; // 心跳等无关帧
            }
            if back.payload.first() == Some(&0x04) {
                let reply = match decode_sys_event(&back.payload) {
                    Ok(SysEvent::AdminReplyV2(r)) => r,
                    other => return Err(format!("bad reply: {other:?}")),
                };
                if reply.req_id() == cmd.req_id() {
                    return Ok(reply);
                }
            }
        }
    }
}

impl RawClient {
    async fn ask(&mut self, path: &str, request: u64, payload: &[u8]) -> R<Frame> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let frame = Frame::ask(
                request,
                &format!("parrot://jvm-gw-1/jvm/user/{path}"),
                "bin:parrot.test.Echo#v1",
                Bytes::copy_from_slice(payload),
                None,
            );
            self.send(frame).await?;
            loop {
                let reply = self.next_frame(Duration::from_secs(15)).await?;
                if reply.header.correlation_id == request
                    && matches!(
                        reply.header.frame_type,
                        frame_type::REPLY | frame_type::REPLY_ERR
                    )
                {
                    return Ok(reply);
                }
            }
        })
        .await
        .map_err(|_| "business ask deadline exceeded".to_string())?
    }
}

fn stub_deploy(name: &str, pool: usize) -> AdminCommandV2 {
    AdminCommandV2::DeployComponent {
        req_id: 0,
        component: ComponentDeploy {
            name: name.into(),
            version: "9.9.9".into(),
            artifact: AdminArtifactRef::Props {
                factory: "app.stub".into(),
            },
            instances: if pool == 1 {
                AdminInstancePolicy::Singleton
            } else {
                AdminInstancePolicy::Pool { count: pool }
            },
            config: None,
        },
    }
}

#[tokio::test]
#[ignore = "requires Java 21 and a built gateway; run make test-jvm-gateway"]
async fn jvm_gateway_admin_v2_full_chain() {
    let (port, mut gateway) = start_jvm().await.expect("JVM gateway setup must succeed");
    let mut c = RawClient::connect(port).await.expect("connect + handshake");

    // ── MG11 jvm 侧：deploy（Props → jvm 方言应答 DialectMismatch 0x0A02）──
    // B5 首期 JVM 方言只吃 Jvm artifact；Props 属 parrot 方言——错误码即契约。
    let mut cmd = stub_deploy("echo", 1);
    cmd.set_req_id(101);
    let r = c.admin(&cmd).await.expect("deploy reply");
    match r {
        AdminReplyV2::Failed { code, detail, .. } => {
            assert_eq!(code, 0x0A02, "props on jvm = dialect mismatch: {detail}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // ── Status 空 → COMPONENT_NOT_FOUND（0x0A03）──
    let r = c
        .admin(&AdminCommandV2::ComponentStatus {
            req_id: 102,
            path_prefix: "/user/echo".into(),
        })
        .await
        .expect("status reply");
    match r {
        AdminReplyV2::Failed { code, .. } => assert_eq!(code, 0x0A03),
        other => panic!("expected Failed, got {other:?}"),
    }

    // ── Deploy Jvm artifact：不存在的类 → SPAWN_FAILED（0x0A06）──
    let cmd = AdminCommandV2::DeployComponent {
        req_id: 103,
        component: ComponentDeploy {
            name: "ghost".into(),
            version: "1.0.0".into(),
            artifact: AdminArtifactRef::Jvm {
                main_class: "parrot.no.SuchClass".into(),
                coords: None,
                uri: None,
            },
            instances: AdminInstancePolicy::Singleton,
            config: None,
        },
    };
    let r = c.admin(&cmd).await.expect("deploy ghost reply");
    match r {
        AdminReplyV2::Failed { code, .. } => assert_eq!(code, 0x0A06, "spawn failed expected"),
        other => panic!("expected Failed, got {other:?}"),
    }

    // ── 非 admin tag SYSTEM_EVENT（gossip 0x05）→ 网关吞帧不崩 ──
    let mut gossip = admin_frame(&stub_deploy("x", 1));
    gossip.payload = Bytes::from_static(&[0x05, 0x01, 0x02]);
    c.send(gossip).await.unwrap();

    // 网关仍活：再发 status 仍有回执（前向兼容断言）
    let r = c
        .admin(&AdminCommandV2::ComponentStatus {
            req_id: 201,
            path_prefix: "/user/ghost".into(),
        })
        .await
        .expect("post-gossip status reply");
    match r {
        AdminReplyV2::Failed { code, .. } => assert_eq!(code, 0x0A03),
        other => panic!("expected Failed, got {other:?}"),
    }
    // Positive lifecycle through actual deployment and business dispatch.
    for (request, name) in [(301, "rust-a"), (302, "rust-b")] {
        let reply = c
            .admin(&AdminCommandV2::DeployComponent {
                req_id: request,
                component: ComponentDeploy {
                    name: name.into(),
                    version: "1.2.3".into(),
                    artifact: AdminArtifactRef::Jvm {
                        main_class: "parrot.protocol.jvm.RustGatewayFixture".into(),
                        coords: None,
                        uri: None,
                    },
                    instances: AdminInstancePolicy::Singleton,
                    config: Some(b"configured:".to_vec()),
                },
            })
            .await
            .unwrap();
        assert_eq!(
            reply,
            AdminReplyV2::Deployed {
                req_id: request,
                instances: vec![format!("/user/{name}")]
            }
        );
    }
    let reply = c.ask("rust-a", 303, b"payload").await.unwrap();
    assert_eq!(reply.header.frame_type, frame_type::REPLY);
    assert_eq!(reply.type_key, "bin:parrot.test.ConfiguredEcho#v1");
    assert_eq!(reply.payload.as_ref(), b"configured:payload");
    match c
        .admin(&AdminCommandV2::ComponentStatus {
            req_id: 304,
            path_prefix: "/user/rust-a".into(),
        })
        .await
        .unwrap()
    {
        AdminReplyV2::Status { states, .. } => {
            assert_eq!(states.len(), 1);
            assert_eq!(states[0].path, "/user/rust-a");
            assert_eq!(states[0].version, "1.2.3");
            assert_eq!(states[0].state, "running");
        }
        other => panic!("expected deployed component status, got {other:?}"),
    }
    assert_eq!(
        c.admin(&AdminCommandV2::DrainComponent {
            req_id: 305,
            path_prefix: "/user/rust-a".into(),
            timeout_ms: 1000
        })
        .await
        .unwrap(),
        AdminReplyV2::Drained {
            req_id: 305,
            drained: 1,
            aborted: 0
        }
    );
    // Drained means the entire instance is retired, not merely quiescent.
    let drained = c.ask("rust-a", 306, b"after-drain").await.unwrap();
    assert_eq!(drained.header.frame_type, frame_type::REPLY_ERR);
    assert_eq!(
        parrot_remote::error::decode_err_payload(&drained.payload)
            .unwrap()
            .0,
        parrot_remote::error::ErrCode::ActorNotFound
    );
    for command in [
        AdminCommandV2::ComponentStatus {
            req_id: 309,
            path_prefix: "/user/rust-a".into(),
        },
        AdminCommandV2::StopComponent {
            req_id: 310,
            path_prefix: "/user/rust-a".into(),
        },
    ] {
        match c.admin(&command).await.unwrap() {
            AdminReplyV2::Failed { code, .. } => assert_eq!(code, 0x0A03),
            other => panic!("drained component must be absent, got {other:?}"),
        }
    }
    // Reusing the logical name requires complete retirement of the old actor.
    assert_eq!(
        c.admin(&AdminCommandV2::DeployComponent {
            req_id: 311,
            component: ComponentDeploy {
                name: "rust-a".into(),
                version: "1.2.4".into(),
                artifact: AdminArtifactRef::Jvm {
                    main_class: "parrot.protocol.jvm.RustGatewayFixture".into(),
                    coords: None,
                    uri: None,
                },
                instances: AdminInstancePolicy::Singleton,
                config: Some(b"replacement:".to_vec()),
            },
        })
        .await
        .unwrap(),
        AdminReplyV2::Deployed {
            req_id: 311,
            instances: vec!["/user/rust-a".into()],
        }
    );
    let replacement = c.ask("rust-a", 312, b"payload").await.unwrap();
    assert_eq!(replacement.header.frame_type, frame_type::REPLY);
    assert_eq!(replacement.payload.as_ref(), b"replacement:payload");
    assert_eq!(
        c.admin(&AdminCommandV2::StopComponent {
            req_id: 313,
            path_prefix: "/user/rust-a".into(),
        })
        .await
        .unwrap(),
        AdminReplyV2::Stopped { req_id: 313 }
    );
    let stopped = c.ask("rust-a", 307, b"after-stop").await.unwrap();
    assert_eq!(stopped.header.frame_type, frame_type::REPLY_ERR);
    assert_eq!(
        parrot_remote::error::decode_err_payload(&stopped.payload)
            .unwrap()
            .0,
        parrot_remote::error::ErrCode::ActorNotFound,
        "a timeout or codec failure does not prove component removal"
    );
    let survivor = c.ask("rust-b", 308, b"still-alive").await.unwrap();
    assert_eq!(survivor.header.frame_type, frame_type::REPLY);
    assert_eq!(survivor.payload.as_ref(), b"configured:still-alive");
    gateway
        .kill()
        .await
        .expect("terminate and reap JVM gateway");
}

#[cfg(unix)]
#[tokio::test]
async fn failed_gateway_startup_is_bounded_and_reaped() {
    for (script, expected) in [
        ("exec sleep 30", "deadline exceeded"),
        ("exit 7", "before announcing"),
        (
            "printf 'PARROT_JVM_PORT=bad\\n'; exec sleep 30",
            "invalid port",
        ),
        (
            "printf 'PARROT_JVM_PORT=0\\n'; exec sleep 30",
            "invalid port",
        ),
        ("head -c 65537 /dev/zero; exec sleep 30", "output limit"),
    ] {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        let mut child = spawn_owned(&mut command).unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            wait_for_gateway(&mut child, Duration::from_millis(500)),
        )
        .await
        .expect("startup failure must not hang")
        .unwrap_err();
        assert!(error.contains(expected), "{script}: {error}");
        assert!(
            child.try_wait().unwrap().is_some(),
            "child must be reaped before returning failure"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn ready_gateway_retains_process_until_explicit_cleanup() {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("printf 'startup log\\nPARROT_JVM_PORT=12345\\n'; exec sleep 30");
    let mut child = spawn_owned(&mut command).unwrap();
    assert_eq!(
        wait_for_gateway(&mut child, Duration::from_secs(2))
            .await
            .unwrap(),
        12345
    );
    assert!(child.try_wait().unwrap().is_none());
    child.kill().await.unwrap();
    assert!(child.try_wait().unwrap().is_some());
}
