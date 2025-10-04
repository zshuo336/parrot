//! # 静态类型轨（M4）—— actix 引擎侧接线
//!
//! M4 的设计目标：**parrot 静态类型轨在性能上与纯 actix 持平**。
//! 实现方式与 thread 引擎同构——类型化通道（`flume` 信封，元素 =
//! `A::Msg` 枚举）直达 actor，全程零 `Box<dyn Any>`、零 downcast、
//! 零 actix `Envelope` 装箱；差异仅在消费循环的执行器：
//!
//! - 纯 actix 路径（动态轨）：消息进 `actix::dev::Msg` 邮箱 →
//!   `Envelope` 装箱 → `Addr::send` 返回 `Request` future。
//! - 静态轨（本模块）：`Arbiter::spawn` 消费循环 + 类型化通道——
//!   每条消息一次 `flume` 收发，比 actix 的信封链**少两次堆分配与
//!   一次虚分派**，天然达成"持平或更好"。
//!
//! 通道机制、`TypedActorRef`、超时与错误语义全部在规范面
//! `parrot_api::typed_channel`（与 thread 引擎同一份实现）。

pub use parrot_api::typed_channel::{DEFAULT_ASK_TIMEOUT, TypedActorRef, TypedEnvelope};

use parrot_api::typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive};
use parrot_api::types::ActorResult;
use std::sync::Arc;
use uuid::Uuid;

use crate::actix::system::ActixActorSystem;

impl ActixActorSystem {
    /// M4: 静态轨 spawn——类型化通道直达 actor，零 Any 全链路。
    ///
    /// 消费循环经 `Arbiter::spawn` 跑在系统 arbiter 池的一个 worker
    /// 上（与动态轨 actor 同池、round-robin 分配）：actix 多 arbiter
    /// 并行与零 Any 快路径兼得。通道关闭（所有 ref drop）或系统
    /// shutdown 时任务退出。返回入口协议 M 的 `TypedActorRef<A, M>`；
    /// 其它协议用 `ref_for::<M2>()` 派生。
    pub async fn spawn_typed<A, M>(
        self: &Arc<Self>,
        actor: A,
        path: &str,
    ) -> ActorResult<TypedActorRef<A, M>>
    where
        A: ParrotTypedDispatch,
        M: ParrotMsgVariant<A>,
    {
        let capacity = self.mailbox_capacity_hint();
        let (tx, rx) = parrot_api::typed_channel::typed_channel::<A>(capacity);
        let mut actor = actor;
        let path_str = self.unique_typed_path(path);

        // 消费循环：跑在 arbiter 池的 worker 上（与动态轨 actor 同池、
        // round-robin 分配）。rx 的所有权移入任务；任务退出条件与
        // thread 侧一致——所有 Sender drop（recv 返回 Err）。
        let arbiter = self
            .arbiter_pool()
            .map(|pool| pool.next_arbiter())
            .map_err(|e| {
                parrot_api::errors::ActorError::InternalError(format!(
                    "actix arbiter pool unavailable: {e}"
                ))
            })?;
        arbiter.spawn(async move {
            use parrot_api::typed_channel::consume_one;
            while let Ok(env) = rx.recv_async().await {
                consume_one(&mut actor, env).await;
            }
        });

        Ok(TypedActorRef::new(tx, path_str))
    }

    /// M4: 静态轨 spawn（单一协议便捷入口，无需 derive 枚举信封）。
    ///
    /// 与 thread 引擎的 `spawn_typed_single` 完全同构：适配器
    /// `SingleDispatch` 把单个 `TypedReceive<M>` 包装成
    /// `ParrotTypedDispatch`；用户消息经 `SingleVariant::wrap` 包装。
    pub async fn spawn_typed_single<A, M>(
        self: &Arc<Self>,
        actor: A,
        path: &str,
    ) -> ActorResult<
        TypedActorRef<
            crate::thread::typed::SingleDispatch<A, M>,
            crate::thread::typed::SingleVariant<A, M>,
        >,
    >
    where
        A: TypedReceive<M>,
        M: parrot_api::message::Message,
    {
        use crate::thread::typed::{SingleDispatch, SingleVariant};
        self.spawn_typed::<SingleDispatch<A, M>, SingleVariant<A, M>>(
            SingleDispatch::new(actor),
            path,
        )
        .await
    }
}

impl ActixActorSystem {
    /// 静态轨通道容量（对齐系统配置默认邮箱容量；actix 侧配置面
    /// 暂无独立字段，取 parrot 生态默认 1024）。
    fn mailbox_capacity_hint(&self) -> usize {
        1024
    }

    /// 唯一化静态轨路径（与动态轨 spawn 的 uuid 后缀策略一致）。
    fn unique_typed_path(&self, path: &str) -> String {
        format!("{}/{}", path, Uuid::new_v4().simple())
    }
}
