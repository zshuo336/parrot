//! Async Wasmtime invocation with cancellation-owned Store retirement.
use super::*;
use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::Poll,
};

/// Physical cleanup failure outlives cancellation and vetoes Store reuse.
#[derive(Clone, Debug, Default)]
pub(super) struct CleanupFailures(Arc<Mutex<Option<CleanupFailure>>>);
#[derive(Clone, Debug)]
struct CleanupFailure {
    message: String,
    panicked: bool,
}
impl CleanupFailures {
    pub(super) fn record<T>(&self, result: &wasmtime::Result<T>) {
        if let Err(error) = result {
            let message = error
                .downcast_ref::<HostPanic>()
                .map_or_else(|| format!("{error:#}"), |panic| panic.0.clone());
            self.0.lock().unwrap().get_or_insert(CleanupFailure {
                message,
                panicked: error.downcast_ref::<HostPanic>().is_some(),
            });
        }
    }

    pub(super) fn result(&self) -> wasmtime::Result<()> {
        self.0.lock().unwrap().clone().map_or(Ok(()), |failure| {
            Err(if failure.panicked {
                HostPanic(failure.message).into()
            } else {
                wasmtime::Error::msg(failure.message)
            })
        })
    }

    pub(super) fn dispose<T>(&self, value: T) -> wasmtime::Result<()> {
        let result = guest_boundary(|| {
            drop(value);
            Ok(())
        });
        self.record(&result);
        result
    }
}

struct Authorities {
    messaging: Option<Arc<dyn GuestMessaging>>,
    services: Option<Arc<dyn GuestServices>>,
}
impl Drop for Authorities {
    fn drop(&mut self) {
        let failures = CleanupFailures::default();
        let _ = failures.dispose(self.messaging.take());
        let _ = failures.dispose(self.services.take());
    }
}

/// Compiled immutable code; preparation may block and should run off an async
/// executor. Instantiation and all messaging-enabled calls use async fibers.
pub struct PreparedComponent {
    pub(super) engine: wasmtime::Engine,
    pub(super) component: Arc<wasmtime::component::Component>,
    pub(super) cfg: WasmConfig,
    pub(super) epoch_driver: Option<Arc<epoch::Driver>>,
    pub(super) started_at: std::time::Instant,
}
impl PreparedComponent {
    /// Supply authority explicitly. `None` keeps messaging disabled even when
    /// a guest imports the optional interface. Initializer cancellation drops
    /// the entire unpublished Store.
    pub fn instantiate_async(
        self,
        host: HostCtx,
        messaging: Option<Arc<dyn GuestMessaging>>,
    ) -> impl Future<Output = Result<WasmComponent, ComponentError>> + Send {
        self.instantiate_with_services(host, messaging, None)
    }

    /// Bind optional service authority to this admitted actor generation. The
    /// runtime owns all returned subscriptions; direct callers own registration
    /// retirement through the trusted provider. Existing APIs default to deny.
    pub fn instantiate_with_services(
        self,
        host: HostCtx,
        messaging: Option<Arc<dyn GuestMessaging>>,
        services: Option<Arc<dyn GuestServices>>,
    ) -> impl Future<Output = Result<WasmComponent, ComponentError>> + Send {
        let authorities = Authorities {
            messaging,
            services,
        };
        async move {
            let mut authorities = authorities;
            let mut store = create_store(&self.engine, &self.cfg, host)?;
            store.data_mut().messaging = authorities.messaging.take();
            store.data_mut().services = authorities.services.take();
            let linker = ParrotComponent::linker(&self.engine, true)
                .map_err(|e| init_error("async linker", e))?;
            let failures = store.data().cleanup.clone();
            let instance = boundary(
                linker.instantiate_async(&mut store, &self.component),
                failures,
            )
            .await
            .map_err(|e| init_error("async instantiate", e))?;
            let bindings = guest_boundary(|| ParrotComponent::bindings(&mut store, instance))
                .map_err(|e| init_error("async exports", e))?;
            Ok(WasmComponent {
                store,
                bindings,
                poisoned: false,
                started: false,
                asynchronous: true,
                _epoch_driver: self.epoch_driver,
                engine: self.engine,
                component: self.component,
                cfg: self.cfg,
                metrics: WasmMetrics {
                    instantiate_micros: self.started_at.elapsed().as_micros().min(u64::MAX as u128)
                        as u64,
                    ..Default::default()
                },
            })
        }
    }
}

