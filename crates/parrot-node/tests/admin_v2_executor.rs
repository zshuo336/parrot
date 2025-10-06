//! B2（DEV_09 §5.1）parrot Executor 场景/集成测试。
//!
//! 覆盖：
//! - ArtifactChannel：cache_dir 环境变量、file:// uri 取件、sha256 校验
//!   （正确/不匹配/缺文件/非 file 协议）、Props 直通、跨方言拒绝
//! - exec_deploy_v2：Singleton/Pool/Sharded 路径、未知工厂、非 Props 方言、
//!   引擎不同源拒绝、实例可收消息（真实引擎行为验证）
//! - NodeComponentExecutor：Status 版本报告、Stop 前缀匹配（不误吞近名）、
//!   Drain 排空（慢 actor 在途消息→超时中止）、COMPONENT_NOT_FOUND
//! - exec_command_v2：四命令直通
//!
//! 引擎共享：进程级专用 runtime（Box::leak——避免 per-test runtime drop
//! 杀 worker 的已知坑，见 parrot-app 场景测试同款处理）。

use std::sync::Arc;


use parrot::thread::system::ThreadActorSystem;
use parrot_api::types::BoxedMessage;
use parrot_node::{
    exec_command_v2, exec_deploy_v2, ArtifactChannel, NEcho, NInc, NTotal,
    NodeComponentExecutor,
};
use parrot_remote::admin_v2::{
    AdminArtifactRef, AdminCommandV2, AdminInstancePolicy, AdminReplyV2, ComponentDeploy,
    ComponentExecutor as _, v2_err,
};

// ══════════════════════════════════════════════════════════════════════════
// 共享引擎（NODE 单例装配——PropsFactory fn 指针依赖）
// ══════════════════════════════════════════════════════════════════════════

static INIT: std::sync::Once = std::sync::Once::new();

fn engine() -> Arc<ThreadActorSystem> {
    INIT.call_once(|| {
        // 进程级专用 runtime：泄漏保活（worker/timer 永生——避免 per-test
        // runtime drop 杀 worker 的已知坑）
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("dedicated engine runtime");
        let handle = rt.handle().clone();
        Box::leak(Box::new(rt));
        let ts = ThreadActorSystem::shared_with_handle(Default::default(), handle.clone());
        // facade：PropsFactory 只用 ts；NODE 装配一致性走 ptr_eq
        let facade = {
            let (tx, rx) = std::sync::mpsc::channel();
            handle.spawn(async move {
                let f = parrot::system::ParrotActorSystem::new(Default::default())
                    .await
                    .expect("facade init");
                tx.send(Arc::new(f)).ok();
            });
            rx.recv().expect("facade")
        };
        parrot_node::NODE
            .set(parrot_node::NodeState {
                facade,
                ts: ts.clone(),
            })
            .ok()
            .expect("NODE singleton init");
    });
    parrot_node::NODE.get().expect("NODE").ts.clone()
}

fn deploy_cmd(name: &str, factory: &str, policy: AdminInstancePolicy) -> ComponentDeploy {
    ComponentDeploy {
        name: name.into(),
        version: "1.0.0".into(),
        artifact: AdminArtifactRef::Props {
            factory: factory.into(),
        },
        instances: policy,
        config: None,
    }
}

fn temp_artifact(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("parrot-b2-test-{tag}-{}", std::process::id()));
    std::fs::write(&p, bytes).unwrap();
    p
}

fn sha256_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

async fn ask_total(ts: &Arc<ThreadActorSystem>, path: &str) -> u64 {
    let r = ts
        .get_actor_ref(path)
        .unwrap_or_else(|| panic!("actor {path} not in registry"));
    let reply = r
        .send(Box::new(parrot_node::NGetTotal) as BoxedMessage)
        .await
        .expect("ask NGetTotal");
    reply
        .downcast_ref::<NTotal>()
        .unwrap_or_else(|| panic!("expect NTotal"))
        .0
}

// ══════════════════════════════════════════════════════════════════════════
// ArtifactChannel（7）
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn cache_dir_default_and_env() {
    // 缺省 /tmp/parrot-artifacts（环境未设时）
    // 注：并行测试环境隔离——用子进程验证 env 覆盖
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--nocapture")
        .env("PARROT_ARTIFACT_DIR", "/custom/art-dir")
        .arg("cache_dir_env_probe")
        .output()
        .unwrap();
    assert!(out.status.success(), "probe child failed");
    // 本进程：缺省路径形态正确（不依赖具体值是否被其它测试污染）
    let d = ArtifactChannel::cache_dir();
    assert!(d.is_absolute() || d.starts_with("/tmp"), "absolute cache dir");
}

