//! admin-v2 部署协议（DEV_09 §3.2 B1）。
//!
//! 与 v1（`admin.rs`——单 actor spawn/stop）的差异：组件级生命周期。
//! 四命令（Deploy/Drain/Stop/Status）× 组件（多实例 + artifact + config），
//! 码点 0x03/0x04（Y1：老节点对未知 tag 的防御路径回 Unsupported——
//! `decode_sys_event` 未知 tag 报错，ingress 层 warn 丢帧）。
//!
//! BD-1（施工裁定）：`ComponentDeploy.artifact` 用本地镜像类型
//! `AdminArtifactRef`（serde 同形于 `parrot_app::ArtifactRef`）——
//! parrot-remote 不依赖 parrot-app（防循环依赖），由 parrot-app 侧
//! `From` 转换。
//!
//! 布局与 v1 同构：`[u8 tag][bincode(body)]`。

use bytes::BytesMut;

use crate::error::ErrCode;
use crate::frame::{frame_type, Frame};

// ─────────────────────────────────────────────────────────────
// 镜像类型（BD-1）——serde 形状与 parrot_app::manifest 逐一对应。
// 改动必须与 parrot-app 侧同步（From 转换 + roundtrip 测试互锁）。
// ─────────────────────────────────────────────────────────────

/// artifact 引用（`parrot_app::ArtifactRef` 的 serde 同形镜像）。
///
/// externally tagged（serde 默认 enum 形态）——与 parrot-app
/// `ArtifactRef` 的 TOML/bincode 形态一致（BD-1：serde 兼容同形，
/// bincode 不支持 internally tagged，两侧统一 externally tagged）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AdminArtifactRef {
    /// parrot 本地工厂名（inventory PropsFactory）。
    Props { factory: String },
    /// Erlang beam：模块目录 + 模块名。
    Beam { app: String },
    /// Python 模块：ray job working_dir（runtime_env 为 TOML 文本形态）。
    PyModule {
        module: String,
        runtime_env: Option<String>,
    },
    /// JVM：main 类 + 可选坐标（child-first loader 数据源）。
    Jvm {
        main_class: String,
        coords: Option<String>,
    },
    /// wasm 组件（C 阶段）。
    Wasm { digest: String, uri: String },
    /// 动态库（D 阶段）。
    Dylib {
        digest: String,
        uri: String,
        abi: u32,
    },
}

/// 实例策略镜像（`parrot_app::InstancePolicy` 同形）。
///
/// serde 形状：`{"singleton":{}}` / `{"pool":{"count":N}}` /
/// `{"sharded":{"count":N}}`（externally tagged——与 parrot-app TOML
/// 形态一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AdminInstancePolicy {
    Singleton,
    Pool { count: usize },
    Sharded { count: usize },
}

impl AdminInstancePolicy {
    /// 实例数（路径规划用）。
    pub fn instance_count(&self) -> usize {
        match self {
            Self::Singleton => 1,
            Self::Pool { count } | Self::Sharded { count } => *count,
        }
    }
}

/// 组件部署载荷（DeployComponent 命令体）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ComponentDeploy {
    pub name: String,
    pub version: String,
    pub artifact: AdminArtifactRef,
    pub instances: AdminInstancePolicy,
    /// toml 片段（组件级 config——目标节点方言各自解释；bincode/serde
    /// 原生支持 Option<Vec<u8>>——无须 bytes 适配层）。
    #[serde(default)]
    pub config: Option<Vec<u8>>,
}

// ─────────────────────────────────────────────────────────────
// 命令与回执
// ─────────────────────────────────────────────────────────────

/// admin-v2 四命令（DEV_09 §5.1）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AdminCommandV2 {
    /// 部署组件（多实例——目标节点按方言执行）。
    DeployComponent {
        req_id: u64,
        component: ComponentDeploy,
    },
    /// 排空组件（优雅：等在途消息处理完或超时中止）。
    DrainComponent {
        req_id: u64,
        path_prefix: String,
        timeout_ms: u64,
    },
    /// 停组件（立即——路径前缀下全部实例）。
    StopComponent { req_id: u64, path_prefix: String },
    /// 查组件状态（路径前缀匹配全部实例）。
    ComponentStatus { req_id: u64, path_prefix: String },
}

