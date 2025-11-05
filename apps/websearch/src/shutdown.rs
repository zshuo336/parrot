//! Application stop notification. A request remains visible to late subscribers.
use tokio::sync::watch;

#[derive(Clone)]
pub(crate) struct Stop(watch::Sender<bool>);

impl Stop {
    pub(crate) fn new() -> Self {
        Self(watch::channel(false).0)
    }

    pub(crate) fn request(&self) {
        self.0.send_replace(true);
    }

    pub(crate) fn requested(&self) -> bool {
        *self.0.borrow()
    }

    pub(crate) async fn wait(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|requested| *requested).await;
    }
}

pub(crate) struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Signals {
    pub(crate) fn install() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => {},
                _ = self.terminate.recv() => {},
            }
            Ok(())
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn stop_is_retained_without_existing_receivers() {
        let stop = Stop::new();
        stop.request();
        assert!(stop.requested());
        tokio::time::timeout(Duration::from_secs(1), stop.wait())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn all_current_and_late_waiters_observe_the_same_stop() {
        let stop = Stop::new();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let stop = stop.clone();
            tasks.spawn(async move { stop.wait().await });
        }
        tokio::task::yield_now().await;
        stop.request();
        stop.request();
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
            stop.wait().await;
        })
        .await
        .unwrap();
    }
}

/// Preserve the public startup-failure exit status after asynchronous retirement.
#[derive(Debug)]
pub(crate) struct StartupError(pub(crate) String);
impl std::fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for StartupError {}
