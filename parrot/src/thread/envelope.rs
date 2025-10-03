use parrot_api::types::BoxedMessage;
use std::fmt;
use std::fmt::Debug;
use tokio::sync::oneshot;

use parrot_api::errors::ActorError;
use parrot_api::types::ActorResult;

/// M5 SSO 档：≤16B 小消息的 inline 载荷（安全枚举，零裸指针）。
///
/// 显式变体（非字节转写）：类型安全、Debug/PartialEq 免费获得。
/// 覆盖引擎与用户最高频的小消息形态（计数器、开关、时间戳、回执）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InlinePayload {
    /// u64（计数器/ID/时间戳）
    U64(u64),
    /// i64（带符号计数/偏移）
    I64(i64),
    /// u32（小 ID/序列号）
    U32(u32),
    /// usize（长度/索引）
    USize(usize),
    /// bool（开关/确认位）
    Bool(bool),
    /// ()（纯信号）
    Unit,
}

impl InlinePayload {
    /// 还原为 BoxedMessage（消费侧 receive_message 边界的唯一装箱点）。
    pub fn into_boxed(self) -> BoxedMessage {
        match self {
            InlinePayload::U64(v) => Box::new(v),
            InlinePayload::I64(v) => Box::new(v),
            InlinePayload::U32(v) => Box::new(v),
            InlinePayload::USize(v) => Box::new(v),
            InlinePayload::Bool(v) => Box::new(v),
            InlinePayload::Unit => Box::new(()),
        }
    }
}

/// 可 inline 的小消息 trait：用户消息类型显式声明 inline 能力。
///
/// `ThreadActorRef::ask_inline` 消费本 trait；避免特化（specialization
/// 未稳定），inline 与装箱路径经不同入口显式选择。
pub trait InlineMsg: Send + 'static {
    fn into_inline(self) -> InlinePayload;
}

impl InlineMsg for u64 {
    fn into_inline(self) -> InlinePayload {
        InlinePayload::U64(self)
    }
}
impl InlineMsg for i64 {
    fn into_inline(self) -> InlinePayload {
        InlinePayload::I64(self)
    }
}
impl InlineMsg for u32 {
    fn into_inline(self) -> InlinePayload {
        InlinePayload::U32(self)
    }
}
impl InlineMsg for usize {
    fn into_inline(self) -> InlinePayload {
        InlinePayload::USize(self)
    }
}
impl InlineMsg for bool {
    fn into_inline(self) -> InlinePayload {
        InlinePayload::Bool(self)
    }
}
impl InlineMsg for () {
    fn into_inline(self) -> InlinePayload {
        InlinePayload::Unit
    }
}

/// M5 分层载荷：≤16B inline / >16B 独立 Box（按值随信封流动）。
#[derive(Debug)]
pub enum AskPayload {
    /// SSO inline：零独立堆分配。
    Inline(InlinePayload),
    /// >16B 消息：payload Box 是该消息唯一的一次分配
    /// > （信封本身经 `push_ask` 按值进邮箱，不再装箱）。
    Boxed(BoxedMessage),
}

impl AskPayload {
    /// 还原为 BoxedMessage（inline 变体在此装箱——receive_message
    /// 边界的既定成本；Boxed 变体零成本移动）。
    pub fn into_boxed(self) -> BoxedMessage {
        match self {
            AskPayload::Inline(p) => p.into_boxed(),
            AskPayload::Boxed(b) => b,
        }
    }
}

/// Envelope for ask operations, containing the message payload and reply channel.
///
/// # Allocation profile (M5, supersedes ADR-13)
///
/// | 载荷 | asker 侧分配 | 说明 |
/// |------|------------|------|
/// | ≤16B（`new_inline`） | 1（oneshot cell） | 载荷 inline，信封按值流动零分配 |
/// | >16B（`new_typed`） | 2（payload Box + oneshot cell） | payload Box 是唯一消息分配 |
/// | 已装箱（`with_boxed`） | 1（oneshot cell） | 调用方已付 payload Box |
///
/// 信封**不再自我装箱**：经 [`crate::thread::mailbox::Mailbox::push_ask`]
/// 按值进入邮箱内部缓冲（flume bounded 预分配环形区，按值入队零额外
/// 堆分配）。`Mailbox::pop` 的兼容路径会把信封装箱还原
/// （`MailboxItem::into_boxed_message`），仅消费侧处理器
/// （`pop_item`）享受免装箱收益。
///
/// # Legacy escape hatch
///
/// `parrot_envelope_legacy` feature 开启时，ask 入口走历史双装箱路径
/// （payload Box + envelope Box）；供 M5 灰度期回退。
pub struct AskEnvelope {
    /// The actual message being sent
    pub payload: AskPayload,
    /// Channel to send the reply back to the requester.
    ///
    /// `Option` 使 `take_reply` 可以零成本取走 sender（M5 单块消费路径），
    /// 其余时间 Some。
    reply: Option<oneshot::Sender<ActorResult<BoxedMessage>>>,
}