// 子进程探针（cache_dir env 覆盖验证——由上一个测试驱动）
#[test]
fn cache_dir_env_probe() {
    if std::env::var("PARROT_ARTIFACT_DIR").as_deref() != Ok("/custom/art-dir") {
        return; // 直接跑（非探针模式）——无断言
    }
    assert_eq!(ArtifactChannel::cache_dir(), std::path::PathBuf::from("/custom/art-dir"));
}

#[test]
fn fetch_file_uri_with_matching_digest() {
    let bytes = b"hello parrot artifact".to_vec();
    let p = temp_artifact("ok", &bytes);
    let digest = sha256_of(&bytes);
    let art = AdminArtifactRef::Wasm {
        digest: format!("sha256:{digest}"),
        uri: format!("file://{}", p.display()),
    };
    let got = ArtifactChannel.fetch(&art).expect("fetch ok");
    assert_eq!(got, p);
    std::fs::remove_file(&p).ok();
}

#[test]
fn fetch_bare_digest_without_prefix() {
    let bytes = b"bare digest form".to_vec();
    let p = temp_artifact("bare", &bytes);
    let digest = sha256_of(&bytes);
    let art = AdminArtifactRef::Dylib {
        digest, // 无 sha256: 前缀
        uri: format!("file://{}", p.display()),
        abi: 1,
    };
    assert!(ArtifactChannel.fetch(&art).is_ok());
    std::fs::remove_file(&p).ok();
}

#[test]
fn fetch_rejects_digest_mismatch() {
    let bytes = b"content v1".to_vec();
    let p = temp_artifact("bad", &bytes);
    let art = AdminArtifactRef::Wasm {
        digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
        uri: format!("file://{}", p.display()),
    };
    let err = ArtifactChannel.fetch(&art).unwrap_err();
    assert_eq!(err.0, v2_err::ARTIFACT_DIGEST);
    assert!(err.1.contains("mismatch"));
    std::fs::remove_file(&p).ok();
}

#[test]
fn fetch_rejects_missing_file() {
    let art = AdminArtifactRef::Wasm {
        digest: String::new(),
        uri: "file:///nonexistent/parrot/missing.wasm".into(),
    };
    let err = ArtifactChannel.fetch(&art).unwrap_err();
    assert_eq!(err.0, v2_err::ARTIFACT_FETCH);
    std::fs::remove_file("/nonexistent/parrot/missing.wasm").ok();
}

#[test]
fn fetch_rejects_non_file_scheme() {
    let art = AdminArtifactRef::Wasm {
        digest: String::new(),
        uri: "https://cdn.example.com/a.wasm".into(),
    };
    let err = ArtifactChannel.fetch(&art).unwrap_err();
    assert_eq!(err.0, v2_err::ARTIFACT_FETCH);
    assert!(err.1.contains("unsupported"));
}

#[test]
fn fetch_props_passthrough_and_cross_dialect_rejected() {
    // Props：零传输直通
    let art = AdminArtifactRef::Props {
        factory: "deploy.echo".into(),
    };
    assert_eq!(ArtifactChannel.fetch(&art).unwrap(), std::path::PathBuf::new());
    // Beam/PyModule/Jvm：parrot 方言不认 → DIALECT_MISMATCH
    for art in [
        AdminArtifactRef::Beam { app: "frontier".into() },
        AdminArtifactRef::PyModule {
            module: "m".into(),
            runtime_env: None,
        },
        AdminArtifactRef::Jvm {
            main_class: "M".into(),
            coords: None,
        },
    ] {
        let err = ArtifactChannel.fetch(&art).unwrap_err();
        assert_eq!(err.0, v2_err::DIALECT_MISMATCH);
    }
}

#[test]
fn verify_empty_digest_skips() {
    let p = temp_artifact("empty-digest", b"whatever");
    assert!(ArtifactChannel::verify(&p, "").is_ok());
    std::fs::remove_file(&p).ok();
}