impl AdminCommandV2 {
    pub fn req_id(&self) -> u64 {
        match self {
            Self::DeployComponent { req_id, .. }
            | Self::DrainComponent { req_id, .. }
            | Self::StopComponent { req_id, .. }
            | Self::ComponentStatus { req_id, .. } => *req_id,
        }
    }

    pub fn set_req_id(&mut self, id: u64) {
        match self {
            Self::DeployComponent { req_id, .. }
            | Self::DrainComponent { req_id, .. }
            | Self::StopComponent { req_id, .. }
            | Self::ComponentStatus { req_id, .. } => *req_id = id,
        }
    }
}

/// 单实例状态报告。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ComponentStateReport {
    pub path: String,
    /// 方言相关状态串（parrot: starting/running/stopping/stopped；
    /// erl/ray/jvm 网关各自映射）。
    pub state: String,
    pub version: String,
}

/// admin-v2 回执（全部经 0x04 码点）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AdminReplyV2 {
    Deployed {
        req_id: u64,
        /// 实例路径列表（数量 = instances.instance_count()）。
        instances: Vec<String>,
    },
    Drained {
        req_id: u64,
        drained: usize,
        aborted: usize,
    },
    Stopped {
        req_id: u64,
    },
    Status {
        req_id: u64,
        states: Vec<ComponentStateReport>,
    },
    Failed {
        req_id: u64,
        /// 0x0A00+ = v2 扩展段；低段复用 ErrCode。
        code: u16,
        detail: String,
    },
}

impl AdminReplyV2 {
    pub fn req_id(&self) -> u64 {
        match self {
            Self::Deployed { req_id, .. }
            | Self::Drained { req_id, .. }
            | Self::Stopped { req_id }
            | Self::Status { req_id, .. }
            | Self::Failed { req_id, .. } => *req_id,
        }
    }
}

/// v2 错误码扩展段（ErrCode 13 之后起 0x0A00——与 v1 空间隔离）。
pub mod v2_err {
    /// artifact 获取失败（uri 不可达 / sha256 不符）。
    pub const ARTIFACT_FETCH: u16 = 0x0A00;
    /// artifact 校验失败（digest 不匹配）。
    pub const ARTIFACT_DIGEST: u16 = 0x0A01;
    /// 方言不支持该 artifact 形态（如 ray 收到 Beam）。
    pub const DIALECT_MISMATCH: u16 = 0x0A02;
    /// 组件未部署（Stop/Status/Drain 找不到实例）。
    pub const COMPONENT_NOT_FOUND: u16 = 0x0A03;
    /// drain 超时中止。
    pub const DRAIN_TIMEOUT: u16 = 0x0A04;
    /// B2：Props 工厂名未注册（find_factory 未命中）。
    pub const FACTORY_NOT_FOUND: u16 = 0x0A05;
    /// B2：实例 spawn 失败（路径冲突 / 调度器拒绝等）。
    pub const SPAWN_FAILED: u16 = 0x0A06;
}

// ─────────────────────────────────────────────────────────────
// 编解码（[u8 tag][bincode(body)]）
// ─────────────────────────────────────────────────────────────

pub use crate::admin::sys_event_tag;

/// 编码 v2 命令（tag 0x03）。
pub fn encode_admin_cmd_v2(cmd: &AdminCommandV2) -> bytes::Bytes {
    let mut b = BytesMut::new();
    b.extend_from_slice(&[sys_event_tag::ADMIN_CMD_V2]);
    b.extend_from_slice(&bincode::serde::encode_to_vec(cmd, bincode::config::standard()).unwrap());
    b.freeze()
}

/// 编码 v2 回执（tag 0x04）。
pub fn encode_admin_reply_v2(r: &AdminReplyV2) -> bytes::Bytes {
    let mut b = BytesMut::new();
    b.extend_from_slice(&[sys_event_tag::ADMIN_REPLY_V2]);
    b.extend_from_slice(&bincode::serde::encode_to_vec(r, bincode::config::standard()).unwrap());
    b.freeze()
}

/// v2 回执帧构造（目标节点 → 发起方；cid = req_id）。
pub fn reply_frame_v2(req_id: u64, reply_to: &str, reply: &AdminReplyV2) -> Frame {
    Frame {
        header: crate::frame::FrameHeader {
            frame_len: 0,
            version: crate::frame::PROTOCOL_VERSION,
            frame_type: frame_type::SYSTEM_EVENT,
            flags: 0,
            correlation_id: req_id,
            hop_count: 0,
            hop_limit: 8,
            seq: crate::frame::SEQ_NONE,
        },
        path: reply_to.to_string(),
        type_key: String::new(),
        payload: encode_admin_reply_v2(reply),
    }
}

