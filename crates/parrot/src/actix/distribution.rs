//! Actor placement counts reservations until their execution future is disposed.
use actix::ArbiterHandle;
use parrot_api::runtime::LoadBalancingStrategy;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

pub(super) struct Distribution {
    strategy: LoadBalancingStrategy,
    state: Mutex<State>,
}
struct State {
    loads: Vec<usize>,
    next: usize,
    random: fastrand::Rng,
}
impl Distribution {
    pub(super) fn loads(&self) -> Vec<usize> {
        self.state.lock().unwrap().loads.clone()
    }
    pub(super) fn new(size: usize, strategy: LoadBalancingStrategy) -> Arc<Self> {
        Arc::new(Self {
            strategy,
            state: Mutex::new(State {
                loads: vec![0; size],
                next: 0,
                random: fastrand::Rng::new(),
            }),
        })
    }

    pub(super) fn reserve(self: &Arc<Self>) -> Lease {
        let mut state = self.state.lock().unwrap();
        let size = state.loads.len();
        let start = state.next;
        let index = match self.strategy {
            LoadBalancingStrategy::RoundRobin => start,
            LoadBalancingStrategy::Random => state.random.usize(..size),
            LoadBalancingStrategy::LeastLoaded => (0..size)
                .map(|offset| (start + offset) % size)
                .min_by_key(|&index| state.loads[index])
                .unwrap(),
        };
        state.loads[index] += 1;
        state.next = (index + 1) % size;
        Lease {
            distribution: self.clone(),
            index,
        }
    }
}

pub(super) struct Lease {
    distribution: Arc<Distribution>,
    pub(super) index: usize,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.distribution.state.lock().unwrap().loads[self.index] -= 1;
    }
}

// Field order releases the load even when disposing a user future panics, and
// never releases it before that future's destructor finishes.
struct Assigned<F> {
    future: Pin<Box<F>>,
    _lease: Lease,
}
impl<F: Future> Future for Assigned<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}

pub(super) struct Assignment {
    pub(super) worker: ArbiterHandle,
    pub(super) lease: Lease,
}
impl Assignment {
    pub(super) fn spawn<F>(self, future: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.worker.spawn(Assigned {
            future: Box::pin(future),
            _lease: self.lease,
        })
    }

    pub(super) fn spawn_fn<M, F>(self, make: M) -> bool
    where
        M: FnOnce() -> F + Send + 'static,
        F: Future<Output = ()> + 'static,
    {
        let lease = self.lease;
        self.worker.spawn_fn(move || {
            let future = Box::pin(make());
            actix::spawn(Assigned {
                future,
                _lease: lease,
            });
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_atomic_and_least_loaded_rotates_ties() {
        let distribution = Distribution::new(3, LoadBalancingStrategy::LeastLoaded);
        let leases: Vec<_> = (0..6).map(|_| distribution.reserve()).collect();
        assert_eq!(
            leases.iter().map(|lease| lease.index).collect::<Vec<_>>(),
            [0, 1, 2, 0, 1, 2]
        );
        let mut leases = leases;
        drop(leases.remove(1));
        let replacement = distribution.reserve();
        assert_eq!(replacement.index, 1);
        drop(replacement);
        drop(leases);
        assert_eq!(distribution.state.lock().unwrap().loads, [0, 0, 0]);
        let tasks: Vec<_> = (0..30)
            .map(|_| {
                let distribution = distribution.clone();
                std::thread::spawn(move || distribution.reserve())
            })
            .collect();
        let leases: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
        assert_eq!(distribution.state.lock().unwrap().loads, [10, 10, 10]);
        drop(leases);
        assert_eq!(distribution.state.lock().unwrap().loads, [0, 0, 0]);
    }

    #[test]
    fn random_placement_uses_rng_instead_of_circular_selection() {
        let distribution = Distribution::new(3, LoadBalancingStrategy::Random);
        distribution.state.lock().unwrap().random = fastrand::Rng::with_seed(12345);
        let mut expected = fastrand::Rng::with_seed(12345);
        let mut actual = Vec::new();
        for _ in 0..64 {
            let lease = distribution.reserve();
            assert_eq!(lease.index, expected.usize(..3));
            actual.push(lease.index);
        }
        assert!((0..3).all(|index| actual.contains(&index)));
        assert!(actual.windows(2).any(|pair| pair[0] == pair[1]));
        assert_eq!(distribution.state.lock().unwrap().loads, [0, 0, 0]);
    }

    #[test]
    fn future_disposal_precedes_load_release_even_when_it_panics() {
        struct Held(Arc<Distribution>);
        impl Drop for Held {
            fn drop(&mut self) {
                assert_eq!(self.0.state.lock().unwrap().loads, [1]);
                panic!("future disposal failed");
            }
        }
        let distribution = Distribution::new(1, LoadBalancingStrategy::LeastLoaded);
        let held = Held(distribution.clone());
        let task = Assigned {
            future: Box::pin(async move {
                let _held = held;
                std::future::pending::<()>().await;
            }),
            _lease: distribution.reserve(),
        };
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(task))).is_err());
        assert_eq!(distribution.state.lock().unwrap().loads, [0]);
    }

