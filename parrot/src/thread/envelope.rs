use crate::thread::reply::ReplyChannel;
use parrot_api::types::BoxedMessage;
use std::fmt::Debug;
use std::fmt;

/// Wrapper for messages sent via 'ask' to carry a reply channel.
#[derive(Debug)]
pub struct AskEnvelope {
    pub payload: BoxedMessage,
    /// The channel to send the reply back on.
    pub reply: Box<dyn ReplyChannel>,
}

impl fmt::Debug for AskEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AskEnvelope")
            .field("payload", &"<boxed-message>")
            .field("reply", &"<reply-channel>")
            .finish()
    }
}

/// 内部控制消息，用于Actor生命周期管理和系统通信
#[derive(Debug)]
pub enum ControlMessage {
    /// 启动Actor
    Start,
    
    /// 停止Actor
    Stop,
    
    /// 子Actor失败通知
    ChildFailure {
        /// 失败的子Actor路径
        path: String,
        /// 失败原因
        reason: String,
    },
    
    /// 系统正在关闭通知
    SystemShutdown,
    
    /// 检查Actor健康状态
    HealthCheck,
}

// Note: BoxedMessage already implements Debug, and Box<dyn ReplyChannel> will require ReplyChannel: Debug 