impl fmt::Debug for AskEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("AskEnvelope")
            .field("payload", &self.payload)
            .field("reply", &"<oneshot>")
            .finish()
    }
}

impl AskEnvelope {
    /// Create an ask envelope from an **unboxed** small message (SSO inline).
    ///
    /// Zero payload allocations: the value lives inline in the envelope,
    /// and the envelope flows by value into the mailbox.
    pub fn new_inline<P: InlineMsg>(
        msg: P,
    ) -> (Self, oneshot::Receiver<ActorResult<BoxedMessage>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                payload: AskPayload::Inline(msg.into_inline()),
                reply: Some(tx),
            },
            rx,
        )
    }

    /// Create an ask envelope from an **unboxed** message of any size
    /// (zero-redundancy constructor: the only allocation is the payload
    /// box itself; the envelope is not boxed).
    pub fn new_typed<M: Send + 'static>(
        msg: M,
    ) -> (Self, oneshot::Receiver<ActorResult<BoxedMessage>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                payload: AskPayload::Boxed(Box::new(msg)),
                reply: Some(tx),
            },
            rx,
        )
    }

    /// Create an ask envelope from an already-boxed message.
    pub fn with_boxed(
        payload: BoxedMessage,
    ) -> (Self, oneshot::Receiver<ActorResult<BoxedMessage>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                payload: AskPayload::Boxed(payload),
                reply: Some(tx),
            },
            rx,
        )
    }

    /// Compatibility constructor accepting an already-boxed payload.
    pub fn new(payload: BoxedMessage) -> (Self, oneshot::Receiver<ActorResult<BoxedMessage>>) {
        Self::with_boxed(payload)
    }

    /// send success reply
    pub async fn reply_success(self, response: BoxedMessage) {
        if let Some(reply) = self.reply {
            let _ = reply.send(Ok(response));
        }
    }

    /// send error reply
    pub async fn reply_error(self, error: ActorError) {
        if let Some(reply) = self.reply {
            let _ = reply.send(Err(error));
        }
    }

    /// Synchronous reply for timeout/shutdown paths where a dropped receiver
    /// is expected and not an error.
    pub fn reply_sync(self, result: ActorResult<BoxedMessage>) {
        if let Some(reply) = self.reply {
            let _ = reply.send(result);
        }
    }

    /// Deconstruct into (payload, reply sender) — used by the actor-side
    /// handler which consumes both parts.
    pub fn into_parts(self) -> (BoxedMessage, oneshot::Sender<ActorResult<BoxedMessage>>) {
        (
            self.payload.into_boxed(),
            self.reply.expect("reply sender already taken"),
        )
    }

    /// M5: deconstruct keeping the layered payload (no inline re-box yet).
    pub fn into_parts_layered(self) -> (AskPayload, oneshot::Sender<ActorResult<BoxedMessage>>) {
        (
            self.payload,
            self.reply.expect("reply sender already taken"),
        )
    }

    /// M5: split off the reply sender, keeping the envelope payload movable
    /// (used by the single-block consumer path). Returns `None` when the
    /// sender was already taken.
    pub fn take_reply(&mut self) -> Option<oneshot::Sender<ActorResult<BoxedMessage>>> {
        self.reply.take()
    }
}