/// v2 Failed 回执便捷构造。
pub fn failed_v2(req_id: u64, code: u16, detail: impl Into<String>) -> AdminReplyV2 {
    AdminReplyV2::Failed {
        req_id,
        code,
        detail: detail.into(),
    }
}

/// ErrCode → v2 回执码（低段直接透传数值）。
pub fn from_errcode(c: ErrCode) -> u16 {
    c as u16
}

// ─────────────────────────────────────────────────────────────
// 目标节点侧执行器 trait（方言挂接点）
// ─────────────────────────────────────────────────────────────

/// 组件生命周期执行器（目标节点方言实现：parrot Executor / ray / erl / jvm 网关）。
///
/// `handle_admin_command_v2` 按 req_id 匹配回执；本 trait 的四个方法
/// 返回 `AdminReplyV2`（req_id 已由调用方回填）。
#[async_trait::async_trait]
pub trait ComponentExecutor: Send + Sync {
    async fn deploy(&self, req_id: u64, c: &ComponentDeploy) -> AdminReplyV2;
    async fn drain(&self, req_id: u64, prefix: &str, timeout_ms: u64) -> AdminReplyV2;
    async fn stop(&self, req_id: u64, prefix: &str) -> AdminReplyV2;
    async fn status(&self, req_id: u64, prefix: &str) -> AdminReplyV2;
}

/// 目标节点侧：处理入站 AdminCommandV2（无执行器 → Failed DIALECT_MISMATCH）。
pub async fn handle_admin_command_v2(
    cmd: AdminCommandV2,
    executor: Option<&dyn ComponentExecutor>,
    back: &crate::transport::FrameSender,
    reply_to: &str,
) {
    let req_id = cmd.req_id();
    let reply = match (executor, cmd) {
        (Some(ex), AdminCommandV2::DeployComponent { req_id, component }) => {
            ex.deploy(req_id, &component).await
        }
        (
            Some(ex),
            AdminCommandV2::DrainComponent {
                req_id,
                path_prefix,
                timeout_ms,
            },
        ) => ex.drain(req_id, &path_prefix, timeout_ms).await,
        (
            Some(ex),
            AdminCommandV2::StopComponent {
                req_id,
                path_prefix,
            },
        ) => ex.stop(req_id, &path_prefix).await,
        (
            Some(ex),
            AdminCommandV2::ComponentStatus {
                req_id,
                path_prefix,
            },
        ) => ex.status(req_id, &path_prefix).await,
        (None, _) => failed_v2(
            req_id,
            v2_err::DIALECT_MISMATCH,
            "no ComponentExecutor installed on this node",
        ),
    };
    let _ = back.send(reply_frame_v2(req_id, reply_to, &reply)).await;
}

/// v2 回执挂起表（req_id → oneshot；与 v1 AdminPending 同构但类型独立）。
#[derive(Default)]
pub struct AdminPendingV2 {
    inner: std::sync::Mutex<
        std::collections::HashMap<u64, tokio::sync::oneshot::Sender<AdminReplyV2>>,
    >,
}

impl AdminPendingV2 {
    pub fn insert(&self, req_id: u64, tx: tokio::sync::oneshot::Sender<AdminReplyV2>) {
        self.inner.lock().unwrap().insert(req_id, tx);
    }
    pub fn complete(&self, r: AdminReplyV2) -> bool {
        self.inner
            .lock()
            .unwrap()
            .remove(&r.req_id())
            .is_some_and(|tx| tx.send(r).is_ok())
    }
    pub fn fail_all(&self, detail: &str) {
        for (_, tx) in self.inner.lock().unwrap().drain() {
            let _ = tx.send(AdminReplyV2::Failed {
                req_id: 0,
                code: ErrCode::ConnectionLost as u16,
                detail: detail.into(),
            });
        }
    }
}

// ─────────────────────────────────────────────────────────────
// golden vectors（B1 冻结件——只增不改；修改 = 协议 break）
// ─────────────────────────────────────────────────────────────

