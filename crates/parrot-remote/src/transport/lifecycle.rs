//! Admission and ownership for transport operations and connection drivers.
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Wake, Waker},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::error::RemoteError;

#[cfg(test)]
mod notification_contracts;

struct NotificationWake(Option<Waker>);
impl Wake for NotificationWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        let _ = crate::ingress::unwind::Cleanup::default().call(|| {
            self.0
                .as_ref()
                .expect("live notification waker")
                .wake_by_ref()
        });
    }
}
impl Drop for NotificationWake {
    fn drop(&mut self) {
        crate::ingress::unwind::Cleanup::default().dispose(self.0.take());
    }
}

// Tokio bulk notification can discard sibling wakes after a callback panics.
// Protect each registered callback, including the operation's IO notifications.
pub(crate) async fn notify_safely<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut notification: Option<Arc<NotificationWake>> = None;
    std::future::poll_fn(|cx| {
        let notification = notification
            .get_or_insert_with(|| Arc::new(NotificationWake(Some(cx.waker().clone()))));
        if !notification
            .0
            .as_ref()
            .expect("live notification waker")
            .will_wake(cx.waker())
        {
            *notification = Arc::new(NotificationWake(Some(cx.waker().clone())));
        }
        let waker = Waker::from(notification.clone());
        future.as_mut().poll(&mut Context::from_waker(&waker))
    })
    .await
}

#[derive(Clone)]
pub(crate) struct ConnectionAdmission {
    connections: Arc<Semaphore>,
    handshakes: Arc<Semaphore>,
    valid: bool,
}

impl Default for ConnectionAdmission {
    fn default() -> Self {
        Self::new(super::RuntimeKnobs::default())
    }
}

impl ConnectionAdmission {
    pub(crate) fn new(knobs: super::RuntimeKnobs) -> Self {
        let max = Semaphore::MAX_PERMITS.min(u32::MAX as usize);
        Self {
            connections: Arc::new(Semaphore::new(knobs.connection_capacity.min(max))),
            handshakes: Arc::new(Semaphore::new(knobs.handshake_capacity.min(max))),
            valid: (1..=max).contains(&knobs.connection_capacity)
                && (1..=max).contains(&knobs.handshake_capacity),
        }
    }

    fn acquire(&self) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), RemoteError> {
        if !self.valid {
            return Err(RemoteError::Transport(
                "invalid connection admission limits".into(),
            ));
        }
        let connection = self
            .connections
            .clone()
            .try_acquire_owned()
            .map_err(|_| RemoteError::Transport("connection capacity exhausted".into()))?;
        let handshake = self
            .handshakes
            .clone()
            .try_acquire_owned()
            .map_err(|_| RemoteError::Transport("handshake capacity exhausted".into()))?;
        Ok((connection, handshake))
    }
}

/// Release capacity only after the actual IO is destroyed, even when external
/// senders or close observers outlive the connection driver.
pub(crate) struct AdmittedIo<S> {
    io: S,
    _connection: OwnedSemaphorePermit,
}

impl<S> AdmittedIo<S> {
    pub(crate) fn new(io: S, connection: OwnedSemaphorePermit) -> Self {
        Self {
            io,
            _connection: connection,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for AdmittedIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for AdmittedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[derive(Default)]
pub(crate) struct ConnectionTasks {
    pub(crate) metrics: std::sync::Arc<crate::metrics::TransportMetrics>,
    closed: Mutex<bool>,
    tracker: TaskTracker,
    pub(super) cancelled: CancellationToken,
    pub(super) admission: ConnectionAdmission,
}

impl ConnectionTasks {
    pub(crate) fn with_knobs(knobs: Option<super::RuntimeKnobs>) -> Self {
        Self {
            admission: ConnectionAdmission::new(knobs.unwrap_or_default()),
            ..Default::default()
        }
    }

    pub(crate) fn with_admission(mut self, admission: ConnectionAdmission) -> Self {
        self.admission = admission;
        self
    }

    pub(crate) fn admit(
        &self,
    ) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), RemoteError> {
        if self.is_closed() {
            return Err(Self::error());
        }
        self.admission.acquire()
    }
    pub(crate) fn with_metrics(metrics: std::sync::Arc<crate::metrics::TransportMetrics>) -> Self {
        Self {
            metrics,
            ..Default::default()
        }
    }
    pub(crate) async fn run<T>(
        &self,
        operation: impl Future<Output = Result<T, RemoteError>>,
    ) -> Result<T, RemoteError> {
        let _token = {
            let closed = self.closed.lock().unwrap();
            if *closed {
                return Err(Self::error());
            }
            self.tracker.token()
        };
        notify_safely(async {
            tokio::select! {
                biased;
                _ = self.cancelled.cancelled() => Err(Self::error()),
                result = operation => result,
            }
        })
        .await
    }

    pub(crate) fn spawn(
        &self,
        driver: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), RemoteError> {
        let closed = self.closed.lock().unwrap();
        if *closed {
            return Err(Self::error());
        }
        self.tracker.spawn(notify_safely(driver));
        Ok(())
    }

    pub(crate) fn close_admission(&self) {
        *self.closed.lock().unwrap() = true;
    }

    pub(crate) fn close(&self) {
        self.close_admission();
        let cleanup = crate::ingress::unwind::Cleanup::default();
        let _ = cleanup.call(|| self.tracker.close());
        let _ = cleanup.call(|| self.cancelled.cancel());
    }

    pub(crate) async fn wait(&self) {
        notify_safely(self.tracker.wait()).await;
    }

    pub(crate) fn is_closed(&self) -> bool {
        *self.closed.lock().unwrap()
    }

    pub(crate) fn error() -> RemoteError {
        RemoteError::Transport("transport is shut down".into())
    }
}