// ══════════════════════════════════════════════════════════════════════════
// exec_deploy_v2（7）
// ══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn deploy_singleton_path_and_real_behavior() {
    let ts = engine();
    // 名字唯一化（并行测试不撞 registry）
    let name = format!("b2echo{}", line!());
    let paths = exec_deploy_v2(&ts, deploy_cmd(&name, "deploy.echo", AdminInstancePolicy::Singleton))
        .await
        .expect("deploy");
    assert_eq!(paths, vec![format!("/user/{name}")]);
    // 真实行为：NEcho(v) → NEchoed(v)（引擎层 ask）
    let r = ts.get_actor_ref(&paths[0]).expect("in registry");
    let reply = r.send(Box::new(NEcho(7)) as BoxedMessage).await.expect("ask");
    assert_eq!(reply.downcast_ref::<parrot_node::NEchoed>().unwrap().0, 7);
    // 清理
    let _ = ts.stop_actor(&paths[0]).await;
}

#[tokio::test]
async fn deploy_pool_multi_instance_paths() {
    let ts = engine();
    let name = format!("b2cnt{}", line!());
    let paths = exec_deploy_v2(
        &ts,
        deploy_cmd(&name, "deploy.counter", AdminInstancePolicy::Pool { count: 3 }),
    )
    .await
    .expect("deploy");
    assert_eq!(
        paths,
        vec![
            format!("/user/{name}-0"),
            format!("/user/{name}-1"),
            format!("/user/{name}-2"),
        ]
    );
    // 每实例独立计数：inc 一次 → total 1（非共享）
    for p in &paths {
        let r = ts.get_actor_ref(p).expect("in registry");
        let _ = r.send(Box::new(NInc(1)) as BoxedMessage).await;
    }
    assert_eq!(ask_total(&ts, &paths[1]).await, 1);
    for p in &paths {
        let _ = ts.stop_actor(p).await;
    }
}

#[tokio::test]
async fn deploy_sharded_paths_and_kv_behavior() {
    let ts = engine();
    let name = format!("b2kv{}", line!());
    let paths = exec_deploy_v2(
        &ts,
        deploy_cmd(&name, "deploy.kv", AdminInstancePolicy::Sharded { count: 2 }),
    )
    .await
    .expect("deploy");
    assert_eq!(paths.len(), 2);
    // KV 行为：put/get
    let r = ts.get_actor_ref(&paths[0]).expect("in registry");
    let _ = r
        .send(Box::new(parrot_node::NPut("k".into(), "v".into())) as BoxedMessage)
        .await;
    let reply = r
        .send(Box::new(parrot_node::NGet("k".into())) as BoxedMessage)
        .await
        .expect("ask");
    assert_eq!(
        reply.downcast_ref::<parrot_node::NGot>().unwrap().0,
        Some("v".into())
    );
    for p in &paths {
        let _ = ts.stop_actor(p).await;
    }
}

#[tokio::test]
async fn deploy_unknown_factory() {
    let ts = engine();
    let err = exec_deploy_v2(&ts, deploy_cmd("x", "deploy.nope", AdminInstancePolicy::Singleton))
        .await
        .unwrap_err();
    assert_eq!(err.0, v2_err::FACTORY_NOT_FOUND);
    assert!(err.1.contains("deploy.nope"));
}

#[tokio::test]
async fn deploy_non_props_dialect_mismatch() {
    let ts = engine();
    let cmd = ComponentDeploy {
        name: "beamc".into(),
        version: "1".into(),
        artifact: AdminArtifactRef::Beam { app: "frontier".into() },
        instances: AdminInstancePolicy::Singleton,
        config: None,
    };
    let err = exec_deploy_v2(&ts, cmd).await.unwrap_err();
    assert_eq!(err.0, v2_err::DIALECT_MISMATCH);
}

#[tokio::test]
async fn deploy_engine_mismatch_rejected() {
    // 非 NODE 单例引擎 → SPAWN_FAILED（装配一致性守卫）
    let foreign = Arc::new(ThreadActorSystem::new(Default::default()));
    let err = exec_deploy_v2(
        &foreign,
        deploy_cmd("mm", "deploy.echo", AdminInstancePolicy::Singleton),
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, v2_err::SPAWN_FAILED);
    assert!(err.1.contains("engine mismatch"));
}

#[tokio::test]
async fn deploy_duplicate_path_spawn_failed() {
    let ts = engine();
    let name = format!("b2dup{}", line!());
    let paths = exec_deploy_v2(
        &ts,
        deploy_cmd(&name, "deploy.echo", AdminInstancePolicy::Singleton),
    )
    .await
    .expect("first deploy");
    // 同名重部署 → 路径冲突
    let err = exec_deploy_v2(
        &ts,
        deploy_cmd(&name, "deploy.echo", AdminInstancePolicy::Singleton),
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, v2_err::SPAWN_FAILED);
    let _ = ts.stop_actor(&paths[0]).await;
}