/// admin-v2 golden vector：固定命令/回执 → 固定 payload 字节。
/// 四语言（rust/erl/ray/jvm）共享的 admin-v2 wire 事实源。
pub struct AdminV2Vector {
    pub name: &'static str,
    /// 编码后完整 payload（含首字节 tag）。
    pub bytes: Vec<u8>,
}

/// 冻结向量集（OnceLock——内容确定性；dump-vectors 二进制从此导出）。
pub fn admin_v2_vectors() -> &'static [AdminV2Vector] {
    static V: std::sync::OnceLock<Vec<AdminV2Vector>> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        vec![
            AdminV2Vector {
                name: "v2-deploy-props-singleton",
                bytes: encode_admin_cmd_v2(&AdminCommandV2::DeployComponent {
                    req_id: 1,
                    component: ComponentDeploy {
                        name: "echo".into(),
                        version: "1.0.0".into(),
                        artifact: AdminArtifactRef::Props {
                            factory: "app.echo".into(),
                        },
                        instances: AdminInstancePolicy::Singleton,
                        config: None,
                    },
                })
                .to_vec(),
            },
            AdminV2Vector {
                name: "v2-drain",
                bytes: encode_admin_cmd_v2(&AdminCommandV2::DrainComponent {
                    req_id: 2,
                    path_prefix: "/user/idx".into(),
                    timeout_ms: 5000,
                })
                .to_vec(),
            },
            AdminV2Vector {
                name: "v2-stop",
                bytes: encode_admin_cmd_v2(&AdminCommandV2::StopComponent {
                    req_id: 3,
                    path_prefix: "/user/idx".into(),
                })
                .to_vec(),
            },
            AdminV2Vector {
                name: "v2-status",
                bytes: encode_admin_cmd_v2(&AdminCommandV2::ComponentStatus {
                    req_id: 4,
                    path_prefix: "/user/".into(),
                })
                .to_vec(),
            },
            AdminV2Vector {
                name: "v2-reply-deployed",
                bytes: encode_admin_reply_v2(&AdminReplyV2::Deployed {
                    req_id: 1,
                    instances: vec!["/user/echo".into()],
                })
                .to_vec(),
            },
            AdminV2Vector {
                name: "v2-reply-failed",
                bytes: encode_admin_reply_v2(&AdminReplyV2::Failed {
                    req_id: 9,
                    code: v2_err::DIALECT_MISMATCH,
                    detail: "no executor".into(),
                })
                .to_vec(),
            },
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 码点常量（2）──────────────────────────────────────
    #[test]
    fn tag_constants() {
        assert_eq!(sys_event_tag::ADMIN_CMD_V2, 0x03);
        assert_eq!(sys_event_tag::ADMIN_REPLY_V2, 0x04);
        // v1 码点不变（Y1）
        assert_eq!(sys_event_tag::ADMIN_CMD, 0x01);
        assert_eq!(sys_event_tag::ADMIN_REPLY, 0x02);
    }

    // ── 四命令 roundtrip（4）─────────────────────────────
    #[test]
    fn roundtrip_deploy() {
        let cmd = AdminCommandV2::DeployComponent {
            req_id: 1,
            component: ComponentDeploy {
                name: "crawler".into(),
                version: "1.2.0".into(),
                artifact: AdminArtifactRef::Props {
                    factory: "crawler.v2".into(),
                },
                instances: AdminInstancePolicy::Sharded { count: 3 },
                config: Some(b"[worker]\nconcurrency = 4\n".to_vec()),
            },
        };
        let p = encode_admin_cmd_v2(&cmd);
        assert_eq!(p[0], 0x03);
        let (got, _): (AdminCommandV2, usize) =
            bincode::serde::decode_from_slice(&p[1..], bincode::config::standard()).unwrap();
        assert_eq!(got, cmd);
    }

    #[test]
    fn roundtrip_drain_stop_status() {
        let cmds = [
            AdminCommandV2::DrainComponent {
                req_id: 2,
                path_prefix: "/user/idx".into(),
                timeout_ms: 5000,
            },
            AdminCommandV2::StopComponent {
                req_id: 3,
                path_prefix: "/user/idx".into(),
            },
            AdminCommandV2::ComponentStatus {
                req_id: 4,
                path_prefix: "/user/".into(),
            },
        ];
        for c in cmds {
            let p = encode_admin_cmd_v2(&c);
            assert_eq!(p[0], 0x03);
            let (got, _): (AdminCommandV2, usize) =
                bincode::serde::decode_from_slice(&p[1..], bincode::config::standard()).unwrap();
            assert_eq!(got, c);
        }
    }

    // ── 回执 roundtrip（3）────────────────────────────────
    #[test]
    fn roundtrip_replies() {
        let rs = [
            AdminReplyV2::Deployed {
                req_id: 1,
                instances: vec!["/user/c-0".into(), "/user/c-1".into()],
            },
            AdminReplyV2::Drained {
                req_id: 2,
                drained: 3,
                aborted: 1,
            },
            AdminReplyV2::Status {
                req_id: 4,
                states: vec![ComponentStateReport {
                    path: "/user/c-0".into(),
                    state: "running".into(),
                    version: "1.2.0".into(),
                }],
            },
        ];
        for r in rs {
            let p = encode_admin_reply_v2(&r);
            assert_eq!(p[0], 0x04);
            let (got, _): (AdminReplyV2, usize) =
                bincode::serde::decode_from_slice(&p[1..], bincode::config::standard()).unwrap();
            assert_eq!(got, r);
        }
    }

    #[test]
    fn roundtrip_failed_with_v2_code() {
        let r = failed_v2(9, v2_err::ARTIFACT_DIGEST, "sha256 mismatch");
        let p = encode_admin_reply_v2(&r);
        let (got, _): (AdminReplyV2, usize) =
            bincode::serde::decode_from_slice(&p[1..], bincode::config::standard()).unwrap();
        assert_eq!(got, r);
        assert_eq!(got.req_id(), 9);
    }

    // ── decode_sys_event 集成（v1 入口认识 0x03/0x04）（3）
    #[test]
    fn sys_event_dispatches_v2_cmd() {
        let cmd = AdminCommandV2::StopComponent {
            req_id: 5,
            path_prefix: "/user/x".into(),
        };
        let p = encode_admin_cmd_v2(&cmd);
        match crate::admin::decode_sys_event(&p).unwrap() {
            crate::admin::SysEvent::AdminCommandV2(got) => assert_eq!(got, cmd),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn sys_event_dispatches_v2_reply() {
        let r = AdminReplyV2::Stopped { req_id: 6 };
        let p = encode_admin_reply_v2(&r);
        match crate::admin::decode_sys_event(&p).unwrap() {
            crate::admin::SysEvent::AdminReplyV2(got) => assert_eq!(got, r),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn unknown_tag_still_rejected() {
        let err = crate::admin::decode_sys_event(&[0x99, 0, 0]).unwrap_err();
        assert!(err.to_string().contains("unknown SYSTEM_EVENT tag"));
    }

    // ── req_id 读写（2）───────────────────────────────────
    #[test]
    fn req_id_accessors() {
        let mut c = AdminCommandV2::ComponentStatus {
            req_id: 0,
            path_prefix: "/".into(),
        };
        assert_eq!(c.req_id(), 0);
        c.set_req_id(42);
        assert_eq!(c.req_id(), 42);
    }

    #[test]
    fn reply_req_id_all_variants() {
        let rs = [
            AdminReplyV2::Deployed {
                req_id: 1,
                instances: vec![],
            },
            AdminReplyV2::Drained {
                req_id: 2,
                drained: 0,
                aborted: 0,
            },
            AdminReplyV2::Stopped { req_id: 3 },
            AdminReplyV2::Status {
                req_id: 4,
                states: vec![],
            },
            AdminReplyV2::Failed {
                req_id: 5,
                code: 0x0A00,
                detail: String::new(),
            },
        ];
        for (i, r) in rs.iter().enumerate() {
            assert_eq!(r.req_id(), (i + 1) as u64);
        }
    }

    // ── artifact 形态全覆盖（6）──────────────────────────
    #[test]
    fn artifact_all_kinds_roundtrip() {
        let arts = [
            AdminArtifactRef::Props {
                factory: "f".into(),
            },
            AdminArtifactRef::Beam {
                app: "frontier".into(),
            },
            AdminArtifactRef::PyModule {
                module: "crawler_jobs".into(),
                runtime_env: None,
            },
            AdminArtifactRef::Jvm {
                main_class: "parrot.Crawler".into(),
                coords: Some("file:///tmp/a.jar".into()),
            },
            AdminArtifactRef::Wasm {
                digest: "sha256:aa".into(),
                uri: "file:///tmp/a.wasm".into(),
            },
            AdminArtifactRef::Dylib {
                digest: "sha256:bb".into(),
                uri: "file:///tmp/liba.so".into(),
                abi: 1,
            },
        ];
        for a in arts {
            let d = ComponentDeploy {
                name: "x".into(),
                version: "0.0.1".into(),
                artifact: a.clone(),
                instances: AdminInstancePolicy::Singleton,
                config: None,
            };
            let bytes = bincode::serde::encode_to_vec(&d, bincode::config::standard())
                .unwrap_or_else(|e| panic!("encode {:#?}: {e}", d.artifact));
            let (got, _): (ComponentDeploy, usize) =
                bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                    .unwrap_or_else(|e| panic!("decode {:#?}: {e}", d.artifact));
            assert_eq!(got, d);
        }
    }

    // ── 实例策略（3）─────────────────────────────────────
    #[test]
    fn instance_policy_counts() {
        assert_eq!(AdminInstancePolicy::Singleton.instance_count(), 1);
        assert_eq!(AdminInstancePolicy::Pool { count: 4 }.instance_count(), 4);
        assert_eq!(
            AdminInstancePolicy::Sharded { count: 8 }.instance_count(),
            8
        );
    }

    #[test]
    fn instance_policy_serde_shape() {
        // externally tagged——与 parrot-app InstancePolicy TOML 形态一致
        let s = serde_json::to_string(&AdminInstancePolicy::Pool { count: 3 }).unwrap();
        assert_eq!(s, r#"{"Pool":{"count":3}}"#);
    }

    #[test]
    fn config_none_default() {
        // config 字段缺省（serde default）——旧载荷兼容
        let bytes = bincode::serde::encode_to_vec(
            &ComponentDeploy {
                name: "n".into(),
                version: "1".into(),
                artifact: AdminArtifactRef::Props {
                    factory: "f".into(),
                },
                instances: AdminInstancePolicy::Singleton,
                config: None,
            },
            bincode::config::standard(),
        )
        .unwrap();
        let (got, _): (ComponentDeploy, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert!(got.config.is_none());
    }

    // ── pending 表（3）────────────────────────────────────
    #[tokio::test]
    async fn pending_v2_complete_routing() {
        let p = AdminPendingV2::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        p.insert(7, tx);
        assert!(p.complete(AdminReplyV2::Stopped { req_id: 7 }));
        assert!(matches!(rx.await, Ok(AdminReplyV2::Stopped { req_id: 7 })));
        assert!(!p.complete(AdminReplyV2::Stopped { req_id: 7 }), "迟到回执");
    }

    #[tokio::test]
    async fn pending_v2_fail_all() {
        let p = AdminPendingV2::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        p.insert(8, tx);
        p.fail_all("system shutdown");
        match rx.await {
            Ok(AdminReplyV2::Failed { code, detail, .. }) => {
                assert_eq!(code, ErrCode::ConnectionLost as u16);
                assert_eq!(detail, "system shutdown");
            }
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn v2_err_codes_in_extension_range() {
        const _: () = assert!(v2_err::ARTIFACT_FETCH >= 0x0A00);
        const _: () = assert!(v2_err::ARTIFACT_DIGEST >= 0x0A00);
        const _: () = assert!(v2_err::DIALECT_MISMATCH >= 0x0A00);
        const _: () = assert!(v2_err::COMPONENT_NOT_FOUND >= 0x0A00);
        const _: () = assert!(v2_err::DRAIN_TIMEOUT >= 0x0A00);
        const _: () = assert!(v2_err::FACTORY_NOT_FOUND >= 0x0A00);
        const _: () = assert!(v2_err::SPAWN_FAILED >= 0x0A00);
        // 与 v1 段隔离（运行时值断言——依赖 ErrCode 数值）
        assert!(v2_err::ARTIFACT_FETCH > ErrCode::Forbidden as u16);
    }

    // ── 执行器分发（3）────────────────────────────────────
    struct CountingExecutor {
        hits: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ComponentExecutor for CountingExecutor {
        async fn deploy(&self, req_id: u64, _c: &ComponentDeploy) -> AdminReplyV2 {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            AdminReplyV2::Deployed {
                req_id,
                instances: vec!["/user/x".into()],
            }
        }
        async fn drain(&self, _req_id: u64, _p: &str, _t: u64) -> AdminReplyV2 {
            unreachable!()
        }
        async fn stop(&self, _req_id: u64, _p: &str) -> AdminReplyV2 {
            unreachable!()
        }
        async fn status(&self, _req_id: u64, _p: &str) -> AdminReplyV2 {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn executor_dispatch_deploy() {
        let ex = CountingExecutor {
            hits: std::sync::atomic::AtomicUsize::new(0),
        };
        let cmd = AdminCommandV2::DeployComponent {
            req_id: 11,
            component: ComponentDeploy {
                name: "x".into(),
                version: "1".into(),
                artifact: AdminArtifactRef::Props {
                    factory: "f".into(),
                },
                instances: AdminInstancePolicy::Singleton,
                config: None,
            },
        };
        // 直接调执行器（不经网络——handle 的分发逻辑另有帧级测试）
        let r = ex
            .deploy(
                cmd.req_id(),
                match &cmd {
                    AdminCommandV2::DeployComponent { component, .. } => component,
                    _ => unreachable!(),
                },
            )
            .await;
        assert!(matches!(r, AdminReplyV2::Deployed { .. }));
        assert_eq!(ex.hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ── 帧构造（2）───────────────────────────────────────
    #[test]
    fn reply_frame_shape() {
        let f = reply_frame_v2(
            9,
            "parrot://n1/_admin",
            &AdminReplyV2::Stopped { req_id: 9 },
        );
        assert_eq!(f.header.frame_type, frame_type::SYSTEM_EVENT);
        assert_eq!(f.header.correlation_id, 9);
        assert_eq!(f.path, "parrot://n1/_admin");
        assert_eq!(f.payload[0], 0x04);
    }

    #[test]
    fn from_errcode_passthrough() {
        assert_eq!(from_errcode(ErrCode::ActorNotFound), 1);
        assert_eq!(from_errcode(ErrCode::Forbidden), 13);
    }

    // ── golden vectors 冻结断言（3）───────────────────────
    #[test]
    fn golden_vectors_deterministic() {
        let v1 = admin_v2_vectors();
        let v2 = admin_v2_vectors();
        assert!(std::ptr::eq(v1, v2), "OnceLock 单例");
        for (i, v) in v1.iter().enumerate() {
            assert!(!v.bytes.is_empty());
            let expect_tag = if i < 4 {
                sys_event_tag::ADMIN_CMD_V2
            } else {
                sys_event_tag::ADMIN_REPLY_V2
            };
            assert_eq!(v.bytes[0], expect_tag, "{} tag", v.name);
        }
    }

    #[test]
    fn golden_vectors_decode_back() {
        for v in admin_v2_vectors() {
            let ev = crate::admin::decode_sys_event(&v.bytes)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
            match &ev {
                crate::admin::SysEvent::AdminCommandV2(_)
                | crate::admin::SysEvent::AdminReplyV2(_) => {}
                other => panic!("{}: wrong variant {other:?}", v.name),
            }
        }
    }

    #[test]
    fn golden_deploy_cmd_stable_bytes() {
        // 首字节 tag + bincode 变体索引 + varint req_id（standard config：
        // 变长整数——req_id=1 单字节 0x01）确定性。
        let v = &admin_v2_vectors()[0];
        assert_eq!(v.name, "v2-deploy-props-singleton");
        assert_eq!(v.bytes[0], 0x03);
        assert_eq!(v.bytes[1], 0x00); // variant index: DeployComponent = 0
        assert_eq!(v.bytes[2], 0x01); // req_id = 1（varint）
        assert_eq!(v.bytes[3], 0x04); // name 长度 varint = 4
        assert_eq!(&v.bytes[4..8], b"echo");
    }

    #[test]
    fn golden_failed_reply_code_in_v2_range() {
        let v = &admin_v2_vectors()[5];
        assert_eq!(v.name, "v2-reply-failed");
        assert_eq!(v.bytes[0], 0x04);
        // Failed 变体 = 4：编码确定性（外部帧只验证可解性——具体字段偏移
        // 由 decode 断言）
        match crate::admin::decode_sys_event(&v.bytes).unwrap() {
            crate::admin::SysEvent::AdminReplyV2(AdminReplyV2::Failed { code, detail, .. }) => {
                assert_eq!(code, v2_err::DIALECT_MISMATCH);
                assert_eq!(detail, "no executor");
            }
            other => panic!("wrong: {other:?}"),
        }
    }
}
