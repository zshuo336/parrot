//! LiveKit room metadata, subscription policy and an injected signal event pump.
//!
//! This module creates no engine actors, receptionist registrations, Wire or
//! LiveKit network connections. Hosts supply a trusted LiveKitSignal adapter;
//! InMemSignal is only a local fixture. The actor supervision tree, real server
//! adapter and media-plane integration described in DEV_05 / 07 remain separate
//! implementation work. Track keys and downsample FPS are metadata, not evidence
//! of actual registration or video processing.

use std::collections::HashMap;
use std::time::Instant;

// ── 领域模型 ─────────────────────────────────────────────

/// Last processed room-model state, not an engine supervision receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomState {
    /// The model accepts joins and publications.
    Open,
    /// Transient state while synchronous close clears metadata.
    Closing,
    Closed,
}

/// 参与者（信令面实体——媒体面不建模，端口直通）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub identity: String,
    /// 订阅权限位（Policy actor 判定输入）。
    pub can_subscribe: bool,
    pub can_publish: bool,
    pub joined_at_ms: u64,
}

/// 轨道（媒体元数据——Receptionist key：`media/track/{room}/{track}`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub sid: String,
    pub kind: TrackKind,
    pub publisher: String,
    /// 降采样断言源（AI 分支——≤10fps 硬门槛）。
    pub downstream_fps: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Video,
}

impl Track {
    pub fn receptionist_key(room: &str, sid: &str) -> String {
        format!("media/track/{room}/{sid}")
    }
}

// ── 房间监督树 ───────────────────────────────────────────

/// One room's metadata state machine; it owns no engine actors or media tasks.
pub struct RoomSupervisor {
    pub room: String,
    pub state: RoomState,
    pub participants: HashMap<String, Participant>,
    pub tracks: HashMap<String, Track>,
    opened_at: Instant,
}

impl RoomSupervisor {
    pub fn new(room: impl Into<String>) -> Self {
        Self {
            room: room.into(),
            state: RoomState::Open,
            participants: HashMap::new(),
            tracks: HashMap::new(),
            opened_at: Instant::now(),
        }
    }

    /// Insert or replace participant metadata from a trusted signal adapter.
    pub fn join(
        &mut self,
        identity: &str,
        can_subscribe: bool,
        can_publish: bool,
    ) -> Result<(), BridgeError> {
        if self.state != RoomState::Open {
            return Err(BridgeError::RoomClosed(self.room.clone()));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.participants.insert(
            identity.into(),
            Participant {
                identity: identity.into(),
                can_subscribe,
                can_publish,
                joined_at_ms: now,
            },
        );
        Ok(())
    }

    /// Remove the participant and its track metadata.
    pub fn leave(&mut self, identity: &str) -> Result<(), BridgeError> {
        self.participants
            .remove(identity)
            .ok_or_else(|| BridgeError::NotFound(identity.into()))?;
        self.tracks.retain(|_, t| t.publisher != identity);
        Ok(())
    }

    /// Store track metadata and return its suggested receptionist key.
    pub fn publish_track(
        &mut self,
        publisher: &str,
        sid: &str,
        kind: TrackKind,
    ) -> Result<String, BridgeError> {
        if self.state != RoomState::Open {
            return Err(BridgeError::RoomClosed(self.room.clone()));
        }
        if !self
            .participants
            .get(publisher)
            .map(|p| p.can_publish)
            .unwrap_or(false)
        {
            return Err(BridgeError::Forbidden(format!(
                "{publisher} cannot publish"
            )));
        }
        self.tracks.insert(
            sid.into(),
            Track {
                sid: sid.into(),
                kind,
                publisher: publisher.into(),
                downstream_fps: None,
            },
        );
        Ok(Track::receptionist_key(&self.room, sid))
    }

    /// 订阅校验（Policy actor 语义——F8 ACL 联动入口）。
    pub fn can_subscribe(&self, subscriber: &str, track_sid: &str) -> Result<(), BridgeError> {
        let p = self
            .participants
            .get(subscriber)
            .ok_or_else(|| BridgeError::NotFound(subscriber.into()))?;
        if !p.can_subscribe {
            return Err(BridgeError::Forbidden(subscriber.into()));
        }
        if !self.tracks.contains_key(track_sid) {
            return Err(BridgeError::NotFound(track_sid.into()));
        }
        Ok(())
    }

    /// Clear metadata synchronously; host actors and network work are not drained.
    pub fn close(&mut self) {
        self.state = RoomState::Closing;
        self.participants.clear();
        self.tracks.clear();
        self.state = RoomState::Closed;
    }

    /// 旁挂 AI worker 的降采样登记（≤10fps 硬门槛断言源）。
    pub fn ai_downsample(&mut self, track_sid: &str, fps: u32) -> Result<(), BridgeError> {
        let t = self
            .tracks
            .get_mut(track_sid)
            .ok_or_else(|| BridgeError::NotFound(track_sid.into()))?;
        if fps > 10 {
            return Err(BridgeError::Policy(format!(
                "downsample fps {fps} > 10 hard gate"
            )));
        }
        t.downstream_fps = Some(fps);
        Ok(())
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.opened_at.elapsed()
    }
}

/// 桥错误（REPLY_ERR 映射：Forbidden→13 / NotFound→1 / RoomClosed→7 族）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    Forbidden(String),
    NotFound(String),
    RoomClosed(String),
    Policy(String),
}

