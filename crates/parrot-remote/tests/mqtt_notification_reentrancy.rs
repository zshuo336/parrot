//! Public MQTT queue cleanup must release channel locks before waking a
//! publisher. Network delivery is covered separately by the owned broker suite.
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Wake, Waker},
    time::{Duration, Instant},
};

fn isolated(name: &str, scenario: impl FnOnce()) {
    if std::env::var("PARROT_MQTT_WAKE_CHILD").as_deref() == Ok(name) {
        scenario();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("PARROT_MQTT_WAKE_CHILD", name)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "MQTT callback child failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("MQTT cleanup deadlocked in publisher wake callback");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

macro_rules! cleanup_contract {
    ($name:ident, $module:path, $qos:path) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                use $module as mqtt;
                use $qos as QoS;
                struct Publisher {
                    client: mqtt::AsyncClient,
                    calls: AtomicUsize,
                }
                impl Wake for Publisher {
                    fn wake(self: Arc<Self>) {
                        self.wake_by_ref();
                    }
                    fn wake_by_ref(self: &Arc<Self>) {
                        self.client
                            .try_publish("owned/topic", QoS::AtMostOnce, false, vec![3])
                            .unwrap();
                        self.calls.fetch_add(1, Ordering::SeqCst);
                    }
                }
                let (client, mut eventloop) = mqtt::AsyncClient::new(
                    mqtt::MqttOptions::new("owned-queue", "127.0.0.1", 1883),
                    1,
                );
                client
                    .try_publish("owned/topic", QoS::AtMostOnce, false, vec![1])
                    .unwrap();
                let observer = Arc::new(Publisher {
                    client: client.clone(),
                    calls: AtomicUsize::new(0),
                });
                let waker = Waker::from(observer.clone());
                let mut pending =
                    Box::pin(client.publish("owned/topic", QoS::AtMostOnce, false, vec![2]));
                assert!(pending
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending());
                // First drain frees capacity; the second accepts the blocked
                // publication and wakes it. That callback publishes again.
                eventloop.clean();
                assert_eq!(observer.calls.load(Ordering::SeqCst), 0);
                eventloop.clean();
                assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
                assert!(matches!(
                    pending.as_mut().poll(&mut Context::from_waker(&waker)),
                    std::task::Poll::Ready(Ok(()))
                ));
                eventloop.clean();
                let payloads: Vec<_> = eventloop
                    .pending
                    .iter()
                    .map(|request| match request {
                        mqtt::Request::Publish(publish) => publish.payload.to_vec(),
                        request => panic!("unexpected pending request: {request:?}"),
                    })
                    .collect();
                assert_eq!(payloads, vec![vec![1], vec![2], vec![3]]);
            });
        }
    };
}

cleanup_contract!(
    mqtt_v4_cleanup_allows_publisher_reentry,
    parrot_remote::mqtt::client,
    parrot_remote::mqtt::client::QoS
);
cleanup_contract!(
    mqtt_v5_cleanup_allows_publisher_reentry,
    parrot_remote::mqtt::client::v5,
    parrot_remote::mqtt::client::v5::mqttbytes::QoS
);
