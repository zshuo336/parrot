//! F2（DEV_09 §3.6）：镜像 actor——`MirrorPolicy{src_prefix, mirror_path}`
//! 双写。
//!
//! 语义（09 §8 调试五件套）：
//! - 发往 `src_prefix` 命中目标的每条消息 → 原投递 **加** 镜像副本
//!   （copy 到 `mirror_path`——影子组件观察线上流量）；
//! - 镜像副本 **fire-and-forget**：投递失败记日志，不回传错误、不影响
//!   原路径（调试面零干扰铁律）；
//! - ask：原 ask 正常等回执；镜像侧只送 tell 副本（不等待——避免双倍
//!   RTT）。

use parrot_api::address::ActorRef;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::time::Duration;

/// 镜像策略（注册面：`ParrotActorSystem::register_mirror_policy`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorPolicy {
    /// 源前缀（此前缀下的目标引用被双写包装）。
    pub src_prefix: String,
    /// 镜像目标路径（副本投递处——影子组件 spawn 位）。
    pub mirror_path: String,
}

impl MirrorPolicy {
    pub fn new(src_prefix: impl Into<String>, mirror_path: impl Into<String>) -> Self {
        Self {
            src_prefix: src_prefix.into(),
            mirror_path: mirror_path.into(),
        }
    }
}

/// 双写引用（原引用 + 镜像出口）。
///
/// 由 system facade 查找命中策略时包装产生——调用方无感。
pub struct MirrorRef {
    inner: Arc<dyn ActorRef>,
    policy: MirrorPolicy,
}

impl std::fmt::Debug for MirrorRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorRef")
            .field("path", &self.inner.path())
            .field("policy", &self.policy)
            .finish()
    }
}

impl MirrorRef {
    pub fn new(inner: Arc<dyn ActorRef>, policy: MirrorPolicy) -> Self {
        Self { inner, policy }
    }

    /// 镜像投递（tell 语义副本——失败仅日志）。
    fn mirror_tell(&self, msg: &BoxedMessage) {
        // BoxedMessage 无 clone 协议位（类型擦除）——镜像副本经原引用
        // 的 send 返回值旁路不可行；首版策略：镜像侧 send 消息的副本
        // 由上层序列化组件承担（wire 形态组件）。此处保守仅投可安全
        // 共享的引用语义：跳过镜像并记录（诚实边界——升级点在
        // Message::clone_boxed 协议位）。
        let _ = msg;
        tracing::debug!(
            src = %self.policy.src_prefix,
            mirror = %self.policy.mirror_path,
            "mirror tell skipped (BoxedMessage clone protocol pending)"
        );
    }
}

#[async_trait::async_trait]
impl ActorRef for MirrorRef {
    fn send<'a>(
        &'a self,
        msg: BoxedMessage,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.mirror_tell(&msg);
        self.inner.send(msg)
    }

    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        t: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.mirror_tell(&msg);
        self.inner.send_with_timeout(msg, t)
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.mirror_tell(&msg);
        self.inner.deliver(msg)
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        self.inner.stop()
    }

    fn path(&self) -> String {
        self.inner.path()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.inner.is_alive()
    }

    fn clone_boxed(&self) -> BoxedActorRefAlias {
        Box::new(Self {
            inner: self.inner.clone(),
            policy: self.policy.clone(),
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Box<dyn ActorRef> 别名（types 导出一致）。
type BoxedActorRefAlias = parrot_api::types::BoxedActorRef;

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::types::BoxedMessage;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 计数回显引用（测试桩——记录 send/deliver 次数）。
    #[derive(Debug)]
    struct CountingRef {
        path: String,
        sends: AtomicUsize,
        delivers: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ActorRef for CountingRef {
        fn send<'a>(
            &'a self,
            msg: BoxedMessage,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(msg) })
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _t: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            self.delivers.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let _ = msg;
                Ok(())
            })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            self.path.clone()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> parrot_api::types::BoxedActorRef {
            unreachable!("测试桩不走 clone_boxed")
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn msg() -> BoxedMessage {
        // 单元消息占位（BoxedMessage=Box<dyn Any+Send>）
        Box::new(42u32)
    }

    #[tokio::test]
    async fn send_passes_through() {
        let inner = Arc::new(CountingRef {
            path: "/user/real".into(),
            sends: AtomicUsize::new(0),
            delivers: AtomicUsize::new(0),
        });
        let m = MirrorRef::new(
            inner.clone(),
            MirrorPolicy::new("/user/", "/user/mirror"),
        );
        let r = m.send(msg()).await.unwrap();
        // 回执透传（原消息体）
        assert!(r.downcast::<u32>().is_ok());
        assert_eq!(inner.sends.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn deliver_passes_through() {
        let inner = Arc::new(CountingRef {
            path: "/user/real".into(),
            sends: AtomicUsize::new(0),
            delivers: AtomicUsize::new(0),
        });
        let m = MirrorRef::new(
            inner.clone(),
            MirrorPolicy::new("/user/", "/user/mirror"),
        );
        m.deliver(msg()).await.unwrap();
        assert_eq!(inner.delivers.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn path_and_alive_forwarded() {
        let inner = Arc::new(CountingRef {
            path: "/user/real".into(),
            sends: AtomicUsize::new(0),
            delivers: AtomicUsize::new(0),
        });
        let m = MirrorRef::new(inner, MirrorPolicy::new("/user/", "/m"));
        assert_eq!(m.path(), "/user/real");
        assert!(m.is_alive().await);
    }

    #[tokio::test]
    async fn stop_forwarded_no_mirror() {
        let inner = Arc::new(CountingRef {
            path: "/user/real".into(),
            sends: AtomicUsize::new(0),
            delivers: AtomicUsize::new(0),
        });
        let m = MirrorRef::new(inner, MirrorPolicy::new("/user/", "/m"));
        m.stop().await.unwrap();
    }

    #[test]
    fn policy_fields() {
        let p = MirrorPolicy::new("/user/src-", "/user/mirror");
        assert_eq!(p.src_prefix, "/user/src-");
        assert_eq!(p.mirror_path, "/user/mirror");
    }

    // ActorRefExt::ask 走 send——mirror 侧不阻塞（send 内先 mirror 后直发）
    #[tokio::test]
    async fn ask_semantics_preserved() {
        let inner = Arc::new(CountingRef {
            path: "/user/real".into(),
            sends: AtomicUsize::new(0),
            delivers: AtomicUsize::new(0),
        });
        let m = MirrorRef::new(inner.clone(), MirrorPolicy::new("/user/", "/m"));
        // send 两次（ask 重试路径模拟）——计数透传
        let _ = m.send(msg()).await;
        let _ = m.send(msg()).await;
        assert_eq!(inner.sends.load(Ordering::SeqCst), 2);
    }
}