// ══════════════════════════════════════════════════════════════════════════
// NodeComponentExecutor（6）
// ══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn executor_status_reports_version_and_paths() {
    let ts = engine();
    let ex = NodeComponentExecutor::new(ts.clone());
    let name = format!("b2st{}", line!());
    let r = ex
        .deploy(1, &deploy_cmd(&name, "deploy.counter", AdminInstancePolicy::Pool { count: 2 }))
        .await;
    match r {
        AdminReplyV2::Deployed { instances, req_id: _ } => assert_eq!(instances.len(), 2),
        other => panic!("deploy: {other:?}"),
    }
    let st = ex.status(2, &format!("/user/{name}")).await;
    match st {
        AdminReplyV2::Status { states, .. } => {
            assert_eq!(states.len(), 2);
            assert!(states.iter().all(|s| s.version == "1.0.0"));
            assert!(states.iter().all(|s| s.state == "running"));
        }
        other => panic!("status: {other:?}"),
    }
    // 清理
    let _ = ex.stop(3, &format!("/user/{name}")).await;
}

#[tokio::test]
async fn executor_stop_prefix_no_false_match() {
    let ts = engine();
    let ex = NodeComponentExecutor::new(ts.clone());
    // 近名对：n1 与 n1x——前缀 /user/n1 不得误吞 /user/n1x
    let n1 = format!("b2a{}", line!());
    let n1x = format!("{n1}x");
    ex.deploy(1, &deploy_cmd(&n1, "deploy.echo", AdminInstancePolicy::Singleton)).await;
    ex.deploy(2, &deploy_cmd(&n1x, "deploy.echo", AdminInstancePolicy::Singleton)).await;
    let r = ex.stop(3, &format!("/user/{n1}")).await;
    assert!(matches!(r, AdminReplyV2::Stopped { .. }));
    // n1 停了；n1x 仍在
    assert!(ts.get_actor_ref(&format!("/user/{n1}")).is_none());
    assert!(ts.get_actor_ref(&format!("/user/{n1x}")).is_some());
    let _ = ex.stop(4, &format!("/user/{n1x}")).await;
}