async fn boundary<T>(
    future: impl Future<Output = wasmtime::Result<T>>,
    failures: CleanupFailures,
) -> wasmtime::Result<T> {
    drive(future, None, failures).await?
}

// Catch imported callbacks before they can unwind through Wasmtime's fiber.
// Owning the concrete host future separately also separates poll and Drop panic.
pub(super) async fn host_future<F: Future>(
    make: impl FnOnce() -> F,
    failures: CleanupFailures,
) -> wasmtime::Result<F::Output> {
    let future = guest_boundary(|| Ok(make()))?;
    drive(future, None, failures).await
}

// Own the suspended fiber so cancellation also disposes host imports under
// the recovery context, with no thread-local guard surviving a poll.
struct ScopedFuture<F> {
    future: Option<std::pin::Pin<Box<F>>>,
    scope: Option<recording::Scope>,
    failures: CleanupFailures,
}
impl<F> ScopedFuture<F> {
    fn retire(&mut self) -> wasmtime::Result<()> {
        let result = recording::scoped(self.scope.as_ref(), || {
            drop(self.future.take());
            Ok(())
        });
        // A broken scope may panic or omit its callback. Keep the fiber owned
        // until that unwind is caught, then dispose under a fresh boundary.
        let fallback = guest_boundary(|| {
            drop(self.future.take());
            Ok(())
        });
        let result = result.and(fallback);
        self.failures.record(&result);
        result
    }

    fn retire_scope(&mut self) -> wasmtime::Result<()> {
        let result = recording::retire_scope(self.scope.take());
        self.failures.record(&result);
        result
    }
}
impl<F> Drop for ScopedFuture<F> {
    fn drop(&mut self) {
        if self.future.is_some() {
            let _ = self.retire();
        }
        let _ = self.retire_scope();
    }
}

async fn recovery_boundary<T>(
    future: impl Future<Output = wasmtime::Result<T>>,
    scope: Option<recording::Scope>,
    failures: CleanupFailures,
) -> wasmtime::Result<T> {
    drive(future, scope, failures).await?
}

async fn drive<F: Future>(
    future: F,
    scope: Option<recording::Scope>,
    failures: CleanupFailures,
) -> wasmtime::Result<F::Output> {
    let mut owned = ScopedFuture {
        future: Some(Box::pin(future)),
        scope,
        failures: failures.clone(),
    };
    let result = std::future::poll_fn(|cx| {
        recording::scoped(owned.scope.as_ref(), || {
            Ok(owned
                .future
                .as_mut()
                .expect("live host future")
                .as_mut()
                .poll(cx))
        })
        .map(|poll| poll.map(Ok))
        .unwrap_or_else(|error| Poll::Ready(Err(error)))
    })
    .await;
    let cleanup = owned.retire();
    let context_cleanup = owned.retire_scope();
    if let Err(error) = cleanup.and(context_cleanup).and(failures.result()) {
        // In particular, an unpublished subscription returned by a completed
        // future must still be disposed if retiring that future failed.
        let _ = failures.dispose(result);
        return Err(error);
    }
    result
}

struct InvocationGuard<'a> {
    component: &'a mut WasmComponent,
    completed: bool,
}
impl Drop for InvocationGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.component.poisoned = true;
            self.component.store.data_mut().trace_parent = None;
            self.component.store.data_mut().subscriptions.clear();
            let status = recording::outcome(&self.component.store.data().cleanup.result());
            let _ = self.component.retire_recording(status);
        }
    }
}

enum Call<'a> {
    Handle(&'a str, &'a [u8]),
    Tell(&'a str, &'a [u8]),
    Start,
    Drain,
}
impl WasmComponent {
    /// Whether the component was instantiated with the optional async linker.
    pub fn uses_async(&self) -> bool {
        self.asynchronous
    }

