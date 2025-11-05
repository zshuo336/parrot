//! Admission and ownership for transport operations and connection drivers.
use std::{future::Future, sync::Mutex};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::error::RemoteError;

#[derive(Default)]
pub(crate) struct ConnectionTasks {
    pub(crate) metrics: std::sync::Arc<crate::metrics::TransportMetrics>,
    closed: Mutex<bool>,
    tracker: TaskTracker,
    pub(super) cancelled: CancellationToken,
}

impl ConnectionTasks {
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
        tokio::select! {
            biased;
            _ = self.cancelled.cancelled() => Err(Self::error()),
            result = operation => result,
        }
    }

    pub(crate) fn spawn(
        &self,
        driver: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), RemoteError> {
        let closed = self.closed.lock().unwrap();
        if *closed {
            return Err(Self::error());
        }
        self.tracker.spawn(driver);
        Ok(())
    }

    pub(crate) fn close(&self) {
        let mut closed = self.closed.lock().unwrap();
        *closed = true;
        self.cancelled.cancel();
        self.tracker.close();
    }

    pub(crate) async fn wait(&self) {
        self.tracker.wait().await;
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.cancelled.is_cancelled()
    }

    pub(crate) fn error() -> RemoteError {
        RemoteError::Transport("transport is shut down".into())
    }
}