#[tokio::test]
async fn executor_stop_not_found() {
    let ts = engine();
    let ex = NodeComponentExecutor::new(ts.clone());
    let r = ex.stop(1, "/user/never-deployed-xyz").await;
    match r {
        AdminReplyV2::Failed { code, .. } => assert_eq!(code, v2_err::COMPONENT_NOT_FOUND),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn executor_drain_empty_mailbox_succeeds() {
    let ts = engine();
    let ex = NodeComponentExecutor::new(ts.clone());
    let name = format!("b2dr{}", line!());
    ex.deploy(1, &deploy_cmd(&name, "deploy.counter", AdminInstancePolicy::Singleton)).await;
    // 邮箱空 → drain 立即成功 + 实例移除
    let r = ex.drain(2, &format!("/user/{name}"), 3000).await;
    match r {
        AdminReplyV2::Drained { drained, aborted, .. } => {
            assert_eq!((drained, aborted), (1, 0));
        }
        other => panic!("{other:?}"),
    }
    assert!(ts.get_actor_ref(&format!("/user/{name}")).is_none());
}

#[tokio::test]
async fn executor_drain_inflight_then_settles() {
    let ts = engine();
    let ex = NodeComponentExecutor::new(ts.clone());
    let name = format!("b2slow{}", line!());
    ex.deploy(1, &deploy_cmd(&name, "deploy.slow", AdminInstancePolicy::Singleton)).await;
    // 投一条慢消息（100ms 处理）→ drain 等它处理完（在途消息不被丢）
    let r0 = ts.get_actor_ref(&format!("/user/{name}")).unwrap();
    let r = r0.clone_boxed();
    let inflight = tokio::spawn(async move {
        let _ = r
            .send(Box::new(parrot_node::NSlowEcho(120)) as BoxedMessage)
            .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await; // 消息入箱
    let rr = ex.drain(2, &format!("/user/{name}"), 3000).await;
    match rr {
        AdminReplyV2::Drained { drained, aborted, .. } => {
            assert_eq!((drained, aborted), (1, 0), "slow message settled in time");
        }
        other => panic!("{other:?}"),
    }
    inflight.await.unwrap();
    assert!(ts.get_actor_ref(&format!("/user/{name}")).is_none());
}

#[tokio::test]
async fn executor_drain_timeout_aborts() {
    let ts = engine();
    let ex = NodeComponentExecutor::new(ts.clone());
    let name = format!("b2to{}", line!());
    ex.deploy(1, &deploy_cmd(&name, "deploy.slow", AdminInstancePolicy::Singleton)).await;
    // 三条慢消息（1.2s each）：首条处理中，后两条积压邮箱 →
    // drain 预算 80ms 内邮箱不可能清空 → 超时中止（实例保留）
    let r0 = ts.get_actor_ref(&format!("/user/{name}")).unwrap();
    for _ in 0..3 {
        let r = r0.clone_boxed();
        tokio::spawn(async move {
            let _ = r
                .send(Box::new(parrot_node::NSlowEcho(1200)) as BoxedMessage)
                .await;
        });
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await; // 首条已 pop 处理中
    let rr = ex.drain(2, &format!("/user/{name}"), 80).await;
    match rr {
        AdminReplyV2::Drained { drained, aborted, .. } => {
            assert_eq!((drained, aborted), (0, 1), "backlog exceeds budget");
        }
        other => panic!("{other:?}"),
    }
    // 实例保留（发起方决定强停）
    assert!(ts.get_actor_ref(&format!("/user/{name}")).is_some());
    // 强停收尾（stop 等当前消息处理完——总时长可控）
    let _ = ex.stop(3, &format!("/user/{name}")).await;
    assert!(ts.get_actor_ref(&format!("/user/{name}")).is_none());
}

// ══════════════════════════════════════════════════════════════════════════
// exec_command_v2 直通（2）+ caps 位（2）
// ══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn command_v2_full_lifecycle() {
    let ts = engine();
    let name = format!("b2lc{}", line!());
    let d = exec_command_v2(
        &ts,
        AdminCommandV2::DeployComponent {
            req_id: 1,
            component: deploy_cmd(&name, "deploy.counter", AdminInstancePolicy::Pool { count: 2 }),
        },
    )
    .await;
    match d {
        AdminReplyV2::Deployed { req_id, instances } => {
            assert_eq!(req_id, 1);
            assert_eq!(instances.len(), 2);
        }
        other => panic!("{other:?}"),
    }
    let s = exec_command_v2(&ts, AdminCommandV2::ComponentStatus { req_id: 2, path_prefix: format!("/user/{name}") }).await;
    assert!(matches!(s, AdminReplyV2::Status { .. }));
    let st = exec_command_v2(&ts, AdminCommandV2::StopComponent { req_id: 3, path_prefix: format!("/user/{name}") }).await;
    assert!(matches!(st, AdminReplyV2::Stopped { req_id: 3, .. }));
    // 停后 status → NOT_FOUND
    let s2 = exec_command_v2(&ts, AdminCommandV2::ComponentStatus { req_id: 4, path_prefix: format!("/user/{name}") }).await;
    match s2 {
        AdminReplyV2::Failed { code, .. } => assert_eq!(code, v2_err::COMPONENT_NOT_FOUND),
        other => panic!("{other:?}"),
    }
}

#[test]
fn caps_artifacts_bit_wire_shape() {
    // 发起侧预判语义：位值冻结 + 与 codec 正交（协议事实）
    assert_eq!(parrot_remote::handshake::caps::ARTIFACTS, 1 << 5);
    assert_eq!(parrot_remote::handshake::caps::WASM, 1 << 6);
    assert_eq!(parrot_remote::handshake::caps::DYLIB, 1 << 7);
    let a = parrot_remote::handshake::caps::BIN | parrot_remote::handshake::caps::ARTIFACTS;
    let b = parrot_remote::handshake::caps::BIN;
    assert_eq!(parrot_remote::handshake::negotiate_caps(a, b).unwrap() & 1 << 5, 0);
}

#[test]
fn instance_paths_policy_expansion() {
    use parrot_node::executor_v2::instance_paths;
    assert_eq!(
        instance_paths("c", AdminInstancePolicy::Singleton),
        vec!["/user/c"]
    );
    assert_eq!(
        instance_paths("c", AdminInstancePolicy::Pool { count: 2 }),
        vec!["/user/c-0", "/user/c-1"]
    );
    assert_eq!(
        instance_paths("c", AdminInstancePolicy::Sharded { count: 3 }).len(),
        3
    );
}