    #[actix::test]
    async fn rejected_and_unpolled_dispatch_release_their_reservations() {
        let worker = actix::Arbiter::new();
        let handle = worker.handle();
        let distribution = Distribution::new(1, LoadBalancingStrategy::LeastLoaded);
        let assignment = Assignment {
            worker: handle.clone(),
            lease: distribution.reserve(),
        };
        assert_eq!(distribution.loads(), [1]);
        drop(assignment);
        assert_eq!(distribution.loads(), [0]);
        handle.stop();
        tokio::task::spawn_blocking(move || worker.join().unwrap())
            .await
            .unwrap();
        let assignment = Assignment {
            worker: handle.clone(),
            lease: distribution.reserve(),
        };
        assert!(!assignment.spawn(std::future::pending()));
        assert_eq!(distribution.loads(), [0]);
        let assignment = Assignment {
            worker: handle,
            lease: distribution.reserve(),
        };
        assert!(!assignment.spawn_fn(std::future::pending));
        assert_eq!(distribution.loads(), [0]);
    }

    #[actix::test]
    async fn queued_local_execution_keeps_its_reservation_until_disposal() {
        let worker = actix::Arbiter::new();
        let handle = worker.handle();
        let (release, held) = std::sync::mpsc::channel();
        struct Release(Option<std::sync::mpsc::Sender<()>>);
        impl Drop for Release {
            fn drop(&mut self) {
                if let Some(send) = self.0.take() {
                    let _ = send.send(());
                }
            }
        }
        let release = Release(Some(release));
        let (entered, blocked) = tokio::sync::oneshot::channel();
        assert!(handle.spawn_fn(move || {
            entered.send(()).unwrap();
            held.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        }));
        blocked.await.unwrap();
        let distribution = Distribution::new(1, LoadBalancingStrategy::LeastLoaded);
        let assignment = Assignment {
            worker: handle.clone(),
            lease: distribution.reserve(),
        };
        let (started, running) = tokio::sync::oneshot::channel();
        let (finish, done) = tokio::sync::oneshot::channel();
        assert!(assignment.spawn_fn(move || {
            let local = std::rc::Rc::new(());
            async move {
                started.send(()).unwrap();
                done.await.unwrap();
                assert_eq!(std::rc::Rc::strong_count(&local), 1);
            }
        }));
        assert_eq!(
            distribution.loads(),
            [1],
            "queued closure lost its reservation"
        );
        drop(release);
        running.await.unwrap();
        assert_eq!(
            distribution.loads(),
            [1],
            "pending local future lost its reservation"
        );
        finish.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while distribution.loads() != [0] {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handle.stop();
        tokio::task::spawn_blocking(move || worker.join().unwrap())
            .await
            .unwrap();
    }
}
