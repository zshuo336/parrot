use parrot_remote::livekit::{
    InMemSignal, LiveKitSignal, ParrotLiveKitBridge, RoomState, SignalEvent, TrackKind,
};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

type Observation = (String, String, Vec<u8>, RoomState);

#[derive(Default)]
struct ReentrantSignal {
    inner: InMemSignal,
    bridge: OnceLock<Weak<ParrotLiveKitBridge<Self>>>,
    observations: Mutex<Vec<Observation>>,
}

impl LiveKitSignal for ReentrantSignal {
    fn poll_events(&self, room: &str) -> Vec<SignalEvent> {
        self.inner.poll_events(room)
    }

    fn send_downlink(&self, room: &str, to: &str, payload: &[u8]) {
        let bridge = self.bridge.get().unwrap().upgrade().unwrap();
        let room = room.to_owned();
        let observed_room = room.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let observer = std::thread::spawn(move || {
            let state = bridge.room_state(&observed_room).unwrap();
            tx.send(state).unwrap();
        });
        let state = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("downlink callback could not reenter room_state while pump held the room lock");
        observer.join().unwrap();
        self.observations
            .lock()
            .unwrap()
            .push((room, to.into(), payload.into(), state));
    }
}

#[test]
fn pump_downlinks_allow_room_state_reentry_and_keep_receipt_order() {
    let signal = Arc::new(ReentrantSignal::default());
    let bridge = Arc::new(ParrotLiveKitBridge::new(signal.clone()));
    signal.bridge.set(Arc::downgrade(&bridge)).ok().unwrap();
    bridge.open_room("room");
    for event in [
        SignalEvent::ParticipantJoin {
            identity: "publisher".into(),
            can_subscribe: true,
            can_publish: true,
        },
        SignalEvent::ParticipantJoin {
            identity: "viewer".into(),
            can_subscribe: true,
            can_publish: false,
        },
        SignalEvent::TrackPublished {
            publisher: "publisher".into(),
            sid: "track".into(),
            kind: TrackKind::Video,
        },
        SignalEvent::Subscribe {
            subscriber: "viewer".into(),
            track_sid: "track".into(),
        },
        SignalEvent::ParticipantLeave {
            identity: "publisher".into(),
        },
        SignalEvent::Subscribe {
            subscriber: "viewer".into(),
            track_sid: "track".into(),
        },
    ] {
        signal.inner.inject("room", event);
    }
    assert_eq!(bridge.pump("room"), 6);
    let observations = signal.observations.lock().unwrap();
    let expected = [
        ("publisher", b"joined".as_slice()),
        ("viewer", b"joined".as_slice()),
        ("publisher", b"media/track/room/track".as_slice()),
        ("viewer", b"subscribed".as_slice()),
        ("publisher", b"left".as_slice()),
        ("viewer", b"denied:NotFound(\"track\")".as_slice()),
    ];
    assert_eq!(observations.len(), expected.len());
    for ((room, to, payload, state), (expected_to, expected_payload)) in
        observations.iter().zip(expected)
    {
        assert_eq!(room, "room");
        assert_eq!(to, expected_to);
        assert_eq!(payload, expected_payload);
        assert_eq!(*state, RoomState::Open);
    }
    assert_eq!(
        bridge.with_room("room", |room| room.participants.len()),
        Some(1)
    );
    assert_eq!(bridge.with_room("room", |room| room.tracks.len()), Some(0));
}