/// Control messages used internally within the actor system.
#[derive(Debug, Clone)]
pub enum ControlMessage {
    /// Start processing messages (transition from Starting to Running state)
    Start,
    /// Stop the actor (graceful shutdown)
    Stop,
    /// Notify of a child actor failure
    ChildFailure {
        /// Path of the failed child actor
        path: String,
        /// Reason for failure
        reason: String,
    },
    /// System is shutting down, stop gracefully
    SystemShutdown,
    /// Check if actor is healthy (for supervision)
    HealthCheck,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ask_envelope_reply_success() {
        let (envelope, rx) = AskEnvelope::new(Box::new("ping") as BoxedMessage);

        // Responder side: send a successful reply.
        let responder = tokio::spawn(async move {
            envelope
                .reply_success(Box::new("pong") as BoxedMessage)
                .await;
        });
        responder.await.unwrap();

        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("reply must arrive")
            .expect("channel must be open");
        let payload = reply.expect("reply is Ok");
        let pong = payload.downcast::<&str>().expect("payload is &str");
        assert_eq!(*pong, "pong");
    }

    #[tokio::test]
    async fn test_ask_envelope_reply_error() {
        let (envelope, rx) = AskEnvelope::new(Box::new("ping") as BoxedMessage);

        let responder = tokio::spawn(async move {
            envelope
                .reply_error(ActorError::MessageHandlingError("boom".into()))
                .await;
        });
        responder.await.unwrap();

        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("reply must arrive")
            .expect("channel must be open");
        match reply {
            Err(ActorError::MessageHandlingError(msg)) => assert!(msg.contains("boom")),
            other => panic!("expected MessageHandlingError, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn test_dropped_receiver_is_silently_ignored() {
        // When the asker drops the receiver (timeout / cancellation), the
        // inline oneshot sender must not panic; send failure is expected and
        // silently ignored (ADR-13: dropped receiver is not an actor error).
        let (envelope, rx) = AskEnvelope::new(Box::new("ping") as BoxedMessage);
        drop(rx);

        envelope.reply_sync(Ok(Box::new(())));
        // Reaching here without panic is the assertion.
    }

    #[test]
    fn test_control_message_variants_are_cloneable() {
        let msgs = vec![
            ControlMessage::Start,
            ControlMessage::Stop,
            ControlMessage::ChildFailure {
                path: "/user/child".into(),
                reason: "panic".into(),
            },
            ControlMessage::SystemShutdown,
            ControlMessage::HealthCheck,
        ];
        for msg in &msgs {
            let cloned = msg.clone();
            // Debug formatting must work for all variants (used in logs).
            let repr = format!("{:?}", cloned);
            assert!(!repr.is_empty());
        }
    }

    #[test]
    fn test_envelope_debug_formatting() {
        let (envelope, _rx) = AskEnvelope::new(Box::new(42u64) as BoxedMessage);
        let repr = format!("{:?}", envelope);
        assert!(repr.contains("AskEnvelope"));
        assert!(repr.contains("payload"));
        assert!(repr.contains("reply"));
    }

    // ============ M5 分层载荷 ============

    #[test]
    fn test_inline_payload_roundtrip() {
        for p in [
            InlinePayload::U64(42),
            InlinePayload::I64(-7),
            InlinePayload::U32(9),
            InlinePayload::USize(usize::MAX),
            InlinePayload::Bool(true),
            InlinePayload::Unit,
        ] {
            let boxed = p.into_boxed();
            let back = match p {
                InlinePayload::U64(v) => *boxed.downcast::<u64>().unwrap() == v,
                InlinePayload::I64(v) => *boxed.downcast::<i64>().unwrap() == v,
                InlinePayload::U32(v) => *boxed.downcast::<u32>().unwrap() == v,
                InlinePayload::USize(v) => *boxed.downcast::<usize>().unwrap() == v,
                InlinePayload::Bool(v) => *boxed.downcast::<bool>().unwrap() == v,
                InlinePayload::Unit => boxed.downcast::<()>().is_ok(),
            };
            assert!(back, "inline roundtrip failed for {:?}", p);
        }
    }

    #[test]
    fn test_new_inline_envelope_holds_inline_payload() {
        let (env, _rx) = AskEnvelope::new_inline(7u64);
        assert!(matches!(
            env.payload,
            AskPayload::Inline(InlinePayload::U64(7))
        ));
        let (payload, _reply) = env.into_parts_layered();
        let boxed = payload.into_boxed();
        assert_eq!(*boxed.downcast::<u64>().unwrap(), 7);
    }

    #[test]
    fn test_new_typed_envelope_holds_boxed_payload() {
        let (env, _rx) = AskEnvelope::new_typed("hello".to_string());
        assert!(matches!(env.payload, AskPayload::Boxed(_)));
        let boxed = env.into_parts().0;
        assert_eq!(*boxed.downcast::<String>().unwrap(), "hello");
    }
}