// ── 信令面（可注入——真实 livekit / 内存桩） ─────────────

/// LiveKit 信令事件（WebSocket/JSON-RPC 语义的 Rust 投影）。
#[derive(Debug, Clone, PartialEq)]
pub enum SignalEvent {
    ParticipantJoin {
        identity: String,
        can_subscribe: bool,
        can_publish: bool,
    },
    ParticipantLeave {
        identity: String,
    },
    TrackPublished {
        publisher: String,
        sid: String,
        kind: TrackKind,
    },
    /// 订阅请求（Policy 判定 + 订阅确认）。
    Subscribe {
        subscriber: String,
        track_sid: String,
    },
}

/// Trusted host injection interface. No real server adapter is supplied here.
pub trait LiveKitSignal: Send + Sync {
    /// 拉取一批信令事件（非阻塞——空 Vec=无）。
    fn poll_events(&self, room: &str) -> Vec<SignalEvent>;
    /// Publish a model acknowledgement through the host adapter.
    fn send_downlink(&self, room: &str, to: &str, payload: &[u8]);
}

/// 内存信令桩（单进程测试/演示——不经网络）。
#[derive(Default, Clone)]
pub struct InMemSignal {
    events: std::sync::Arc<std::sync::Mutex<Vec<(String, SignalEvent)>>>,
    downlinks: std::sync::Arc<std::sync::Mutex<Vec<Downlink>>>,
}

/// 下行记录（room, to, payload）。
pub type Downlink = (String, String, Vec<u8>);

impl InMemSignal {
    pub fn inject(&self, room: &str, ev: SignalEvent) {
        self.events.lock().unwrap().push((room.into(), ev));
    }

    pub fn downlinks(&self) -> Vec<Downlink> {
        self.downlinks.lock().unwrap().clone()
    }
}

impl LiveKitSignal for InMemSignal {
    fn poll_events(&self, room: &str) -> Vec<SignalEvent> {
        let mut all = self.events.lock().unwrap();
        let (mine, rest): (Vec<_>, Vec<_>) = all.drain(..).partition(|(r, _)| r == room);
        *all = rest;
        mine.into_iter().map(|(_, e)| e).collect()
    }

    fn send_downlink(&self, room: &str, to: &str, payload: &[u8]) {
        self.downlinks
            .lock()
            .unwrap()
            .push((room.into(), to.into(), payload.to_vec()));
    }
}

/// 桥（多房间——每房间一个 RoomSupervisor + 共享信令面）。
pub struct ParrotLiveKitBridge<S: LiveKitSignal> {
    pub signal: std::sync::Arc<S>,
    rooms: std::sync::RwLock<HashMap<String, RoomSupervisor>>,
}