    /// Async ASK. Recording is explicit and owned for this invocation only.
    pub fn handle_async<'a>(
        &'a mut self,
        type_key: &'a str,
        payload: &'a [u8],
        recording: Option<Box<dyn GuestTrace>>,
    ) -> impl Future<Output = Result<Vec<u8>, ComponentError>> + Send + 'a {
        self.invoke_async(
            Call::Handle(type_key, payload),
            recording.map(recording::Invocation::new),
        )
    }

    /// Async TELL; guest imports can suspend without blocking the executor.
    pub fn tell_async<'a>(
        &'a mut self,
        type_key: &'a str,
        payload: &'a [u8],
        recording: Option<Box<dyn GuestTrace>>,
    ) -> impl Future<Output = Result<(), ComponentError>> + Send + 'a {
        let invocation = self.invoke_async(
            Call::Tell(type_key, payload),
            recording.map(recording::Invocation::new),
        );
        async move { invocation.await.map(|_| ()) }
    }

    pub fn on_start_async(
        &mut self,
        recording: Option<Box<dyn GuestTrace>>,
    ) -> impl Future<Output = Result<(), ComponentError>> + Send + '_ {
        let invocation = self.invoke_async(Call::Start, recording.map(recording::Invocation::new));
        async move { invocation.await.map(|_| ()) }
    }

    pub fn on_drain_async(
        &mut self,
        recording: Option<Box<dyn GuestTrace>>,
    ) -> impl Future<Output = Result<(), ComponentError>> + Send + '_ {
        let invocation = self.invoke_async(Call::Drain, recording.map(recording::Invocation::new));
        async move { invocation.await.map(|_| ()) }
    }

    async fn invoke_async(
        &mut self,
        call: Call<'_>,
        recording: Option<recording::Invocation>,
    ) -> Result<Vec<u8>, ComponentError> {
        let parent = recording
            .as_ref()
            .map(recording::Invocation::parent)
            .transpose()
            .map_err(|e| init_error("recording parent", e))?;
        if parent.is_some_and(|parent| !parent.is_valid()) {
            return Err(ComponentError::InvalidTraceParent);
        }
        // Protect rebuild/start as well as the business call. A cancelled
        // rebuild must never make a partially initialized Store reusable.
        let mut guard = InvocationGuard {
            component: self,
            completed: false,
        };
        guard
            .component
            .prepare_async(!matches!(call, Call::Start), recording.as_ref())
            .await?;
        let component = &mut *guard.component;
        component.store.data_mut().trace_parent = parent;
        component.store.data_mut().recording = recording;
        let is_start = matches!(call, Call::Start);
        let is_message = matches!(call, Call::Handle(..) | Call::Tell(..));
        let failures = component.store.data().cleanup.clone();
        let result = match call {
            Call::Handle(key, payload) => boundary(
                component.bindings.handle_fn.call_async(
                    &mut component.store,
                    (Msg {
                        type_key: key.into(),
                        payload: payload.into(),
                    },),
                ),
                failures.clone(),
            )
            .await
            .map(|(result,)| result),
            Call::Tell(key, payload) => boundary(
                component.bindings.tell_fn.call_async(
                    &mut component.store,
                    (Msg {
                        type_key: key.into(),
                        payload: payload.into(),
                    },),
                ),
                failures.clone(),
            )
            .await
            .map(|()| Ok(Vec::new())),
            Call::Start => boundary(
                component
                    .bindings
                    .on_start_fn
                    .call_async(&mut component.store, ()),
                failures.clone(),
            )
            .await
            .map(|()| Ok(Vec::new())),
            Call::Drain => boundary(
                component
                    .bindings
                    .on_drain_fn
                    .call_async(&mut component.store, ()),
                failures,
            )
            .await
            .map(|()| Ok(Vec::new())),
        };
        let unfinished = matches!(&result, Ok(Err(_))).then_some(GuestSpanStatus::Error);
        let result = component.exit_message_scope(result, unfinished);
        let result = match result {
            Ok(result) => {
                if is_message {
                    component.metrics.messages = component.metrics.messages.saturating_add(1);
                }
                if is_start {
                    component.started = true;
                }
                result.map_err(|error| component.classify_trap(error))
            }
            Err(error) => Err(component.classify_trap_err(&error)),
        };
        guard.completed = true;
        result
    }

    async fn prepare_async(
        &mut self,
        restore_start: bool,
        recovery: Option<&recording::Invocation>,
    ) -> Result<(), ComponentError> {
        self.store
            .data()
            .cleanup
            .result()
            .map_err(|error| self.classify_trap_err(&error))?;
        self.retire_recording(GuestSpanStatus::Cancelled)
            .map_err(|error| self.classify_trap_err(&error))?;
        self.store.data_mut().trace_parent = None;
        if self.poisoned {
            let mut store = create_store(&self.engine, &self.cfg, self.store.data().host.clone())?;
            store.data_mut().next_span_handle = self.store.data().next_span_handle;
            store.data_mut().messaging = self.store.data().messaging.clone();
            store.data_mut().services = self.store.data().services.clone();
            store.data_mut().next_subscription_handle = self.store.data().next_subscription_handle;
            self.store = store;
            let linker = ParrotComponent::linker(&self.engine, true)
                .map_err(|error| init_error("async rebuild linker", error))?;
            let failures = self.store.data().cleanup.clone();
            let instance = boundary(
                linker.instantiate_async(&mut self.store, &self.component),
                failures,
            )
            .await
            .map_err(|error| init_error("async rebuild", error))?;
            self.bindings = guest_boundary(|| ParrotComponent::bindings(&mut self.store, instance))
                .map_err(|error| init_error("async rebuild exports", error))?;
            if restore_start && self.started {
                self.reset_budget()?;
                self.begin_recovery_recording(recovery)?;
                let scope = self
                    .store
                    .data()
                    .recording
                    .as_ref()
                    .map(recording::Invocation::execution_scope)
                    .transpose()
                    .map(Option::flatten);
                let result = match scope {
                    Ok(scope) => {
                        let failures = self.store.data().cleanup.clone();
                        recovery_boundary(
                            self.bindings.on_start_fn.call_async(&mut self.store, ()),
                            scope,
                            failures,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                let result = self.exit_message_scope(result, None);
                result.map_err(|error| self.classify_trap_err(&error))?;
            }
            self.poisoned = false;
        }
        self.reset_budget()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering::SeqCst},
        task::Context,
    };

    struct Fiber {
        pending: bool,
        drops: Arc<AtomicUsize>,
    }
    impl Future for Fiber {
        type Output = wasmtime::Result<usize>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            if self.pending {
                Poll::Pending
            } else {
                Poll::Ready(Ok(42))
            }
        }
    }
    impl Drop for Fiber {
        fn drop(&mut self) {
            self.drops.fetch_add(1, SeqCst);
            panic!("fiber disposal failed");
        }
    }

    #[tokio::test]
    async fn ordinary_fiber_disposal_is_contained_on_completion_and_cancellation() {
        for pending in [false, true] {
            let failures = CleanupFailures::default();
            let drops = Arc::new(AtomicUsize::new(0));
            let mut call = Box::pin(boundary(
                Fiber {
                    pending,
                    drops: drops.clone(),
                },
                failures.clone(),
            ));
            let polled = std::future::poll_fn(|cx| {
                Poll::Ready(
                    guest_boundary(|| Ok(call.as_mut().poll(cx)))
                        .expect("fiber panic escaped poll"),
                )
            })
            .await;
            if pending {
                assert!(polled.is_pending());
            } else {
                assert!(
                    matches!(polled, Poll::Ready(Err(ref error)) if error.to_string().contains("fiber disposal failed"))
                );
            }
            guest_boundary(|| {
                drop(call);
                Ok(())
            })
            .expect("fiber panic escaped cancellation");
            assert_eq!(drops.load(SeqCst), 1);
            assert_eq!(
                failures.result().unwrap_err().to_string(),
                "host panic: fiber disposal failed"
            );
        }
    }
}
