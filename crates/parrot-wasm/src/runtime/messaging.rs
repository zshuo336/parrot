//! Optional asynchronous, invocation-bounded guest messaging.
use super::{asynchronous::host_future, Msg, StoreState};
use std::{future::Future, pin::Pin};
use wasmtime::component::{ComponentType, Lift, Lower};

pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(ComponentType, Lift, Lower, Clone, Copy, Debug, PartialEq, Eq)]
#[component(enum)]
#[repr(u8)]
pub enum MessagingError {
    #[component(name = "disabled")]
    Disabled,
    #[component(name = "denied")]
    Denied,
    #[component(name = "invalid-request")]
    InvalidRequest,
    #[component(name = "limit")]
    Limit,
    #[component(name = "timeout")]
    Timeout,
    #[component(name = "unknown-target")]
    UnknownTarget,
    #[component(name = "unknown-type")]
    UnknownType,
    #[component(name = "codec")]
    Codec,
    #[component(name = "failed")]
    Failed,
    #[component(name = "unexpected-reply")]
    UnexpectedReply,
}

#[derive(Debug)]
pub struct MessagingRequest {
    pub destination: String,
    pub message: Msg,
    pub timeout_ms: u32,
}

/// Trusted host authority bound to one component, never supplied by the guest.
/// Implementations must authorize before decoding/resolving, enforce the finite
/// timeout over admission and response, and release owned waits on cancellation.
/// Codec and callback execution is trusted host code, not a hard realtime sandbox.
pub trait GuestMessaging: std::fmt::Debug + Send + Sync {
    fn ask(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Msg, MessagingError>> + Send + '_>>;
    fn tell(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessagingError>> + Send + '_>>;
}

fn admit(state: &mut StoreState, request: &MessagingRequest) -> Result<(), MessagingError> {
    // Charge attempted calls too; denial and malformed requests cannot evade
    // the per-invocation host work budget.
    if !state.charge_host_call() {
        return Err(MessagingError::Limit);
    }
    if state.messaging.is_none() {
        return Err(MessagingError::Disabled);
    }
    if request.destination.is_empty()
        || request.destination.len() > 4096
        || request.destination.chars().any(char::is_control)
        || request.message.type_key.is_empty()
        || request.message.type_key.len() > 1024
        || request.message.type_key.chars().any(char::is_control)
        || request.timeout_ms == 0
        || request.timeout_ms > 30_000
    {
        return Err(MessagingError::InvalidRequest);
    }
    if request.message.payload.len() > MAX_MESSAGE_BYTES {
        return Err(MessagingError::Limit);
    }
    if request.destination == state.host.self_path {
        return Err(MessagingError::Denied);
    }
    Ok(())
}

pub(super) fn link(linker: &mut wasmtime::component::Linker<StoreState>) -> wasmtime::Result<()> {
    let mut messaging = linker.instance("parrot:messaging/messages@0.1.0")?;
    messaging.func_wrap_async(
        "ask",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>,
         (destination, message, timeout_ms): (String, Msg, u32)| {
            Box::new(async move {
                let request = MessagingRequest {
                    destination,
                    message,
                    timeout_ms,
                };
                if let Err(error) = admit(caller.data_mut(), &request) {
                    return Ok((Err(error),));
                }
                let host = caller
                    .data()
                    .messaging
                    .as_ref()
                    .expect("admitted messaging");
                let result = host_future(|| host.ask(request), caller.data().cleanup.clone())
                    .await?
                    .and_then(|reply| {
                        if reply.payload.len() > MAX_MESSAGE_BYTES || reply.type_key.len() > 1024 {
                            Err(MessagingError::Limit)
                        } else {
                            Ok(reply)
                        }
                    });
                Ok((result,))
            })
        },
    )?;
    messaging.func_wrap_async(
        "tell",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>,
         (destination, message, timeout_ms): (String, Msg, u32)| {
            Box::new(async move {
                let request = MessagingRequest {
                    destination,
                    message,
                    timeout_ms,
                };
                if let Err(error) = admit(caller.data_mut(), &request) {
                    return Ok((Err(error),));
                }
                let host = caller
                    .data()
                    .messaging
                    .as_ref()
                    .expect("admitted messaging");
                Ok((host_future(|| host.tell(request), caller.data().cleanup.clone()).await?,))
            })
        },
    )?;
    Ok(())
}
