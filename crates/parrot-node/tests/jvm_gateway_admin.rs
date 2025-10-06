//! B5（DEV_09 §5.2 MG1-4/MG11 jvm 侧）Rust 驱动真实 JVM 网关子进程的
//! admin-v2 全链集成测试。
//!
//! 流程：spawn `java -cp <jar> parrot.protocol.jvm.ParrotGatewayMain 0` →
//! 解析 `PARROT_JVM_PORT=<n>` → 原始 TCP 握手 → SYSTEM_EVENT(0x03) 四命令
//! → 校验 0x04 回执（encode/decode 走 parrot-remote admin_v2——与 Scala
//! AdminV2Codec 互为字节级对账 = MG11 契约）。
//!
//! 无 java / 无 jar 环境 → 本文件整体 skip（报阻塞点——DEV_09 纪律）。

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use parrot_remote::admin::{
    decode_sys_event, SysEvent,
};
use parrot_remote::admin_v2::{
    encode_admin_cmd_v2, AdminArtifactRef, AdminCommandV2, AdminInstancePolicy, AdminReplyV2,
    ComponentDeploy,
};
use parrot_remote::frame::{frame_type, Frame, FrameHeader, PROTOCOL_VERSION, SEQ_NONE};
use parrot_remote::handshake::HandshakeBody;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type R<T> = Result<T, String>;

fn find_jvm_setup() -> Option<(u16, impl Drop)> {
    if which_java().is_none() {
        eprintln!("jvm_gateway_admin: SKIP — java not found");
        return None;
    }
    let jar = find_jar()?;
    let cp = std::env::var("PARROT_JVM_CP")
        .ok()
        .or_else(|| {
            let cp_file = jar.parent().unwrap().join("cp.txt");
            std::fs::read_to_string(cp_file)
                .ok()
                .map(|deps| format!("{}:{}", jar.display(), deps.trim()))
        })
        .unwrap_or_else(|| jar.display().to_string());

    #[allow(clippy::zombie_processes)] // Reaper 守卫 drop 即 kill+wait（下方）
    let mut child = Command::new("java")
        .arg("-cp")
        .arg(&cp)
        .arg("parrot.protocol.jvm.ParrotGatewayMain")
        .arg("0")
        .arg("120")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn java");
    let port = {
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let mut port = None;
        for _ in 0..50 {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if let Some(p) = line.strip_prefix("PARROT_JVM_PORT=") {
                port = p.trim().parse::<u16>().ok();
                break;
            }
        }
        port
    }?;
    // 守尸守卫：drop 即 kill+wait（idle 120s 兜底；测试进程退出必清）
    struct Reaper(std::process::Child);
    impl Drop for Reaper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let reaper = Reaper(child);
    Some((port, reaper))
}

fn which_java() -> Option<std::path::PathBuf> {
    let out = Command::new("sh")
        .arg("-c")
        .arg("command -v java")
        .output()
        .ok()?;
    if out.status.success() {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if p.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(p))
        }
    } else {
        None
    }
}

fn find_jar() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("PARROT_JVM_JAR") {
        let pb = std::path::PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    // repo 根（crate manifest 上两级）+ cwd 双探测
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent()?.parent()?;
    for base in [repo_root, std::path::Path::new(".")] {
        let dir = base.join("interop/jvm/target");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut best = None;
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".jar")
                    && name.contains("parrot-protocol-jvm")
                    && !name.starts_with("original-")
                {
                    best = Some(e.path());
                }
            }
            if best.is_some() {
                return best;
            }
        }
    }
    eprintln!("jvm_gateway_admin: SKIP — interop/jvm jar not found (build via mvn package)");
    None
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
        hs_body.encode_tlv(&mut body);
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
        stream
            .write_all(&enc)
            .await
            .map_err(|e| e.to_string())?;
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
        self.stream.write_all(&enc).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// admin-v2 命令往返：SYSTEM_EVENT(tag 0x03) → 0x04 回执。
    async fn admin(&mut self, cmd: &AdminCommandV2) -> R<AdminReplyV2> {
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
async fn jvm_gateway_admin_v2_full_chain() {
    let Some((port, _reaper)) = find_jvm_setup() else { return };
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
}