impl<S: LiveKitSignal> ParrotLiveKitBridge<S> {
    pub fn new(signal: std::sync::Arc<S>) -> Self {
        Self {
            signal,
            rooms: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Create or replace a room model, discarding its previous metadata.
    pub fn open_room(&self, room: &str) {
        self.rooms
            .write()
            .unwrap()
            .insert(room.into(), RoomSupervisor::new(room));
    }

    pub fn close_room(&self, room: &str) {
        if let Some(r) = self.rooms.write().unwrap().get_mut(room) {
            r.close();
        }
    }

    /// 事件泵一步（信令 → 领域动作 → 下行确认）。
    ///
    /// Return the number of polled events, including rejected/discarded events.
    /// The host drives this method; it is not an actor mailbox drain barrier.
    pub fn pump(&self, room: &str) -> usize {
        let events = self.signal.poll_events(room);
        let n = events.len();
        let mut rooms = self.rooms.write().unwrap();
        let Some(sup) = rooms.get_mut(room) else {
            return n; // 房间未开——事件丢弃
        };
        let mut downlinks = Vec::new();
        for ev in events {
            match ev {
                SignalEvent::ParticipantJoin {
                    identity,
                    can_subscribe,
                    can_publish,
                } => {
                    if sup.join(&identity, can_subscribe, can_publish).is_ok() {
                        downlinks.push((identity, b"joined".to_vec()));
                    }
                }
                SignalEvent::ParticipantLeave { identity } => {
                    if sup.leave(&identity).is_ok() {
                        downlinks.push((identity, b"left".to_vec()));
                    }
                }
                SignalEvent::TrackPublished {
                    publisher,
                    sid,
                    kind,
                } => {
                    if let Ok(key) = sup.publish_track(&publisher, &sid, kind) {
                        // Return a suggested key; no receptionist operation occurs here.
                        downlinks.push((publisher, key.into_bytes()));
                    }
                }
                SignalEvent::Subscribe {
                    subscriber,
                    track_sid,
                } => match sup.can_subscribe(&subscriber, &track_sid) {
                    Ok(()) => downlinks.push((subscriber, b"subscribed".to_vec())),
                    Err(e) => {
                        let msg = format!("denied:{e:?}");
                        downlinks.push((subscriber, msg.into_bytes()));
                    }
                },
            }
        }
        // External callbacks may inspect or update this bridge.
        drop(rooms);
        for (to, payload) in downlinks {
            self.signal.send_downlink(room, &to, &payload);
        }
        n
    }

    pub fn room_state(&self, room: &str) -> Option<RoomState> {
        self.rooms.read().unwrap().get(room).map(|r| r.state)
    }

    pub fn with_room<R>(&self, room: &str, f: impl FnOnce(&RoomSupervisor) -> R) -> Option<R> {
        self.rooms.read().unwrap().get(room).map(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn livekit_room_model_open_close_and_join_acknowledgement() {
        let bridge = ParrotLiveKitBridge::new(std::sync::Arc::new(InMemSignal::default()));
        bridge.open_room("r1");
        assert_eq!(bridge.room_state("r1"), Some(RoomState::Open));

        // 未开房间的桥——事件丢弃（无下行）
        let sig_closed = InMemSignal::default();
        sig_closed.inject(
            "r1",
            SignalEvent::ParticipantJoin {
                identity: "bob".into(),
                can_subscribe: true,
                can_publish: true,
            },
        );
        let b2 = ParrotLiveKitBridge::new(std::sync::Arc::new(sig_closed.clone()));
        assert_eq!(b2.pump("r1"), 1);
        assert!(
            sig_closed.downlinks().is_empty(),
            "closed room drops events"
        );

        // 已开房间：join 成功 + 下行 joined
        let sig = InMemSignal::default();
        sig.inject(
            "r1",
            SignalEvent::ParticipantJoin {
                identity: "alice".into(),
                can_subscribe: true,
                can_publish: true,
            },
        );
        let b3 = ParrotLiveKitBridge::new(std::sync::Arc::new(sig.clone()));
        b3.open_room("r1");
        b3.pump("r1");
        assert!(b3
            .with_room("r1", |r| r.participants.contains_key("alice"))
            .unwrap());
        assert!(
            sig.downlinks()
                .iter()
                .any(|(_, to, p)| to == "alice" && p == b"joined"),
            "join downlink"
        );

        bridge.close_room("r1");
        assert_eq!(bridge.room_state("r1"), Some(RoomState::Closed));
    }

    // Model track metadata, suggested key and subscription policy.
    #[test]
    fn track_publish_and_subscribe_policy() {
        let sig = InMemSignal::default();
        let bridge = ParrotLiveKitBridge::new(std::sync::Arc::new(sig.clone()));
        bridge.open_room("r1");
        sig.inject(
            "r1",
            SignalEvent::ParticipantJoin {
                identity: "pub".into(),
                can_subscribe: true,
                can_publish: true,
            },
        );
        sig.inject(
            "r1",
            SignalEvent::ParticipantJoin {
                identity: "viewer".into(),
                can_subscribe: true,
                can_publish: false,
            },
        );
        sig.inject(
            "r1",
            SignalEvent::TrackPublished {
                publisher: "pub".into(),
                sid: "t1".into(),
                kind: TrackKind::Video,
            },
        );
        bridge.pump("r1");
        // The model returns a suggested key, not a registration receipt.
        let dl = sig.downlinks();
        assert!(dl
            .iter()
            .any(|(_, to, p)| to == "pub" && p == b"media/track/r1/t1"));
        // 订阅成功
        sig.inject(
            "r1",
            SignalEvent::Subscribe {
                subscriber: "viewer".into(),
                track_sid: "t1".into(),
            },
        );
        bridge.pump("r1");
        assert!(sig
            .downlinks()
            .iter()
            .any(|(_, to, p)| to == "viewer" && p == b"subscribed"));
        // 无发布权者发轨道 → denied
        sig.inject(
            "r1",
            SignalEvent::TrackPublished {
                publisher: "viewer".into(),
                sid: "t2".into(),
                kind: TrackKind::Audio,
            },
        );
        bridge.pump("r1");
        assert!(bridge
            .with_room("r1", |r| !r.tracks.contains_key("t2"))
            .unwrap());
    }

    // F9：AI 降采样 ≤10fps 硬门槛
    #[test]
    fn livekit_ai_downsample_gate() {
        let mut sup = RoomSupervisor::new("r1");
        sup.join("w", true, true).unwrap();
        sup.publish_track("w", "cam", TrackKind::Video).unwrap();
        assert!(sup.ai_downsample("cam", 25).is_err(), "25fps 拒");
        assert!(sup.ai_downsample("cam", 10).is_ok());
        assert_eq!(sup.tracks["cam"].downstream_fps, Some(10));
        assert!(sup.ai_downsample("cam", 5).is_ok(), "更低帧率可再降");
    }
}
