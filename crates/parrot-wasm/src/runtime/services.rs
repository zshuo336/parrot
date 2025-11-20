//! Optional service discovery. Subscriptions belong to one Store generation.
use super::{
    asynchronous::{host_future, CleanupFailures},
    StoreState,
};
use std::{future::Future, pin::Pin};
use wasmtime::component::{ComponentType, Lift, Lower};

const MAX_SUBSCRIPTIONS: usize = 16;

pub type ServiceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ServiceError>> + Send + 'a>>;

#[derive(ComponentType, Lift, Lower, Clone, Copy, Debug, PartialEq, Eq)]
#[component(enum)]
#[repr(u8)]
pub enum ServiceError {
    #[component(name = "disabled")]
    Disabled,
    #[component(name = "denied")]
    Denied,
    #[component(name = "invalid-request")]
    InvalidRequest,
    #[component(name = "invalid-handle")]
    InvalidHandle,
    #[component(name = "limit")]
    Limit,
    #[component(name = "timeout")]
    Timeout,
    #[component(name = "closed")]
    Closed,
    #[component(name = "failed")]
    Failed,
}

#[derive(ComponentType, Lift, Lower, Clone, Debug, PartialEq, Eq)]
#[component(record)]
pub struct ServiceEvent {
    pub registered: bool,
    pub path: String,
}

/// A trusted, cancellation-safe receiver. Timeout must cover the actual wait;
/// it must not consume an event. Drop must release the receiver and its permits.
pub trait GuestSubscription: std::fmt::Debug + Send + Sync {
    fn next(&mut self, timeout_ms: u32) -> ServiceFuture<'_, Option<ServiceEvent>>;
}

/// Trusted authority for one admitted actor generation. Implementations must
/// authorize exact keys before gateway access, publish only the bound self path,
/// honor finite timeouts, fence old-generation writes and retire registrations.
/// This authority may outlive a poisoned inner Store; it must never outlive the
/// admitted actor's authority. Arbitrary callbacks are not a realtime sandbox.
pub trait GuestServices: std::fmt::Debug + Send + Sync {
    fn register(&self, key: String, timeout_ms: u32) -> ServiceFuture<'_, ()>;
    fn deregister(&self, key: String, timeout_ms: u32) -> ServiceFuture<'_, ()>;
    fn subscribe(
        &self,
        key: String,
        timeout_ms: u32,
    ) -> ServiceFuture<'_, Box<dyn GuestSubscription>>;
}

#[derive(Debug)]
pub(super) struct Subscription {
    value: Option<Box<dyn GuestSubscription>>,
    cleanup: CleanupFailures,
}
impl Subscription {
    fn close(mut self) -> wasmtime::Result<()> {
        self.cleanup.dispose(self.value.take())
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        // One hostile receiver destructor cannot abandon sibling resources.
        let _ = self.cleanup.dispose(self.value.take());
    }
}

fn admit(state: &mut StoreState) -> Result<(), ServiceError> {
    if !state.charge_host_call() {
        return Err(ServiceError::Limit);
    }
    if state.services.is_none() {
        return Err(ServiceError::Disabled);
    }
    Ok(())
}
fn timeout(value: u32) -> Result<(), ServiceError> {
    if !(1..=30_000).contains(&value) {
        return Err(ServiceError::InvalidRequest);
    }
    Ok(())
}
fn key_request(state: &mut StoreState, key: &str, wait: u32) -> Result<(), ServiceError> {
    admit(state)?;
    timeout(wait)?;
    if key.is_empty() || key.len() > 1024 || key.chars().any(char::is_control) {
        return Err(ServiceError::InvalidRequest);
    }
    Ok(())
}

pub(super) fn link(linker: &mut wasmtime::component::Linker<StoreState>) -> wasmtime::Result<()> {
    let mut services = linker.instance("parrot:services/discovery@0.1.0")?;
    for (name, remove) in [("register", false), ("deregister", true)] {
        services.func_wrap_async(
            name,
            move |mut caller: wasmtime::StoreContextMut<'_, StoreState>,
                  (key, wait): (String, u32)| {
                Box::new(async move {
                    if let Err(error) = key_request(caller.data_mut(), &key, wait) {
                        return Ok((Err(error),));
                    }
                    let host = caller.data().services.as_ref().expect("admitted services");
                    let cleanup = caller.data().cleanup.clone();
                    let result = if remove {
                        host_future(|| host.deregister(key, wait), cleanup).await?
                    } else {
                        host_future(|| host.register(key, wait), cleanup).await?
                    };
                    Ok((result,))
                })
            },
        )?;
    }
    services.func_wrap_async(
        "subscribe",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>, (key, wait): (String, u32)| {
            Box::new(async move {
                if let Err(error) = key_request(caller.data_mut(), &key, wait) {
                    return Ok((Err(error),));
                }
                let state = caller.data_mut();
                if state.subscriptions.len() >= MAX_SUBSCRIPTIONS
                    || state.next_subscription_handle == u64::MAX
                {
                    return Ok((Err(ServiceError::Limit),));
                }
                let host = state.services.as_ref().expect("admitted services");
                let stream =
                    match host_future(|| host.subscribe(key, wait), state.cleanup.clone()).await? {
                        Ok(value) => Subscription {
                            value: Some(value),
                            cleanup: state.cleanup.clone(),
                        },
                        Err(error) => return Ok((Err(error),)),
                    };
                let handle = state.next_subscription_handle;
                state.next_subscription_handle += 1;
                state.subscriptions.insert(handle, stream);
                Ok((Ok(handle),))
            })
        },
    )?;
    services.func_wrap_async(
        "next",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>, (handle, wait): (u64, u32)| {
            Box::new(async move {
                if let Err(error) = admit(caller.data_mut()).and_then(|()| timeout(wait)) {
                    return Ok((Err(error),));
                }
                let state = caller.data_mut();
                let cleanup = state.cleanup.clone();
                let Some(stream) = state.subscriptions.get_mut(&handle) else {
                    return Ok((Err(ServiceError::InvalidHandle),));
                };
                let result = host_future(|| stream.value.as_mut().expect("live subscription").next(wait), cleanup).await?;
                if matches!(&result, Ok(None) | Err(ServiceError::Closed)) {
                    if let Some(stream) = state.subscriptions.remove(&handle) {
                        stream.close()?;
                    }
                }
                if matches!(&result, Ok(Some(event)) if event.path.len() > 4096 || event.path.chars().any(char::is_control)) {
                    if let Some(stream) = state.subscriptions.remove(&handle) {
                        stream.close()?;
                    }
                    return Ok((Err(ServiceError::Limit),));
                }
                Ok((result,))
            })
        },
    )?;
    services.func_wrap(
        "close",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>, (handle,): (u64,)| {
            if let Err(error) = admit(caller.data_mut()) {
                return Ok((Err(error),));
            }
            Ok((match caller.data_mut().subscriptions.remove(&handle) {
                Some(stream) => {
                    stream.close()?;
                    Ok(())
                }
                None => Err(ServiceError::InvalidHandle),
            },))
        },
    )?;
    Ok(())
}
