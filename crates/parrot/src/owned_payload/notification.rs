//! Observer callbacks cannot unwind committed state or another owner's cleanup.
use super::{PublicationInput, panic_message};
use std::{
    future::Future,
    sync::Arc,
    task::{Context, Wake, Waker},
};
use tokio::sync::{Mutex, MutexGuard, Notify};

fn attempt(action: impl FnOnce()) -> bool {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
        let _ = panic_message(payload);
        return false;
    }
    true
}

pub(crate) fn call(action: impl FnOnce()) {
    let _ = attempt(action);
}

pub(crate) fn one(notify: &Notify) {
    if !attempt(|| notify.notify_one()) {
        // The failed observer consumed the single wake. Let independent
        // waiters inspect committed work rather than strand the ready queue.
        all(notify);
    }
}

pub(crate) fn all(notify: &Notify) {
    call(|| notify.notify_waiters());
}

struct Observer(PublicationInput<Waker>);
impl Wake for Observer {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        call(|| self.0.value().wake_by_ref());
    }
}

pub(crate) async fn observe<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut observer: Option<Arc<Observer>> = None;
    std::future::poll_fn(|cx| {
        let observer = observer
            .get_or_insert_with(|| Arc::new(Observer(PublicationInput::new(cx.waker().clone()))));
        if !observer.0.value().will_wake(cx.waker()) {
            *observer = Arc::new(Observer(PublicationInput::new(cx.waker().clone())));
        }
        let waker = Waker::from(observer.clone());
        future.as_mut().poll(&mut Context::from_waker(&waker))
    })
    .await
}

pub(crate) async fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.try_lock() {
        Ok(guard) => guard,
        Err(_) => observe(mutex.lock()).await,
    }
}
