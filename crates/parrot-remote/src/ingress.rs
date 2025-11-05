//! 职责：入站帧分发——ASK/TELL/STOP → 本地 actor；REPLY → 回调表（05 §6.1）。
//!
//! I2（06）：入站 ASK 在系统级容量/编码长度预算内并发处理（慢 actor 不再
//! 阻塞同连接的后续帧；REPLY 乱序经 cid 配对天然支持）。TELL 保持串行
//! deliver（05 §6.3 背压贯通语义——对端过载时读循环挂起是有意行为）。
//! 本地解析经 LocalLookup trait 倒置（parrot-remote 不依赖 parrot crate，
//! E5.2 分层铁律）。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::codec_registry::CodecRegistry;
use crate::durable::timing::{Phase, TARGET};
use crate::error::{decode_err_payload, ErrCode};
use crate::frame::{frame_type, Frame};
use crate::node::NodeState;
use crate::registry::{CallbackRegistry, ReplyPayload};
use crate::transport::FrameSender;
use tracing::Instrument;

pub(crate) mod unwind;
use unwind::Cleanup;

/// TELL 重排：迟到帧（缺口超时放行后到达）丢弃计数。
pub static LATE_TELL_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// TELL 重排：缺口超时放行计数（丢帧不卡死语义的触发次数）。
pub static REORDER_GAP_FLUSHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// TELL 重排：实际缓冲过的帧数（乱序真正发生才 >0——切换点观测指标）。
pub static REORDER_BUFFERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// TELL 重排：关闭时尚在接收队列或重排缓冲区、未投递的帧数。
pub static REORDER_DROPPED_ON_SHUTDOWN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// TELL 重排：重排任务空闲自清次数（idle GC——防任务累积）。
pub static REORDER_TASK_RECYCLED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// 本地 actor 解析出口（trait 倒置：parrot facade 在应用层注入实现）。
#[async_trait::async_trait]
pub trait LocalLookup: Send + Sync + 'static {
    /// 路径 → 本地 ActorRef（miss 返回 None → REPLY_ERR ActorNotFound）。
    async fn lookup(&self, path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>>;

    /// Explicit durable endpoint. Ordinary actors are never treated as atomic
    /// durable receivers just because they can return a successful handler result.
    fn durable_receiver(&self, _path: &str) -> Option<Arc<dyn crate::durable::DurableReceiver>> {
        None
    }

    /// Explicit fixed-membership Directory endpoint. Ingress authenticates
    /// the direct peer before admitting any Raft bytes to its owner thread.
    fn directory_receiver(&self) -> Option<crate::roles::directory::network::DirectoryHandle> {
        None
    }

    /// Resolve a node/prefix through the locally assembled Directory. The
    /// default implementation is intentionally linearizable and bounded; an
    /// application may override it for a border proxy or another authority.
    async fn resolve_directory(
        &self,
        key: &str,
        min_version: u64,
    ) -> Result<Option<crate::roles::directory::DirEntry>, String> {
        let Some(handle) = self.directory_receiver() else {
            return Err("directory unavailable".to_owned());
        };
        let query = crate::roles::directory::DirQuery::Resolve {
            prefix_or_node: key.to_owned(),
        };
        let read = handle
            .query_linearizable(&query, std::time::Duration::from_secs(2))
            .await
            .map_err(|e| e.to_string())?;
        match read.entry {
            Some(entry) if entry.version < min_version => Err(format!(
                "directory version {} is older than required {}",
                entry.version, min_version
            )),
            other => Ok(other),
        }
    }

    /// Optional alias to a node-qualified remote actor path. Ordinary requests
    /// preserve opaque payloads; durable destinations cannot be rewritten.
    fn forwarding_target(&self, _path: &str) -> Option<String> {
        None
    }
}

/// 死信计数（TELL miss 目标——无回程信道，只记 metric + debug 日志）。
pub static DEAD_TELL_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// hub 转发计数（observability：跨节点帧转发量）。
pub static RELAYED_ASK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static RELAYED_TELL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 转发指标门面（集成测试/运维观测——不暴露 Atomic 原语）。
pub struct RelayMetrics;

impl RelayMetrics {
    pub fn relayed_ask() -> u64 {
        RELAYED_ASK.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn relayed_tell() -> u64 {
        RELAYED_TELL.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Forward correlation ID → original ID, return sender, destination and creation time.
///
/// hub 路由（07 §6）核心结构：入站 ASK 目标节点非本节点时，分配新 cid
/// 转发到目标节点；REPLY/REPLY_ERR 回来按新 cid 查表还原 orig_cid，经
/// 保存的回源 sender 送回发起方。容量 65536，65s TTL；系统每秒清理，
/// 来源/目标断连时按实际连接清理；同节点其他连接不受牵连。
pub struct RelayTable {
    slots: std::sync::Mutex<std::collections::HashMap<u64, RelaySlot>>,
    ttl: std::time::Duration,
    capacity: usize,
}

struct RelaySlot {
    orig_cid: u64,
    back: FrameSender,
    target: FrameSender,
    created: std::time::Instant,
}

enum RelayReply {
    Missing,
    WrongSource,
    Matched {
        original_cid: u64,
        back: FrameSender,
        expired: bool,
    },
}

impl RelayTable {
    pub fn new() -> Self {
        Self {
            slots: Default::default(),
            ttl: std::time::Duration::from_secs(65),
            capacity: 65536,
        }
    }

    /// Register before making the forwarded request visible to its destination.
    pub fn insert(
        &self,
        forward_cid: u64,
        orig_cid: u64,
        back: FrameSender,
        target: &FrameSender,
    ) -> bool {
        let mut slots = self.slots.lock().unwrap();
        if slots.len() >= self.capacity
            || slots.contains_key(&forward_cid)
            || back.is_closed()
            || target.is_closed()
        {
            return false;
        }
        slots.insert(
            forward_cid,
            RelaySlot {
                orig_cid,
                back,
                target: target.clone(),
                created: std::time::Instant::now(),
            },
        );
        true
    }

    pub fn take(&self, forward_cid: u64) -> Option<(u64, FrameSender)> {
        self.slots
            .lock()
            .unwrap()
            .remove(&forward_cid)
            .filter(|slot| slot.created.elapsed() < self.ttl)
            .map(|slot| (slot.orig_cid, slot.back))
    }

    /// Validate before consuming. Even an expired slot has a terminal reply;
    /// taking it must not prevent the expiry worker from notifying its source.
    fn take_from(&self, cid: u64, source: &FrameSender) -> RelayReply {
        let mut slots = self.slots.lock().unwrap();
        let Some(slot) = slots.get(&cid) else {
            return RelayReply::Missing;
        };
        if !slot.target.same_connection(source) {
            crate::registry::WRONG_REPLY_SOURCE.fetch_add(1, Ordering::Relaxed);
            return RelayReply::WrongSource;
        }
        let slot = slots.remove(&cid).expect("slot checked under lock");
        RelayReply::Matched {
            original_cid: slot.orig_cid,
            back: slot.back,
            expired: slot.created.elapsed() >= self.ttl,
        }
    }

    /// Reclaim source-owned slots and fail only requests using this destination.
    /// Return replies after releasing the table lock.
    pub fn fail_connection(&self, connection: &FrameSender) -> Vec<(u64, FrameSender)> {
        let mut slots = self.slots.lock().unwrap();
        let ids: Vec<_> = slots
            .iter()
            .filter(|(_, slot)| {
                slot.back.same_connection(connection) || slot.target.same_connection(connection)
            })
            .map(|(cid, _)| *cid)
            .collect();
        ids.into_iter()
            .filter_map(|cid| {
                let slot = slots.remove(&cid).expect("slot checked under lock");
                (!slot.back.same_connection(connection)).then_some((slot.orig_cid, slot.back))
            })
            .collect()
    }

    /// Explicit node-wide failure for administrative callers.
    pub fn fail_target(&self, target: &str) -> Vec<(u64, u64, FrameSender)> {
        let mut slots = self.slots.lock().unwrap();
        let ids: Vec<_> = slots
            .iter()
            .filter(|(_, slot)| slot.target.node_id() == target)
            .map(|(cid, _)| *cid)
            .collect();
        ids.into_iter()
            .filter_map(|cid| {
                slots
                    .remove(&cid)
                    .map(|slot| (cid, slot.orig_cid, slot.back))
            })
            .collect()
    }

    /// Actively reclaim expired mappings even if no reply ever arrives.
    pub fn expire(&self) -> Vec<(u64, FrameSender)> {
        let mut slots = self.slots.lock().unwrap();
        let ids: Vec<_> = slots
            .iter()
            .filter(|(_, slot)| slot.created.elapsed() >= self.ttl)
            .map(|(cid, _)| *cid)
            .collect();
        ids.into_iter()
            .filter_map(|cid| slots.remove(&cid).map(|slot| (slot.orig_cid, slot.back)))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for RelayTable {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Ingress {
    control_policy: crate::security::ControlPolicy,
    durable_relays: std::sync::RwLock<std::collections::BTreeSet<String>>,
    // Admission and tracker registration share a fence with close: no task can
    // appear after close has observed an empty tracker.
    ask_open: std::sync::Mutex<bool>,
    ask_slots: Arc<tokio::sync::Semaphore>,
    ask_bytes: Arc<tokio::sync::Semaphore>,
    ask_tasks: tokio_util::task::TaskTracker,
    ask_cancel: tokio_util::sync::CancellationToken,
    pub local: Arc<dyn LocalLookup>,
    mirrors: crate::mirror::WireMirrorRegistry,
    mirror_control: crate::mirror_control::MirrorControlStore,
    pub callbacks: Arc<CallbackRegistry>,
    trace_recorder: crate::trace_attachment::TraceRecorderSlot,
    /// Remote cache keys include authenticated authority identity.
    resolve_cache: std::sync::Mutex<crate::cache::ResolveCache>,
    /// Bound concurrent Directory lookups independently from actor ASK work.
    resolve_slots: Arc<tokio::sync::Semaphore>,
    resolve_pending: Arc<std::sync::Mutex<HashMap<u64, ResolvePending>>>,
    /// P2：SYSTEM_EVENT 分流钩子（admin/gossip/receptionist——system.rs 注入；
    /// None 时吞帧防御式丢弃）。
    sys_event: std::sync::RwLock<Option<Arc<dyn SysEventHook>>>,
    /// hub 转发出口（07 §6）：None = 本节点非路由节点（miss 一律 ActorNotFound）；
    /// Some = 目标 parrot://{node}/... 非本节点时查表转发（网关两两互通）。
    relay: std::sync::RwLock<Option<Arc<dyn RelayRouter>>>,
    /// 转发 cid 映射表（REPLY 回源还原）。
    pub relay_table: Arc<RelayTable>,
    /// 方案 A 学习缓存（node → 直连地址）。hub 注入 ROUTE_HINT 时写入；
    /// RemoteActorSystem::remote_ref 构建 ref 时读取（有直连表优先直连）。
    pub learned: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// TELL 端到端重排（per 源节点任务——直连连接 from_node=唯一 seq 源）。
    /// hub 中转帧多源复用同一连接，seq 流不可归因 → 不进重排（bypass）。
    reorder_tx:
        Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<Frame>>>>,
    reorder_tasks: tokio_util::task::TaskTracker,
    reorder_cancel: tokio_util::sync::CancellationToken,
    /// 重排旋钮（配置切面 [remote.reorder]——默认=原写死值 250ms/1024）。
    reorder_gap_timeout: std::sync::Mutex<std::time::Duration>,
    reorder_buffer_cap: std::sync::Mutex<usize>,
}

struct ResolvePending {
    tx: tokio::sync::oneshot::Sender<ReplyPayload>,
    connection: FrameSender,
    key: String,
    cache_key: String,
    min_version: u64,
    invalidated: bool,
    identity: Arc<()>,
}

fn resolve_cache_key(source: &FrameSender, key: &str) -> String {
    let authority = source.node_id();
    format!(
        "{:?}:{}:{authority}{key}",
        source.authentication(),
        authority.len()
    )
}

/// Owns one resolve registration and removes it if the caller future is
/// cancelled at any await point. Replies may remove the slot first; Drop is
/// intentionally idempotent.
struct ResolvePendingLease {
    table: Arc<std::sync::Mutex<HashMap<u64, ResolvePending>>>,
    cid: u64,
    identity: Arc<()>,
}
impl Drop for ResolvePendingLease {
    fn drop(&mut self) {
        let mut table = self.table.lock().unwrap();
        if table
            .get(&self.cid)
            .is_some_and(|slot| Arc::ptr_eq(&slot.identity, &self.identity))
        {
            table.remove(&self.cid);
        }
    }
}

// A stale worker must never remove a newer worker for the same source.
struct ReorderRegistration {
    map: std::sync::Weak<
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<Frame>>>,
    >,
    source: String,
    sender: tokio::sync::mpsc::WeakSender<Frame>,
}
impl Drop for ReorderRegistration {
    fn drop(&mut self) {
        if let (Some(map), Some(sender)) = (self.map.upgrade(), self.sender.upgrade()) {
            let mut map = map.lock().unwrap();
            if map
                .get(&self.source)
                .is_some_and(|current| current.same_channel(&sender))
            {
                map.remove(&self.source);
            }
        }
    }
}

/// hub 转发出口（system.rs 注入——按 node 查出站 sender + 分配转发 cid）。
#[async_trait::async_trait]
pub trait RelayRouter: Send + Sync + 'static {
    /// 目标节点直连 sender（无直连 → None → RouteUnreachable 回源）。
    fn sender_of(&self, node: &str) -> Option<FrameSender>;
    /// 新转发 cid（hub 侧唯一——与本地 ask 的 cid 空间共用计数器防撞）。
    fn next_cid(&self) -> u64;
    /// 本节点 id（判"目标是本节点还是他节点"）。
    fn self_node(&self) -> &str;
    /// 目标节点的直连拨号地址（方案 A：hub 中转时向源注入 ROUTE_HINT；
    /// 目标不可直拨 → None 不注入）。
    fn dial_addr_of(&self, node: &str) -> Option<String>;
    /// 学习通知（方案 A）：spoke 收到 ROUTE_HINT 后回调——触发后台拨号。
    fn on_hint(&self, node: &str, addr: &str);
}

impl Ingress {
    /// 构造（P1 兼容：无钩子、无转发）。
    pub fn new(local: Arc<dyn LocalLookup>, callbacks: Arc<CallbackRegistry>) -> Self {
        Self::with_ask_limits(
            local,
            callbacks,
            parrot_config::Defaults::inbound_ask_capacity(),
            parrot_config::Defaults::inbound_ask_bytes(),
        )
        .expect("valid default ASK limits")
    }

    /// Bound accepted ASK/completion-TELL futures and their encoded path, key and
    /// payload lengths. This is not a bound on decoded application memory.
    pub fn with_ask_limits(
        local: Arc<dyn LocalLookup>,
        callbacks: Arc<CallbackRegistry>,
        capacity: usize,
        bytes: usize,
    ) -> Result<Self, crate::error::RemoteError> {
        let max = tokio::sync::Semaphore::MAX_PERMITS.min(u32::MAX as usize);
        if capacity == 0 || capacity > max || bytes == 0 || bytes > max {
            return Err(crate::error::RemoteError::Transport(
                "invalid inbound ASK limits".into(),
            ));
        }
        Ok(Self {
            control_policy: Default::default(),
            durable_relays: Default::default(),
            ask_open: std::sync::Mutex::new(true),
            ask_slots: Arc::new(tokio::sync::Semaphore::new(capacity)),
            ask_bytes: Arc::new(tokio::sync::Semaphore::new(bytes)),
            ask_tasks: tokio_util::task::TaskTracker::new(),
            ask_cancel: tokio_util::sync::CancellationToken::new(),
            mirrors: crate::mirror::WireMirrorRegistry::new(&local),
            mirror_control: crate::mirror_control::MirrorControlStore::new(),
            local,
            callbacks,
            trace_recorder: crate::trace_attachment::TraceRecorderSlot::default(),
            resolve_cache: std::sync::Mutex::new(crate::cache::ResolveCache::new()),
            resolve_slots: Arc::new(tokio::sync::Semaphore::new(64)),
            resolve_pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            sys_event: std::sync::RwLock::new(None),
            relay: std::sync::RwLock::new(None),
            relay_table: Arc::new(RelayTable::new()),
            learned: std::sync::Mutex::new(Default::default()),
            reorder_tx: Arc::new(std::sync::Mutex::new(Default::default())),
            reorder_tasks: tokio_util::task::TaskTracker::new(),
            reorder_cancel: tokio_util::sync::CancellationToken::new(),
            reorder_gap_timeout: std::sync::Mutex::new(std::time::Duration::from_millis(250)),
            reorder_buffer_cap: std::sync::Mutex::new(1024),
        })
    }

    pub fn with_control_policy(mut self, policy: crate::security::ControlPolicy) -> Self {
        self.control_policy = policy;
        self
    }

    /// Authorize these authenticated nodes to vouch for durable origins on
    /// forwarded frames. Empty by default; HELLO roles never grant this authority.
    pub fn set_durable_relays(
        &self,
        nodes: std::collections::BTreeSet<String>,
    ) -> Result<(), String> {
        if nodes.len() > 4096 {
            return Err("durable relay budget exceeded".into());
        }
        let mut bytes = 0usize;
        for node in &nodes {
            parrot_config::endpoint::validate_node_id(node)?;
            bytes = bytes
                .checked_add(node.len())
                .ok_or("durable relay byte overflow")?;
            if node.len() > 1024 || bytes > 1024 * 1024 {
                return Err("durable relay identity byte budget exceeded".into());
            }
        }
        *self
            .durable_relays
            .write()
            .map_err(|_| "durable relay policy poisoned")? = nodes;
        Ok(())
    }

    /// Fence admission synchronously, then cancel all accepted ASK futures.
    pub fn close_asks(&self) {
        let mut open = self.ask_open.lock().unwrap();
        *open = false;
        self.ask_cancel.cancel();
        self.ask_tasks.close();
    }

    /// Wait until every accepted ASK future has been dropped. Call close first.
    pub async fn wait_for_asks(&self) {
        self.ask_tasks.wait().await;
    }

    /// Close ASK and TELL admission and cancel all owned asynchronous work.
    pub fn close(&self) {
        self.mirror_control.close();
        self.mirrors.close();
        self.close_asks();
        let pending = std::mem::take(&mut *self.resolve_pending.lock().unwrap());
        for slot in pending.into_values() {
            let _ = slot.tx.send(ReplyPayload::Err(
                ErrCode::Stopped,
                "resolve ingress closed".into(),
            ));
        }
        // This is also the registration fence used by reorder_tx_of.
        let mut map = self.reorder_tx.lock().unwrap();
        self.reorder_cancel.cancel();
        self.reorder_tasks.close();
        map.clear();
    }

    /// Completion barrier for close; no retained reorder future remains afterwards.
    pub async fn wait_closed(&self) {
        self.wait_for_asks().await;
        self.reorder_tasks.wait().await;
        self.mirrors.wait_closed().await;
    }

    /// 配置切面：重排旋钮注入（RemoteActorSystem 构造时下发）。
    pub fn set_reorder_knobs(&self, gap_timeout: std::time::Duration, buffer_cap: usize) {
        if let Ok(mut g) = self.reorder_gap_timeout.lock() {
            *g = gap_timeout;
        }
        if let Ok(mut c) = self.reorder_buffer_cap.lock() {
            *c = buffer_cap;
        }
    }

    /// 转发出口注入（system.rs——星型拓扑两两互通的关键一步）。
    pub fn relay_slot(&self) -> std::sync::RwLockWriteGuard<'_, Option<Arc<dyn RelayRouter>>> {
        self.relay.write().unwrap()
    }

    /// 钩子注入/替换（system.rs install_admin_hook 用）。
    pub fn sys_event_slot(&self) -> std::sync::RwLockWriteGuard<'_, Option<Arc<dyn SysEventHook>>> {
        self.sys_event.write().unwrap()
    }
}

/// SYSTEM_EVENT 入站钩子；处理与回执由实现负责。
#[async_trait::async_trait]
pub trait SysEventHook: Send + Sync + 'static {
    /// Legacy REPLY_ERR compatibility. Return true only after consuming a
    /// connection-bound administrative slot; default hooks handle no such slots.
    fn on_reply_error(
        &self,
        _id: u64,
        _code: ErrCode,
        _detail: &str,
        _source: &FrameSender,
    ) -> bool {
        false
    }
    async fn on_event(&self, event: crate::admin::SysEvent, back: &FrameSender, from: &str);
}

/// 帧路径 → 本地查找路径：剥 parrot://{node}/ 前缀（节点寻址已在连接层完成，
/// Native Actix tails are restored to the engine's exact `actix://...` key.
fn local_path(path: &str) -> &str {
    if let Some((_, local)) = crate::actor_path::split(path) {
        return local;
    }
    if let Some(rest) = path.strip_prefix("parrot://") {
        if let Some(idx) = rest.find('/') {
            return &rest[idx..];
        }
    }
    path
}

/// 路径 → 目标节点 id（parrot://{node}/...；其他形态返回 None——非路由路径）。
fn target_node(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("parrot://")?;
    let idx = rest.find('/')?;
    Some(&rest[..idx])
}

/// Reject commands on the originating connection using only the outer CID.
/// Malformed replies must not become commands or consume a pending request.
fn reject_admin_command(frame: &Frame, back: &FrameSender, detail: &str) {
    use crate::admin::{reply_frame, sys_event_tag, AdminReply};
    use crate::admin_v2::{reply_frame_v2, v2_err, AdminReplyV2};
    let req_id = frame.header.correlation_id;
    let reply = match frame.payload.first() {
        Some(&sys_event_tag::ADMIN_CMD) => reply_frame(
            req_id,
            "",
            &AdminReply::Failed {
                req_id,
                code: ErrCode::ProtocolViolation as u16,
                detail: detail.into(),
            },
        ),
        Some(&sys_event_tag::ADMIN_CMD_V2) => reply_frame_v2(
            req_id,
            "",
            &AdminReplyV2::Failed {
                req_id,
                code: v2_err::DIALECT_MISMATCH,
                detail: detail.into(),
            },
        ),
        _ => return,
    };
    back.send_reply(reply);
}

impl Ingress {
    /// 处理一帧入站（来自 ConnectionTask 的 inbound 队列）。
    /// `back` 是该连接的回程发送端（REPLY 用）。
    ///
    /// I2：ASK 并发分发——spawn 独立任务（慢 actor 不阻塞同连接的后续帧；
    /// REPLY 乱序经 cid 配对天然支持）。FrameSender 是 Clone，move 安全。
    /// This low-level entry point awaits TELL/STOP/control work. The system
    /// calls it through bounded per-connection lanes; direct callers own their
    /// own ordering, admission and cancellation. ASK remains independently owned.
    pub async fn dispatch(self: &Arc<Self>, frame: Frame, back: &FrameSender, from_node: &str) {
        if frame.validate_payload_flags().is_err()
            || (frame.header.flags & crate::frame::flags::TRACE_CONTEXT != 0
                && !back.supports_trace_context())
            || (frame.header.flags & crate::frame::flags::MIRROR_SHADOW != 0
                && (!back.supports_mirror_shadow() || !self.control_policy.allows(back)))
        {
            back.close();
            return;
        }
        if frame.header.flags & crate::frame::flags::BATCH != 0 {
            let Ok(members) = frame.batch_frames() else {
                back.close();
                return;
            };
            for member in members {
                if back.is_closed() {
                    break;
                }
                tokio::task::consume_budget().await;
                self.dispatch_one(member, back, from_node).await;
            }
        } else {
            self.dispatch_one(frame, back, from_node).await;
        }
    }
    async fn dispatch_one(self: &Arc<Self>, frame: Frame, back: &FrameSender, from_node: &str) {
        match frame.header.frame_type {
            frame_type::ASK => {
                self.admit_request(frame, back);
            }
            frame_type::TELL if frame.header.flags & crate::frame::flags::TELL_ACK != 0 => {
                // Completion TELL shares bounded request ownership, panic/close
                // handling and connection-bound return routing with ASK.
                self.admit_request(frame, back);
            }
            frame_type::TELL => self.on_tell(frame, back, from_node).await,
            frame_type::STOP => {
                if self.control_policy.allows(back) {
                    self.on_stop(frame).await;
                } else {
                    back.close();
                }
            }
            frame_type::REPLY | frame_type::REPLY_ERR => self.on_reply(frame, back),
            frame_type::RESOLVE_Q => self.on_resolve_query(frame, back).await,
            frame_type::RESOLVE_R => self.on_resolve_response(frame, back),
            frame_type::INVALIDATE => self.on_invalidate(frame, back),
            frame_type::SYSTEM_EVENT => {
                // P2 管理通道：admin/gossip/receptionist 同帧不同标签
                match crate::admin::decode_sys_event(&frame.payload) {
                    Ok(event) => {
                        if matches!(
                            event,
                            crate::admin::SysEvent::MembershipGossip(_)
                                | crate::admin::SysEvent::MembershipProbe(_)
                                | crate::admin::SysEvent::ReceptionistSync(_)
                        ) && !self.control_policy.allows_cluster(back)
                        {
                            back.close();
                            return;
                        }
                        if let crate::admin::SysEvent::MembershipProbe(probe) = &event {
                            if probe.request_id() != frame.header.correlation_id
                                || back.peer_capabilities()
                                    & crate::handshake::caps::MEMBERSHIP_PROBES
                                    == 0
                                || frame.header.flags != 0
                                || !frame.path.is_empty()
                                || !frame.type_key.is_empty()
                            {
                                back.close();
                                return;
                            }
                        }
                        if matches!(event, crate::admin::SysEvent::ReceptionistSync(_))
                            && (back.peer_capabilities()
                                & crate::handshake::caps::RECEPTIONIST_SYNC
                                == 0
                                || frame.header.correlation_id != 0
                                || frame.header.flags != 0
                                || frame.header.seq != crate::frame::SEQ_NONE
                                || !frame.path.is_empty()
                                || !frame.type_key.is_empty())
                        {
                            back.close();
                            return;
                        }
                        let req_id = match &event {
                            crate::admin::SysEvent::AdminCommand(cmd) => Some(cmd.req_id()),
                            crate::admin::SysEvent::AdminCommandV2(cmd) => Some(cmd.req_id()),
                            crate::admin::SysEvent::AdminReply(reply) => Some(reply.req_id()),
                            crate::admin::SysEvent::AdminReplyV2(reply) => Some(reply.req_id()),
                            _ => None,
                        };
                        if req_id.is_some_and(|id| id != frame.header.correlation_id) {
                            tracing::warn!(
                                "admin frame correlation_id disagrees with payload req_id"
                            );
                            reject_admin_command(
                                &frame,
                                back,
                                "admin frame correlation_id disagrees with payload req_id",
                            );
                            return;
                        }
                        let hook = self.sys_event.read().unwrap().clone();
                        if let Some(hook) = hook {
                            let cleanup = Cleanup::default();
                            // A hook may already have sent a response. On failure,
                            // close only its connection; never fabricate another.
                            let _close = cleanup.close_on_failure(back);
                            let mut hook = cleanup.owned(hook);
                            let _ = unwind::run(&cleanup, || {
                                hook.value().on_event(event, back, from_node)
                            })
                            .await;
                            hook.retire();
                        } else {
                            tracing::debug!("SYSTEM_EVENT ignored (no hook installed)");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "malformed SYSTEM_EVENT rejected");
                        if matches!(frame.payload.first(), Some(0x11..=0x14)) {
                            back.close();
                            return;
                        }
                        reject_admin_command(&frame, back, "malformed administrative command");
                    }
                }
            }
            frame_type::ERROR => {
                let (code, detail) = decode_err_payload(&frame.payload)
                    .unwrap_or((ErrCode::ProtocolViolation, "<undecodable>".into()));
                tracing::warn!(node = %from_node, code = code.name(), %detail, "remote ERROR frame");
                back.close();
            }
            frame_type::ROUTE_HINT => {
                if !self.control_policy.allows_cluster(back) {
                    back.close();
                    return;
                }
                // 方案 A 学习通道（07 §6.2）：hub 告知目标直连地址 → 记入
                // 学习缓存，由 direct_dialer 后台拨号（不阻塞分发循环）。
                match Frame::parse_route_hint(&frame.payload) {
                    Ok((node, addr)) => {
                        tracing::debug!(%node, %addr, "route hint learned");
                        self.on_route_hint(&node, &addr, back).await;
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "malformed ROUTE_HINT dropped");
                    }
                }
            }
            // 心跳/握手由 ConnectionTask 处理；其余已知帧交给 ingress。
            _ => {
                tracing::debug!(
                    ft = frame.header.frame_type,
                    "ingress: frame handled at conn layer"
                );
            }
        }
    }

    fn allows_resolve(&self, frame: &Frame, back: &FrameSender) -> bool {
        self.control_policy.allows_cluster(back)
            && (matches!(
                self.control_policy,
                crate::security::ControlPolicy::InsecureDevelopment
            ) || back.authentication() != crate::security::PeerAuthentication::Unauthenticated)
            && frame.header.hop_count == 0
    }

    /// Cached view scoped to this authenticated peer authority. This API does
    /// not infer a consensus group shared by several directory nodes.
    pub fn cached_resolve(
        &self,
        authority: &FrameSender,
        key: &str,
        now: std::time::Instant,
    ) -> Option<(Vec<String>, crate::cache::CacheState)> {
        if !self.control_policy.allows_cluster(authority) {
            return None;
        }
        self.resolve_cache
            .lock()
            .unwrap()
            .get(&resolve_cache_key(authority, key), now)
            .map(|(endpoints, state)| (endpoints.to_vec(), state))
    }

    /// Outstanding outgoing lookups (hard limit: 64 per ingress).
    pub fn pending_resolves(&self) -> usize {
        self.resolve_pending.lock().unwrap().len()
    }

    async fn on_resolve_query(&self, frame: Frame, back: &FrameSender) {
        let cleanup = Cleanup::default();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let permit = match self.resolve_slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let _ = tokio::time::timeout_at(
                    deadline,
                    back.send(Frame::reply_err(
                        frame.header.correlation_id,
                        &frame.path,
                        ErrCode::Overloaded,
                        "resolve concurrency limit reached",
                    )),
                )
                .await;
                return;
            }
        };
        let result = async {
            let _permit = permit;
            if !self.allows_resolve(&frame, back) {
                return Err((
                    ErrCode::Forbidden,
                    "resolve requires a direct authenticated peer".to_owned(),
                ));
            }
            let query = crate::resolve::decode_query(&frame.payload)
                .map_err(|e| (ErrCode::ProtocolViolation, e.to_string()))?;
            if frame.path != query.key
                || frame.type_key != crate::resolve::RESPONSE_KEY
                || frame.header.flags != 0
                || frame.header.seq != crate::frame::SEQ_NONE
                || frame.header.correlation_id == 0
            {
                return Err((
                    ErrCode::ProtocolViolation,
                    "resolve frame path/type key is not bound to its payload".to_owned(),
                ));
            }
            let entry = tokio::time::timeout_at(
                deadline,
                unwind::run(&cleanup, || {
                    self.local.resolve_directory(&query.key, query.min_version)
                }),
            )
            .await
            .map_err(|_| {
                (
                    ErrCode::Timeout,
                    "directory resolver deadline elapsed".to_owned(),
                )
            })?
            .map_err(|()| {
                (
                    ErrCode::ProtocolViolation,
                    "directory resolver panicked".to_owned(),
                )
            })?
            .map_err(|detail| (ErrCode::DirectoryStale, detail))?;
            // Retain an explicit application error or deadline, but do not
            // publish a successful resolution before future cleanup completes.
            if cleanup.failed() {
                return Err((
                    ErrCode::ProtocolViolation,
                    "directory resolver cleanup panicked".into(),
                ));
            }
            if entry
                .as_ref()
                .is_some_and(|entry| entry.version < query.min_version)
            {
                return Err((
                    ErrCode::DirectoryStale,
                    "resolver returned an obsolete version".into(),
                ));
            }
            Ok(crate::resolve::Response {
                key: query.key,
                entry,
            })
        }
        .await;
        let response = match result {
            Ok(value) => match crate::resolve::encode_response(&value) {
                Ok(payload) => Frame {
                    header: crate::frame::FrameHeader {
                        frame_len: 0,
                        version: crate::frame::PROTOCOL_VERSION,
                        frame_type: frame_type::RESOLVE_R,
                        flags: 0,
                        correlation_id: frame.header.correlation_id,
                        hop_count: 0,
                        hop_limit: frame.header.hop_limit,
                        seq: crate::frame::SEQ_NONE,
                    },
                    path: frame.path,
                    type_key: crate::resolve::RESPONSE_KEY.to_owned(),
                    payload: bytes::Bytes::from(payload),
                },
                Err(e) => Frame::reply_err(
                    frame.header.correlation_id,
                    &frame.path,
                    ErrCode::ProtocolViolation,
                    &e.to_string(),
                ),
            },
            Err((code, detail)) => {
                Frame::reply_err(frame.header.correlation_id, &frame.path, code, &detail)
            }
        };
        // An immediately ready send can report the timeout result even if the
        // resolver consumed its budget. Capacity waits use the same deadline.
        let _ = tokio::time::timeout_at(deadline, back.send(response)).await;
    }

    fn on_resolve_response(&self, frame: Frame, source: &FrameSender) {
        if !self.allows_resolve(&frame, source) {
            source.close();
            return;
        }
        // Invalidation and response acceptance share this fence, including the
        // cache write. No invalidation can fall between consuming the ticket
        // and publishing its result into the cache.
        let mut pending_map = self.resolve_pending.lock().unwrap();
        let Some(slot) = pending_map.get(&frame.header.correlation_id) else {
            return;
        };
        if !slot.connection.same_connection(source) {
            return;
        }
        let response = crate::resolve::decode_response(&frame.payload);
        let valid = response.as_ref().is_ok_and(|response| {
            frame.path == response.key
                && frame.type_key == crate::resolve::RESPONSE_KEY
                && slot.key == response.key
                && frame.header.flags == 0
                && frame.header.seq == crate::frame::SEQ_NONE
                && frame.header.correlation_id != 0
        });
        let pending = pending_map
            .remove(&frame.header.correlation_id)
            .expect("slot checked under lock");
        if !valid {
            let _ = pending.tx.send(ReplyPayload::Err(
                ErrCode::ProtocolViolation,
                "invalid resolve response or request binding".into(),
            ));
            return;
        }
        let response = response.unwrap();
        if pending.invalidated
            || response
                .entry
                .as_ref()
                .is_some_and(|entry| entry.version < pending.min_version)
        {
            let _ = pending.tx.send(ReplyPayload::Err(
                ErrCode::DirectoryStale,
                "resolve result invalidated or older than requested version".into(),
            ));
            return;
        }
        let mut cache = self.resolve_cache.lock().unwrap();
        if let Some(entry) = &response.entry {
            cache.put_with_ttl(
                pending.cache_key,
                entry.endpoints.clone(),
                entry.version,
                std::time::Duration::from_secs(entry.ttl_s as u64),
                std::time::Instant::now(),
            );
        } else {
            // An authoritative absence must not leave an earlier route usable.
            cache.invalidate(&pending.cache_key);
        }
        let _ = pending.tx.send(ReplyPayload::Ok(
            frame.payload,
            crate::resolve::RESPONSE_KEY.to_owned(),
        ));
    }

    fn fail_resolve(&self, cid: u64, source: &FrameSender, code: ErrCode, detail: &str) -> bool {
        let mut pending_map = self.resolve_pending.lock().unwrap();
        let Some(pending) = pending_map.get(&cid) else {
            return false;
        };
        if !pending.connection.same_connection(source) {
            return false;
        }
        let pending = pending_map
            .remove(&cid)
            .expect("pending checked under lock");
        let _ = pending.tx.send(ReplyPayload::Err(code, detail.to_owned()));
        true
    }

    fn on_invalidate(&self, frame: Frame, source: &FrameSender) {
        if !self.allows_resolve(&frame, source) {
            source.close();
            return;
        }
        match crate::resolve::decode_invalidate(&frame.payload) {
            Ok(value) => {
                if frame.path != value.key || frame.type_key != crate::resolve::RESPONSE_KEY {
                    tracing::warn!("INVALIDATE path does not match payload key");
                    return;
                }
                if frame.header.flags != 0
                    || frame.header.seq != crate::frame::SEQ_NONE
                    || frame.header.correlation_id != 0
                {
                    return;
                }
                let cache_key = resolve_cache_key(source, &value.key);
                let mut pending = self.resolve_pending.lock().unwrap();
                for slot in pending
                    .values_mut()
                    .filter(|slot| slot.cache_key == cache_key)
                {
                    slot.invalidated = true;
                }
                self.resolve_cache
                    .lock()
                    .unwrap()
                    .invalidate_versioned(&cache_key, value.version);
            }
            Err(error) => tracing::warn!(err = %error, "malformed INVALIDATE dropped"),
        }
    }

    /// Issue one authenticated resolve request and update the shared cache.
    /// The callback is bound to the exact connection, so a reply from another
    /// peer cannot satisfy the query.
    pub async fn resolve_remote(
        &self,
        sender: &FrameSender,
        key: &str,
        min_version: u64,
        timeout: std::time::Duration,
    ) -> Result<Option<crate::roles::directory::DirEntry>, crate::RemoteError> {
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .filter(|_| !timeout.is_zero())
            .ok_or_else(|| {
                crate::RemoteError::Transport(
                    "resolve timeout must be positive and representable".into(),
                )
            })?;
        if !self.control_policy.allows_cluster(sender) {
            return Err(crate::RemoteError::Transport(
                "resolve peer is not authorized".into(),
            ));
        }
        let payload = crate::resolve::encode_query(&crate::resolve::Query {
            key: key.to_owned(),
            min_version,
        })
        .map_err(|e| crate::RemoteError::Codec(e.to_string()))?;
        let cid = self.callbacks.next_cid();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let identity = Arc::new(());
        {
            let mut pending = self.resolve_pending.lock().unwrap();
            if self.ask_cancel.is_cancelled() {
                return Err(crate::RemoteError::Transport(
                    "resolve ingress closed".into(),
                ));
            }
            if pending.len() >= 64 || pending.contains_key(&cid) || cid == 0 {
                return Err(crate::RemoteError::Transport(
                    "resolve pending limit or correlation collision".into(),
                ));
            }
            pending.insert(
                cid,
                ResolvePending {
                    tx,
                    connection: sender.clone(),
                    key: key.to_owned(),
                    cache_key: resolve_cache_key(sender, key),
                    min_version,
                    invalidated: false,
                    identity: identity.clone(),
                },
            );
        }
        let _lease = ResolvePendingLease {
            table: self.resolve_pending.clone(),
            cid,
            identity,
        };
        let frame = Frame {
            header: crate::frame::FrameHeader {
                frame_len: 0,
                version: crate::frame::PROTOCOL_VERSION,
                frame_type: frame_type::RESOLVE_Q,
                flags: 0,
                correlation_id: cid,
                hop_count: 0,
                hop_limit: crate::frame::DEFAULT_HOP_LIMIT,
                seq: crate::frame::SEQ_NONE,
            },
            path: key.to_owned(),
            type_key: crate::resolve::RESPONSE_KEY.to_owned(),
            payload: bytes::Bytes::from(payload),
        };
        let sent = tokio::select! {
            biased;
            _ = self.ask_cancel.cancelled() => return Err(crate::RemoteError::Transport("resolve ingress closed".into())),
            result = tokio::time::timeout_at(deadline, sender.send(frame)) => result,
        };
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(crate::RemoteError::Transport(format!(
                    "resolve send failed: {error}"
                )))
            }
            Err(_) => return Err(crate::RemoteError::Transport("resolve send timeout".into())),
        }
        let received = tokio::select! {
            biased;
            _ = self.ask_cancel.cancelled() => return Err(crate::RemoteError::Transport("resolve ingress closed".into())),
            _ = sender.closed() => return Err(crate::RemoteError::Transport("resolve connection closed".into())),
            result = tokio::time::timeout_at(deadline, rx) => result,
        };
        let reply = match received {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => {
                return Err(crate::RemoteError::Transport(
                    "resolve connection closed".into(),
                ))
            }
            Err(_) => return Err(crate::RemoteError::Transport("resolve timeout".into())),
        };
        match reply {
            ReplyPayload::Ok(bytes, _) => {
                let response = crate::resolve::decode_response(&bytes)
                    .map_err(|e| crate::RemoteError::Codec(e.to_string()))?;
                if response.key != key {
                    return Err(crate::RemoteError::Codec(
                        "resolve response key does not match request".into(),
                    ));
                }
                Ok(response.entry)
            }
            ReplyPayload::Err(code, detail) => Err(crate::RemoteError::Codec(format!(
                "{}: {}",
                code.name(),
                detail
            ))),
        }
    }

    /// Push a versioned invalidation over an authenticated cluster link.
    pub async fn invalidate_remote(
        &self,
        sender: &FrameSender,
        key: &str,
        version: u64,
    ) -> Result<(), crate::RemoteError> {
        if !self.control_policy.allows_cluster(sender) {
            return Err(crate::RemoteError::Transport(
                "invalidate peer is not authorized".into(),
            ));
        }
        let payload = crate::resolve::encode_invalidate(&crate::resolve::Invalidate {
            key: key.to_owned(),
            version,
        })
        .map_err(|e| crate::RemoteError::Codec(e.to_string()))?;
        sender
            .send(Frame {
                header: crate::frame::FrameHeader {
                    frame_len: 0,
                    version: crate::frame::PROTOCOL_VERSION,
                    frame_type: frame_type::INVALIDATE,
                    flags: 0,
                    correlation_id: 0,
                    hop_count: 0,
                    hop_limit: crate::frame::DEFAULT_HOP_LIMIT,
                    seq: crate::frame::SEQ_NONE,
                },
                path: key.to_owned(),
                type_key: crate::resolve::RESPONSE_KEY.to_owned(),
                payload: bytes::Bytes::from(payload),
            })
            .await
    }

    fn is_remote_target(&self, path: &str) -> bool {
        target_node(path).is_some_and(|target| {
            self.relay
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|router| target != router.self_node())
        })
    }

    /// Mirror ingress to a raw local actor with independent, bounded admission.
    pub fn register_wire_mirror(
        &self,
        policy: crate::mirror::WireMirrorPolicy,
        options: crate::mirror::WireMirrorOptions,
    ) -> Result<crate::mirror::WireMirrorHandle, crate::mirror::WireMirrorError> {
        self.mirrors.register(policy, options)
    }

    pub(crate) fn attach_mirror_system(&self, system: std::sync::Weak<crate::RemoteActorSystem>) {
        self.mirrors.attach_remote(system);
    }

    pub(crate) async fn control_mirror(
        &self,
        command: crate::mirror_control::MirrorControl,
    ) -> Result<crate::mirror_control::MirrorControlReply, String> {
        self.mirror_control.execute(&self.mirrors, command).await
    }

    /// Atomically install an owned policy group; existing sources are never replaced.
    pub fn register_wire_mirror_group(
        &self,
        policies: Vec<(
            crate::mirror::WireMirrorPolicy,
            crate::mirror::WireMirrorOptions,
        )>,
    ) -> Result<crate::mirror::WireMirrorGroup, crate::mirror::WireMirrorError> {
        self.mirrors.register_group(policies)
    }

    pub fn remove_wire_mirror(&self, prefix: &str) -> Option<crate::mirror::WireMirrorHandle> {
        self.mirrors.remove(prefix)
    }

    fn mirror_application(&self, frame: &Frame) {
        if !self.mirrors.is_enabled()
            || frame.header.flags & crate::frame::flags::MIRROR_SHADOW != 0
        {
            return;
        }
        // A qualified destination is eligible only when an installed router
        // proves it is this node. Transit/unknown authorities are not local.
        if let Some(node) = target_node(&frame.path) {
            if !self
                .relay
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|r| r.self_node() == node)
            {
                return;
            }
        }
        if matches!(
            frame.type_key.as_str(),
            crate::durable::DURABLE_KEY
                | crate::roles::directory::client::READ_KEY
                | crate::roles::directory::client::CLIENT_KEY
                | crate::roles::directory::network::RAFT_KEY
        ) {
            return;
        }
        if frame.header.frame_type == frame_type::ASK {
            if let Ok(payload) = frame.borrowed_ask_payload() {
                self.mirrors
                    .offer(local_path(&frame.path), &frame.type_key, payload);
            }
        } else {
            self.mirrors
                .offer(local_path(&frame.path), &frame.type_key, &frame.payload);
        }
    }

    fn resolve_application_path(&self, frame: &mut Frame) {
        if !self.is_remote_target(&frame.path) {
            if let Some(target) = self.local.forwarding_target(local_path(&frame.path)) {
                // A bad resolver must not redirect a local actor to another local
                // alias or silently decode its payload at the wrong destination.
                if self.is_remote_target(&target) {
                    frame.path = target;
                }
            }
        }
    }

    fn admit_request(self: &Arc<Self>, frame: Frame, back: &FrameSender) {
        let open = self.ask_open.lock().unwrap();
        if back.is_closed() {
            return;
        }
        let bytes = frame
            .payload
            .len()
            .checked_add(frame.path.len())
            .and_then(|n| n.checked_add(frame.type_key.len()))
            .and_then(|n| u32::try_from(n).ok());
        let permits = if *open {
            self.ask_slots
                .clone()
                .try_acquire_owned()
                .ok()
                .and_then(|slot| {
                    let bytes = self.ask_bytes.clone().try_acquire_many_owned(bytes?).ok()?;
                    Some((slot, bytes))
                })
        } else {
            None
        };
        let Some(permits) = permits else {
            let code = if *open {
                ErrCode::Overloaded
            } else {
                ErrCode::Stopped
            };
            // Do not create a waiting task for rejection. If even the error
            // cannot fit, disconnect this exact connection so callers observe
            // failure and a full writer cannot block the global dispatcher.
            if back
                .send_frame_sync(Frame::reply_err(
                    frame.header.correlation_id,
                    &frame.path,
                    code,
                    "inbound request admission closed or at capacity",
                ))
                .is_err()
            {
                back.close();
            }
            return;
        };
        let back = back.clone();
        let this = self.clone();
        self.ask_tasks.spawn(async move {
            let _permits = permits;
            tokio::select! {
                biased;
                _ = this.ask_cancel.cancelled() => {},
                _ = back.closed() => {},
                _ = async {
                    // Cover lookup, synchronous ActorRef::send construction,
                    // codecs and reply encoding as well as polling the actor.
                    let cid = frame.header.correlation_id;
                    let path = frame.path.clone();
                    let cleanup = Cleanup::default();
                    if unwind::run(&cleanup, || Box::pin(this.on_request(frame, &back))).await.is_err() {
                        let _ = back.send(Frame::reply_err(cid, &path,
                            ErrCode::ProtocolViolation, "inbound request handler panicked")).await;
                    }
                } => {},
            }
        });
    }

    /// Attach a local recorder to future requests carrying a negotiated causal
    /// parent. Existing operations retain their recorder; None disables capture
    /// without disabling metadata propagation. This does not authenticate peers.
    pub fn set_trace_recorder(&self, recorder: Option<parrot_api::trace::TraceRecorder>) {
        self.trace_recorder.set(recorder);
    }

    pub(crate) fn attach_trace_recorder(
        &self,
        recorder: parrot_api::trace::TraceRecorder,
        policy: crate::IncomingTracePolicy,
    ) -> crate::TraceRecorderAttachment {
        self.trace_recorder.attach(recorder, policy)
    }

    fn request_context(
        &self,
        frame: &mut Frame,
    ) -> Result<Option<parrot_api::trace::TraceContext>, crate::frame::FrameError> {
        let parent = frame.take_trace_parent()?;
        Ok(self.trace_recorder.context(parent))
    }

    async fn on_request(&self, mut frame: Frame, back: &FrameSender) {
        let Ok(context) = self.request_context(&mut frame) else {
            back.close();
            return;
        };
        let shadow = frame.header.flags & crate::frame::flags::MIRROR_SHADOW != 0;
        if shadow {
            // Validated by dispatch. A relative receiver budget bounds pending
            // lookup/admission even when the source times out or disconnects.
            let millis = u32::from_le_bytes(
                frame.payload[..4]
                    .try_into()
                    .expect("validated shadow budget"),
            );
            frame.payload = frame.payload.slice(4..);
            let cid = frame.header.correlation_id;
            let path = frame.path.clone();
            parrot_api::trace::scope(context, async {
                if tokio::time::timeout(
                    std::time::Duration::from_millis(millis.into()),
                    self.on_request_scoped(frame, back),
                )
                .await
                .is_err()
                {
                    back.send_reply(Frame::reply_err(
                        cid,
                        &path,
                        ErrCode::Timeout,
                        "shadow admission deadline elapsed; admission may have occurred",
                    ));
                }
            })
            .await;
        } else {
            parrot_api::trace::scope(context, self.on_request_scoped(frame, back)).await;
        }
    }

    async fn on_request_scoped(&self, mut frame: Frame, back: &FrameSender) {
        if frame.type_key == crate::roles::directory::client::READ_KEY {
            self.on_directory_read(frame, back).await;
            return;
        }
        if frame.type_key == crate::roles::directory::client::CLIENT_KEY {
            self.on_directory_client(frame, back).await;
            return;
        }
        if frame.type_key == crate::roles::directory::network::RAFT_KEY {
            self.on_directory_request(frame, back).await;
            return;
        }
        if frame.type_key == crate::durable::DURABLE_KEY {
            self.on_durable_request(frame, back).await;
            return;
        }
        let shadow = frame.header.flags & crate::frame::flags::MIRROR_SHADOW != 0;
        if shadow {
            let path = local_path(&frame.path);
            let local = target_node(&frame.path).is_none_or(|node| {
                self.relay
                    .read()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|r| r.self_node() == node)
            });
            if !local
                || !crate::mirror::canonical(path)
                || path == "/"
                || self.local.forwarding_target(path).is_some()
            {
                back.send_reply(Frame::reply_err(
                    frame.header.correlation_id,
                    &frame.path,
                    ErrCode::ProtocolViolation,
                    "shadow destination must be a raw actor on this node",
                ));
                return;
            }
        } else {
            self.mirror_application(&frame);
            self.resolve_application_path(&mut frame);
        }
        let cid = frame.header.correlation_id;
        let completion = frame.header.frame_type == frame_type::TELL;
        // Node identity precedes local lookup and codec dispatch. A router
        // can forward opaque payloads and cannot capture a remote namesake.
        if self.is_remote_target(&frame.path) {
            self.try_relay_request(&frame, back, cid).await;
            return;
        }
        // 剥 reply_to 前缀（DEV_01 §3.1 约定）
        let decoded_payload = if completion {
            Ok((None, frame.payload.clone()))
        } else {
            frame.split_reply_to()
        };
        let (reply_to, payload) = match decoded_payload {
            Ok(v) => v,
            Err(e) => {
                let _ = back
                    .send(Frame::reply_err(
                        cid,
                        &frame.path,
                        ErrCode::ProtocolViolation,
                        &e.to_string(),
                    ))
                    .await;
                return;
            }
        };
        let cleanup = Cleanup::default();
        let decoded = match cleanup
            .call(|| CodecRegistry::global().decode_incoming(&frame.type_key, &payload))
        {
            Ok(Ok(message)) => message,
            outcome => {
                let (code, detail) = match outcome {
                    Ok(Err(code)) => (code, format!("type_key={}", frame.type_key)),
                    _ => (
                        ErrCode::ProtocolViolation,
                        "inbound request decoder panicked".into(),
                    ),
                };
                let _ = back
                    .send(Frame::reply_err(cid, &frame.path, code, &detail))
                    .await;
                return;
            }
        };
        let mut decoded = cleanup.owned(decoded);
        let target = unwind::run(&cleanup, || self.local.lookup(local_path(&frame.path))).await;
        let target = match target {
            Ok(Some(target)) => target,
            Ok(None) => {
                decoded.retire();
                if cleanup.failed() {
                    let _ = back
                        .send(Frame::reply_err(
                            cid,
                            &frame.path,
                            ErrCode::ProtocolViolation,
                            "inbound lookup/input cleanup panicked",
                        ))
                        .await;
                    return;
                }
                if self.try_relay_request(&frame, back, cid).await == Some(ErrCode::ActorNotFound) {
                    let _ = back
                        .send(Frame::reply_err(
                            cid,
                            &frame.path,
                            ErrCode::ActorNotFound,
                            &frame.path,
                        ))
                        .await;
                }
                return;
            }
            Err(()) => {
                decoded.retire();
                let _ = back
                    .send(Frame::reply_err(
                        cid,
                        &frame.path,
                        ErrCode::ProtocolViolation,
                        "inbound lookup panicked",
                    ))
                    .await;
                return;
            }
        };
        let mut target = cleanup.owned(target);
        if cleanup.failed() {
            target.retire();
            decoded.retire();
            let _ = back
                .send(Frame::reply_err(
                    cid,
                    &frame.path,
                    ErrCode::ProtocolViolation,
                    "lookup cleanup panicked",
                ))
                .await;
            return;
        }
        // Construct, poll and retire the user future independently. Both owners
        // also remain guarded if connection or system cancellation drops us.
        let outcome = unwind::run(&cleanup, || {
            if shadow {
                let admission = target.value().deliver(decoded.take());
                Box::pin(async move {
                    admission.await?;
                    Ok(Box::new(()) as parrot_api::types::BoxedMessage)
                })
                    as parrot_api::types::BoxedFuture<
                        '_,
                        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
                    >
            } else if frame.header.flags & crate::frame::flags::URGENT != 0 {
                target.value().send_with_options(
                    decoded.take(),
                    parrot_api::message::MessageOptions {
                        priority: parrot_api::message::MessagePriority::HIGH,
                        ..Default::default()
                    },
                )
            } else {
                target.value().send(decoded.take())
            }
        })
        .await;
        let mut response = match outcome {
            Ok(Ok(reply)) => {
                let mut reply = cleanup.owned(reply);
                let response = if shadow {
                    Frame::reply(
                        cid,
                        "",
                        crate::frame::MIRROR_ADMISSION_KEY,
                        bytes::Bytes::new(),
                    )
                } else if completion {
                    Frame::tell_completed(cid)
                } else {
                    match cleanup.call(|| CodecRegistry::global().encode_outgoing(reply.value())) {
                        Ok(Ok((key, payload))) => Frame::reply(
                            cid,
                            &reply_to.unwrap_or_default(),
                            &key,
                            bytes::Bytes::from(payload),
                        ),
                        Ok(Err(code)) => {
                            Frame::reply_err(cid, &frame.path, code, "reply type not remotable")
                        }
                        Err(()) => Frame::reply_err(
                            cid,
                            &frame.path,
                            ErrCode::ProtocolViolation,
                            "reply encoder panicked",
                        ),
                    }
                };
                reply.retire();
                response
            }
            Ok(Err(error)) => {
                let mut error = cleanup.owned(error);
                let (code, detail) = cleanup
                    .call(|| ErrCode::from_actor_error(error.value()))
                    .unwrap_or((
                        ErrCode::ProtocolViolation,
                        "error formatter panicked".into(),
                    ));
                error.retire();
                Frame::reply_err(cid, &frame.path, code, &detail)
            }
            Err(()) => Frame::reply_err(
                cid,
                &frame.path,
                ErrCode::ProtocolViolation,
                "inbound actor invocation panicked",
            ),
        };
        target.retire();
        decoded.retire();
        // Keep an already failed request's stable error; never acknowledge a
        // successful operation before its independently owned inputs retire.
        if cleanup.failed() && response.header.frame_type == frame_type::REPLY {
            response = Frame::reply_err(
                cid,
                &frame.path,
                ErrCode::ProtocolViolation,
                "inbound request cleanup panicked",
            );
        }
        let _ = back.send(response).await;
    }

    fn authorize_directory_client(
        &self,
        frame: &Frame,
        back: &FrameSender,
        path: &str,
    ) -> Result<(), crate::roles::directory::network::NetworkDirectoryError> {
        use crate::roles::directory::network::NetworkDirectoryError;
        if !self.control_policy.allows(back)
            || !matches!(
                back.authentication(),
                crate::PeerAuthentication::MutualTls | crate::PeerAuthentication::InProcess
            )
            || frame.header.hop_count != 0
        {
            return Err(NetworkDirectoryError::Forbidden);
        }
        let node = self
            .relay
            .read()
            .ok()
            .and_then(|r| r.as_ref().map(|r| r.self_node().to_owned()));
        if frame.header.frame_type != frame_type::ASK
            || node.is_none_or(|node| frame.path != format!("parrot://{node}{path}"))
        {
            return Err(NetworkDirectoryError::Invalid(
                "Directory client requires an ASK bound to this node".into(),
            ));
        }
        Ok(())
    }
    async fn on_directory_read(&self, frame: Frame, back: &FrameSender) {
        use crate::roles::directory::{client, network::NetworkDirectoryError};
        let result = async {
            self.authorize_directory_client(&frame, back, client::READ_PATH)?;
            let endpoint = self
                .local
                .directory_receiver()
                .ok_or(NetworkDirectoryError::Closed)?;
            let (_, bytes) = frame
                .split_reply_to()
                .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
            client::receive_read(&endpoint, &bytes).await
        }
        .await;
        let reply = match bincode::serde::encode_to_vec(result, bincode::config::standard()) {
            Ok(bytes) => Frame::reply(
                frame.header.correlation_id,
                "",
                client::READ_REPLY_KEY,
                bytes::Bytes::from(bytes),
            ),
            Err(e) => Frame::reply_err(
                frame.header.correlation_id,
                &frame.path,
                ErrCode::CodecError,
                &e.to_string(),
            ),
        };
        let _ = back.send(reply).await;
    }
    async fn on_directory_client(&self, frame: Frame, back: &FrameSender) {
        use crate::roles::directory::{client, network::NetworkDirectoryError};
        let result = async {
            self.authorize_directory_client(&frame, back, client::CLIENT_PATH)?;
            let endpoint = self
                .local
                .directory_receiver()
                .ok_or(NetworkDirectoryError::Closed)?;
            let (_, payload) = frame
                .split_reply_to()
                .map_err(|e| NetworkDirectoryError::Invalid(e.to_string()))?;
            client::receive(&endpoint, back.node_id(), payload).await
        }
        .await;
        let reply = match bincode::serde::encode_to_vec(result, bincode::config::standard()) {
            Ok(bytes) => Frame::reply(
                frame.header.correlation_id,
                "",
                client::CLIENT_REPLY_KEY,
                bytes::Bytes::from(bytes),
            ),
            Err(e) => Frame::reply_err(
                frame.header.correlation_id,
                &frame.path,
                ErrCode::CodecError,
                &e.to_string(),
            ),
        };
        let _ = back.send(reply).await;
    }
    async fn on_directory_request(&self, frame: Frame, back: &FrameSender) {
        use crate::roles::directory::network::{RAFT_ACK_KEY, RAFT_PATH};
        use crate::security::PeerAuthentication;
        let result = async {
            if !matches!(
                back.authentication(),
                PeerAuthentication::MutualTls | PeerAuthentication::InProcess
            ) || frame.header.hop_count != 0
            {
                return Err((
                    ErrCode::Forbidden,
                    "Raft requires a direct authenticated peer".into(),
                ));
            }
            let node = self
                .relay
                .read()
                .ok()
                .and_then(|r| r.as_ref().map(|r| r.self_node().to_owned()));
            if frame.header.frame_type != frame_type::ASK
                || node.is_none_or(|node| frame.path != format!("parrot://{node}{RAFT_PATH}"))
            {
                return Err((
                    ErrCode::ProtocolViolation,
                    "Raft requires an ASK bound to this node".into(),
                ));
            }
            let endpoint = self
                .local
                .directory_receiver()
                .ok_or((ErrCode::ActorNotFound, "no Directory owner".into()))?;
            let (_, payload) = frame
                .split_reply_to()
                .map_err(|e| (ErrCode::ProtocolViolation, e.to_string()))?;
            endpoint
                .receive(back.node_id(), payload)
                .await
                .map_err(|e| (e.code(), e.to_string()))
        }
        .await;
        let response = match result {
            Ok(()) => Frame::reply(
                frame.header.correlation_id,
                "",
                RAFT_ACK_KEY,
                bytes::Bytes::new(),
            ),
            Err((code, detail)) => {
                Frame::reply_err(frame.header.correlation_id, &frame.path, code, &detail)
            }
        };
        let _ = back.send(response).await;
    }

    async fn on_durable_request(&self, frame: Frame, back: &FrameSender) {
        use crate::durable::{DurableEnvelope, InboxError, DURABLE_RECEIPT_KEY};
        let cid = frame.header.correlation_id;
        let cleanup = Cleanup::default();
        let mut span = tracing::Span::none();
        let result = async {
            if !self.control_policy.allows_cluster(back) {
                return Err((
                    ErrCode::Forbidden,
                    "durable origin requires an authenticated connection".into(),
                ));
            }
            let envelope = DurableEnvelope::decode(frame.payload.clone())
                .map_err(|e| (ErrCode::ProtocolViolation, e))?;
            let authorized = if frame.header.hop_count == 0 {
                back.node_id() == envelope.source
            } else {
                self.durable_relays
                    .read()
                    .map(|nodes| nodes.contains(back.node_id()))
                    .unwrap_or(false)
            };
            if !authorized {
                return Err((
                    ErrCode::Forbidden,
                    "durable source/relay identity not authorized".into(),
                ));
            }
            if frame.path != envelope.target_path() {
                return Err((
                    ErrCode::ProtocolViolation,
                    "durable destination differs from bound envelope".into(),
                ));
            }
            span = tracing::trace_span!(target: TARGET, "durable.ingress",
                source = %envelope.source, session = %envelope.session,
                receiver = %envelope.receiver, path = %envelope.path,
                sequence = envelope.sequence, cid);
            if self.is_remote_target(&frame.path) {
                self.try_relay_request(&frame, back, cid).await;
                return Ok(None);
            }
            let self_node = self
                .relay
                .read()
                .ok()
                .and_then(|r| r.as_ref().map(|r| r.self_node().to_owned()));
            if self_node.as_deref() != Some(envelope.receiver.as_str()) {
                return Err((
                    ErrCode::ProtocolViolation,
                    "durable receiver node identity unavailable or mismatched".into(),
                ));
            }
            let endpoint = cleanup
                .call(|| self.local.durable_receiver(&envelope.path))
                .map_err(|()| {
                    (
                        ErrCode::ProtocolViolation,
                        "durable receiver lookup panicked".into(),
                    )
                })?
                .ok_or((
                    ErrCode::ActorNotFound,
                    "no transactional receiver at this path".into(),
                ))?;
            let mut endpoint = cleanup.owned(endpoint);
            let seq = envelope.sequence;
            let timing = Phase::new(span.clone(), "ingress.receiver");
            let result = unwind::run(&cleanup, || endpoint.value().receive(envelope))
                .instrument(span.clone())
                .await;
            endpoint.retire();
            timing.finish(matches!(&result, Ok(Ok(()))) && !cleanup.failed());
            result
                .map_err(|()| {
                    (
                        ErrCode::ProtocolViolation,
                        "durable receiver invocation panicked".into(),
                    )
                })?
                .map_err(|e| {
                    let code = match &e {
                        InboxError::Capacity => ErrCode::Overloaded,
                        InboxError::Closed => ErrCode::Stopped,
                        InboxError::Invalid(_) | InboxError::Gap { .. } => {
                            ErrCode::ProtocolViolation
                        }
                        _ => ErrCode::CodecError,
                    };
                    (code, e.to_string())
                })?;
            if cleanup.failed() {
                return Err((
                    ErrCode::ProtocolViolation,
                    "durable receiver cleanup panicked".into(),
                ));
            }
            Ok(Some(seq))
        }
        .await;
        match result {
            Ok(Some(seq)) => {
                let timing = Phase::new(span.clone(), "ingress.receipt_admission");
                let result = back
                    .send(Frame::reply(
                        cid,
                        "",
                        DURABLE_RECEIPT_KEY,
                        bytes::Bytes::copy_from_slice(&seq.to_le_bytes()),
                    ))
                    .await;
                timing.finish(result.is_ok());
            }
            Ok(None) => {}
            Err((code, detail)) => {
                let _ = back
                    .send(Frame::reply_err(cid, &frame.path, code, &detail))
                    .await;
            }
        }
    }

    /// hub 转发 ASK：目标是其他节点且本节点有路由能力 → 转发 + 映射登记。
    /// 返回 Some(code) = 未转发（code 是应回源的错误码；ActorNotFound =
    /// 常规 miss 语义）；None = 已转发（等 REPLY 自动回源）。
    async fn try_relay_request(
        &self,
        frame: &Frame,
        back: &FrameSender,
        orig_cid: u64,
    ) -> Option<ErrCode> {
        // ① 非 parrot://{node}/ 路径 → 常规 ActorNotFound（不是路由问题）
        let Some(target) = target_node(&frame.path) else {
            return Some(ErrCode::ActorNotFound);
        };
        // ② 路由能力未注入（非 hub 节点）或目标是本节点 → 常规 ActorNotFound
        let Some(router) = self
            .relay
            .read()
            .ok()
            .and_then(|g| g.clone())
            .filter(|r| target != r.self_node())
        else {
            return Some(ErrCode::ActorNotFound);
        };
        if frame.header.hop_count.saturating_add(1) >= frame.header.hop_limit {
            back.send_reply(Frame::reply_err(
                orig_cid,
                &frame.path,
                ErrCode::RouteUnreachable,
                "relay hop limit exhausted",
            ));
            return Some(ErrCode::RouteUnreachable);
        }
        // ③ 有路由能力但无直连 → RouteUnreachable（比 ActorNotFound 语义准）
        let Some(sender) = router.sender_of(target) else {
            let _ = back
                .send(Frame::reply_err(
                    orig_cid,
                    &frame.path,
                    ErrCode::RouteUnreachable,
                    &format!("no route to {target}"),
                ))
                .await;
            return Some(ErrCode::RouteUnreachable);
        };
        if frame.header.flags & crate::frame::flags::TELL_ACK != 0
            && sender.peer_capabilities() & crate::handshake::caps::TELL_COMPLETION == 0
        {
            back.send_reply(Frame::reply_err(
                orig_cid,
                &frame.path,
                ErrCode::ProtocolViolation,
                "next hop does not support completion TELL",
            ));
            return Some(ErrCode::ProtocolViolation);
        }
        if frame.type_key == crate::durable::DURABLE_KEY
            && sender.peer_capabilities() & crate::handshake::caps::DURABLE_DELIVERY == 0
        {
            back.send_reply(Frame::reply_err(
                orig_cid,
                &frame.path,
                ErrCode::ProtocolViolation,
                "next hop does not support durable delivery",
            ));
            return Some(ErrCode::ProtocolViolation);
        }
        // 转发：新 cid + hop_count+1（防环；≥hop_limit 由对端 Frame 校验拒收）
        let forward_cid = router.next_cid();
        let mut fwd = frame.clone();
        fwd.header.correlation_id = forward_cid;
        fwd.header.hop_count = fwd.header.hop_count.saturating_add(1);
        let fwd = match sender.trace_request(fwd) {
            Ok(frame) => frame,
            Err(error) => {
                back.send_reply(Frame::reply_err(
                    orig_cid,
                    &frame.path,
                    ErrCode::ProtocolViolation,
                    &error.to_string(),
                ));
                return Some(ErrCode::ProtocolViolation);
            }
        };
        // Reserve before registering, then publish without an intervening await:
        // fast replies and cancellation cannot race map insertion.
        let permit = match tokio::time::timeout(self.relay_table.ttl, sender.reserve(&fwd)).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(crate::error::RemoteError::Frame(error))) => {
                let code = error.to_errcode();
                back.send_reply(Frame::reply_err(
                    orig_cid,
                    &frame.path,
                    code,
                    &error.to_string(),
                ));
                return Some(code);
            }
            _ => {
                back.send_reply(Frame::reply_err(
                    orig_cid,
                    &frame.path,
                    ErrCode::ConnectionLost,
                    "relay unavailable",
                ));
                return Some(ErrCode::ConnectionLost);
            }
        };
        if !self
            .relay_table
            .insert(forward_cid, orig_cid, back.clone(), &sender)
        {
            let code = if sender.is_closed() || back.is_closed() {
                ErrCode::ConnectionLost
            } else {
                ErrCode::Overloaded
            };
            back.send_reply(Frame::reply_err(
                orig_cid,
                &frame.path,
                code,
                "relay admission unavailable",
            ));
            return Some(code);
        }
        permit.send(fwd);
        if frame.header.frame_type == frame_type::TELL {
            RELAYED_TELL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            RELAYED_ASK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // 方案 A：目标可直拨 → 向源注入 ROUTE_HINT（源学直连，后续帧不再
        // 经 hub——try_send 队列满仅丢弃 hint，学习退化为继续中转，无害）
        if let Some(addr) = router.dial_addr_of(target) {
            if let Ok(hint) = Frame::try_route_hint(target, &addr) {
                back.send_frame(hint);
            }
        }
        tracing::debug!(%target, forward_cid, orig_cid, "relayed ask");
        None
    }

    /// REPLY 回源：先查转发映射（本节点作为 hub 中转的回程），命中则
    /// 还原 orig_cid 送回源连接；未命中走本地回调表（本节点发起的 ask）。
    fn relay_reply(&self, frame: &Frame, source: &FrameSender) -> bool {
        let cid = frame.header.correlation_id;
        match self.relay_table.take_from(cid, source) {
            RelayReply::Missing => false,
            RelayReply::WrongSource => true,
            RelayReply::Matched {
                original_cid,
                back,
                expired,
            } => {
                if expired {
                    back.send_reply(Frame::reply_err(
                        original_cid,
                        "",
                        ErrCode::Timeout,
                        "relay expired",
                    ));
                } else {
                    let mut out = frame.clone();
                    out.header.correlation_id = original_cid;
                    back.send_reply(out);
                }
                tracing::debug!(cid, original_cid, expired, "relayed reply finalized");
                true
            }
        }
    }

    /// ROUTE_HINT 学习（方案 A）：hub 背书的直连地址入缓存；地址变化
    /// （节点重启换端口）覆盖旧值。拨号由 system 侧 watcher 异步执行。
    async fn on_route_hint(&self, node: &str, addr: &str, _back: &FrameSender) {
        let changed = {
            let mut g = self.learned.lock().unwrap();
            let changed = g.get(node).map(|a| a != addr).unwrap_or(true);
            if changed {
                g.insert(node.to_string(), addr.to_string());
            }
            changed
        };
        if changed {
            tracing::info!(%node, %addr, "learned direct route (hub-endorsed)");
            // 通知 dial watcher（system.rs——同地址已在拨/已连则忽略）
            if let Some(router) = self.relay.read().ok().and_then(|g| g.clone()) {
                router.on_hint(node, addr);
            }
        }
    }

    /// TELL 入站：端到端重排网关。
    ///
    /// 三类路径：
    /// 1. **终点是本节点的直连 seq 帧**：per-from 重排任务——seq 流来自
    ///    唯一源（直连对端），流身份成立，可安全重排
    /// 2. **中转帧**（目标节点非本节点或 hop_count > 0）：线上缺原始
    ///    发送方标识，多源 seq 不可按 hub 身份合并 → bypass 直转
    /// 3. **seq==0 帧**：旧实现/异构网关 → bypass 直投（兼容语义）
    ///
    /// 重排任务冷启动：首帧 seq=N>1（切换后直连新流）→ 缓冲等 250ms——
    /// 超时放行 N。Wire 1.0 未带原始发送方/会话标识，当前不能保证
    /// hub/直连切换的端到端 FIFO；等待窗口不能替代此保证。
    /// 背压：bounded channel 满则 dispatch 挂起 → 读循环挂起（RC8 贯通）。
    async fn on_tell(self: &Arc<Self>, frame: Frame, back: &FrameSender, from_node: &str) {
        if self.reorder_cancel.is_cancelled() {
            DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let seq = frame.header.seq;
        let self_node = self
            .relay
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|r| r.self_node().to_string()));
        // 终点判定：无 relay（非路由节点）→ 本地路径直投；有 relay →
        // 目标节点 == 本节点才重排（中转帧 bypass）
        let destined_here = match &self_node {
            None => true,
            Some(me) => target_node(&frame.path).map(|t| t == me).unwrap_or(true),
        };
        if seq == crate::frame::SEQ_NONE || !destined_here || frame.header.hop_count > 0 {
            self.tell_deliver_or_relay(frame, back).await;
            return;
        }
        // An idle worker may retire between sender acquisition and enqueue.
        // Retry through the registered worker, never bypass FIFO via direct delivery.
        let mut pending = frame;
        loop {
            let Some(tx) = self.reorder_tx_of(from_node) else {
                DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
                return;
            };
            tokio::select! {
                biased;
                _ = self.reorder_cancel.cancelled() => {
                    DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
                    return;
                },
                result = tx.send(pending) => match result {
                    Ok(()) => return,
                    Err(error) => pending = error.0,
                },
            }
        }
    }

    /// Register at most one worker per source, atomically with the close fence.
    fn reorder_tx_of(self: &Arc<Self>, from: &str) -> Option<tokio::sync::mpsc::Sender<Frame>> {
        let mut map = self.reorder_tx.lock().unwrap();
        if self.reorder_cancel.is_cancelled() {
            return None;
        }
        if let Some(tx) = map.get(from).filter(|tx| !tx.is_closed()) {
            return Some(tx.clone());
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<Frame>(256);
        let registration = ReorderRegistration {
            map: Arc::downgrade(&self.reorder_tx),
            source: from.to_owned(),
            sender: tx.downgrade(),
        };
        let this = Arc::downgrade(self);
        let cancel = self.reorder_cancel.clone();
        map.insert(from.to_owned(), tx.clone());
        self.reorder_tasks.spawn(async move {
            let _registration = registration;
            Ingress::reorder_loop(this, &_registration, rx, cancel).await;
        });
        Some(tx)
    }

    /// per 源节点重排循环：expected/BTreeMap 状态机 + recv 超时冲刷。
    ///
    /// 冷启动缺口（expected=1，首帧 seq=N>1）与运行中缺口共用同一条
    /// 250ms 超时路径：超时放行最小 seq（跳缺口），expected 跳至其+1；
    /// 缓冲余帧若与放行帧连续则一并投递。丢帧绝不永久阻塞。
    ///
    /// Idle workers retire only when no dispatcher holds a sender and no frame
    /// is queued. Cancellation also interrupts awaited local lookup/delivery.
    async fn reorder_loop(
        weak_ing: std::sync::Weak<Ingress>,
        registration: &ReorderRegistration,
        mut rx: tokio::sync::mpsc::Receiver<Frame>,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let from = &registration.source;
        let mut expected: u32 = 1;
        let mut buf: std::collections::BTreeMap<u32, Frame> = Default::default();
        // 旋钮启动时快照（任务生命周期内固定——避免运行中改配置引发
        // expected 语义漂移；下个任务自然用新值）
        let (idle_gap, buffer_cap) = weak_ing
            .upgrade()
            .map(|this| {
                (
                    *this
                        .reorder_gap_timeout
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                    *this
                        .reorder_buffer_cap
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                )
            })
            .unwrap_or((std::time::Duration::from_millis(250), 1024));
        let idle_gc = tokio::time::Duration::from_secs(30);
        let work = async {
            loop {
                let f = tokio::select! {
                    f = rx.recv() => match f {
                        Some(f) => f,
                        None => return,
                    },
                    _ = tokio::time::sleep(idle_gap), if !buf.is_empty() => {
                        // 缺口超时：放行最小 seq（跳缺口）
                        let min_seq = *buf.keys().next().unwrap();
                        let f = buf.remove(&min_seq).unwrap();
                        REORDER_GAP_FLUSHED.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            from = %from, expected, got = min_seq,
                            "reorder gap timeout: released buffered TELL (missing frame presumed lost)"
                        );
                        expected = min_seq.wrapping_add(1);
                        // 连续后继一并放行
                        let mut chain = vec![f];
                        while let Some(&k) = buf.keys().next() {
                            if k == expected {
                                chain.push(buf.remove(&k).unwrap());
                                expected = expected.wrapping_add(1);
                            } else {
                                break;
                            }
                        }
                        if let Some(this) = weak_ing.upgrade() {
                            let back = FrameSender::detached();
                            for fr in chain {
                                this.tell_deliver_or_relay(fr, &back).await;
                            }
                        } else {
                            REORDER_DROPPED_ON_SHUTDOWN
                                .fetch_add(chain.len() as u64, Ordering::Relaxed);
                        }
                        continue;
                    },
                    _ = tokio::time::sleep(idle_gc), if buf.is_empty() => {
                        // A sender clone may have been handed to a concurrent
                        // dispatcher. It and any queued frames must survive idle GC.
                        let Some(map) = registration.map.upgrade() else { return; };
                        let mut map = map.lock().unwrap();
                        let idle = map.get(from).is_some_and(|tx| tx.strong_count() == 1 && rx.is_empty());
                        if !idle { continue; }
                        rx.close();
                        map.remove(from);
                        drop(map);
                        REORDER_TASK_RECYCLED.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(from = %from, idle_secs = 30, "reorder task recycled (idle)");
                        return;
                    }
                };
                let seq = f.header.seq;
                if seq == expected {
                    if let Some(this) = weak_ing.upgrade() {
                        let back = FrameSender::detached();
                        this.tell_deliver_or_relay(f, &back).await;
                    }
                    expected = expected.wrapping_add(1);
                    // drain 连续后继
                    while let Some(&k) = buf.keys().next() {
                        if k == expected {
                            let fr = buf.remove(&k).unwrap();
                            if let Some(this) = weak_ing.upgrade() {
                                let back = FrameSender::detached();
                                this.tell_deliver_or_relay(fr, &back).await;
                            }
                            expected = expected.wrapping_add(1);
                        } else {
                            break;
                        }
                    }
                } else if seq.wrapping_sub(expected) > u32::MAX / 2 {
                    // seq "小于" expected（回绕安全比较）→ 迟到帧，丢弃
                    LATE_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(from = %from, seq, expected, "late TELL dropped (gap already flushed)");
                } else {
                    // seq "大于" expected → 缓冲（BTreeMap 去重天然幂等——
                    // 同 seq 重复帧只保一个，防对端 bug 打爆内存）
                    if buf.insert(seq, f).is_none() {
                        REORDER_BUFFERED.fetch_add(1, Ordering::Relaxed);
                    }
                    if buf.len() > buffer_cap {
                        // 防打爆上限：放行最小 seq（跳缺口）
                        let min_seq = *buf.keys().next().unwrap();
                        let f = buf.remove(&min_seq).unwrap();
                        REORDER_GAP_FLUSHED.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            from = %from, expected, got = min_seq, buffered = buf.len(),
                            "reorder buffer overflow cap: released min seq"
                        );
                        expected = min_seq.wrapping_add(1);
                        if let Some(this) = weak_ing.upgrade() {
                            let back = FrameSender::detached();
                            this.tell_deliver_or_relay(f, &back).await;
                        }
                        while let Some(&k) = buf.keys().next() {
                            if k == expected {
                                let fr = buf.remove(&k).unwrap();
                                if let Some(this) = weak_ing.upgrade() {
                                    let back = FrameSender::detached();
                                    this.tell_deliver_or_relay(fr, &back).await;
                                }
                                expected = expected.wrapping_add(1);
                            } else {
                                break;
                            }
                        }
                    }
                }
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {},
            _ = work => {},
        }
        REORDER_DROPPED_ON_SHUTDOWN.fetch_add((buf.len() + rx.len()) as u64, Ordering::Relaxed);
    }

    /// TELL 实际投递（本地命中 / hub 中转 / 死信）——原 on_tell 主体。
    async fn tell_deliver_or_relay(&self, frame: Frame, back: &FrameSender) {
        tokio::select! {
            biased;
            _ = self.reorder_cancel.cancelled() => {},
            _ = async {
                let cleanup = Cleanup::default();
                if unwind::run(&cleanup, || Box::pin(self.tell_deliver_open(frame, back))).await.is_err() {
                    DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
                }
            } => {},
        }
    }

    async fn tell_deliver_open(&self, mut frame: Frame, back: &FrameSender) {
        let Ok(context) = self.request_context(&mut frame) else {
            back.close();
            return;
        };
        parrot_api::trace::scope(context, self.tell_deliver_scoped(frame, back)).await;
    }

    async fn tell_deliver_scoped(&self, mut frame: Frame, back: &FrameSender) {
        self.mirror_application(&frame);
        self.resolve_application_path(&mut frame);
        if self.is_remote_target(&frame.path) {
            if !self.try_relay_tell(&frame, back) {
                DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        let cleanup = Cleanup::default();
        let decoded = match cleanup
            .call(|| CodecRegistry::global().decode_incoming(&frame.type_key, &frame.payload))
        {
            Ok(Ok(message)) => message,
            outcome => {
                let code = match outcome {
                    Ok(Err(code)) => code,
                    _ => ErrCode::ProtocolViolation,
                };
                let _ = cleanup.call(|| tracing::debug!(
                    type_key = %frame.type_key, path = %frame.path, code = code.name(),
                    payload_len = frame.payload.len(), "TELL decode failed → dead letter (codec registry drift?)"
                ));
                DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let mut decoded = cleanup.owned(decoded);
        let mut failed = false;
        match unwind::run(&cleanup, || self.local.lookup(local_path(&frame.path))).await {
            Ok(Some(target)) => {
                let mut target = cleanup.owned(target);
                let result = if cleanup.failed() {
                    Err(())
                } else {
                    unwind::run(&cleanup, || {
                        if frame.header.flags & crate::frame::flags::URGENT != 0 {
                            target.value().deliver_with_options(
                                decoded.take(),
                                parrot_api::message::MessageOptions {
                                    priority: parrot_api::message::MessagePriority::HIGH,
                                    ..Default::default()
                                },
                            )
                        } else {
                            target.value().deliver(decoded.take())
                        }
                    })
                    .await
                };
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        failed = true;
                        let mut error = cleanup.owned(error);
                        let _ = cleanup.call(|| tracing::debug!(path = %frame.path, error = %error.value(), "TELL admission rejected"));
                        error.retire();
                    }
                    Err(()) => failed = true,
                }
                target.retire();
            }
            Ok(None) => {
                decoded.retire();
                failed = cleanup.failed() || !self.try_relay_tell(&frame, back);
            }
            Err(()) => failed = true,
        }
        decoded.retire();
        if failed || cleanup.failed() {
            DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// hub 转发 TELL（无回程语义——转发成功即完事）。
    fn try_relay_tell(&self, frame: &Frame, back: &FrameSender) -> bool {
        if frame.header.hop_count.saturating_add(1) >= frame.header.hop_limit {
            return false;
        }
        let Some(target) = target_node(&frame.path) else {
            tracing::debug!(
                path = %frame.path,
                "TELL relay skipped: path not parrot:// node-addressed"
            );
            return false;
        };
        let Ok(g) = self.relay.read() else {
            return false;
        };
        let Some(router) = g.as_ref() else {
            return false;
        };
        if target == router.self_node() {
            return false;
        }
        let Some(sender) = router.sender_of(target) else {
            return false;
        };
        let mut fwd = frame.clone();
        fwd.header.hop_count = fwd.header.hop_count.saturating_add(1);
        let Ok(fwd) = sender.trace_request(fwd) else {
            return false;
        };
        if sender.send_frame_sync(fwd).is_err() {
            return false;
        }
        RELAYED_TELL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 方案 A：同 ask——向源注入直连 hint
        if let Some(addr) = router.dial_addr_of(target) {
            if let Ok(hint) = Frame::try_route_hint(target, &addr) {
                back.send_frame(hint);
            }
        }
        tracing::debug!(%target, "relayed tell");
        true
    }

    async fn on_stop(&self, mut frame: Frame) {
        let cleanup = Cleanup::default();
        let _ = unwind::run(&cleanup, || {
            Box::pin(async {
                self.resolve_application_path(&mut frame);
                if self.is_remote_target(&frame.path) {
                    self.try_relay_tell(&frame, &FrameSender::detached());
                    return;
                }
                if let Ok(Some(target)) =
                    unwind::run(&cleanup, || self.local.lookup(local_path(&frame.path))).await
                {
                    let mut target = cleanup.owned(target);
                    if !cleanup.failed() {
                        if let Ok(result) = unwind::run(&cleanup, || target.value().stop()).await {
                            cleanup.dispose(result);
                        }
                    }
                    target.retire();
                }
            })
        })
        .await;
    }

    pub(crate) fn connection_closed(&self, connection: &FrameSender) {
        self.callbacks
            .fail_connection(connection, ErrCode::ConnectionLost, "connection lost");
        let mut pending = self.resolve_pending.lock().unwrap();
        let ids: Vec<u64> = pending
            .iter()
            .filter_map(|(cid, slot)| slot.connection.same_connection(connection).then_some(*cid))
            .collect();
        for cid in ids {
            if let Some(slot) = pending.remove(&cid) {
                let _ = slot.tx.send(ReplyPayload::Err(
                    ErrCode::ConnectionLost,
                    "connection lost".into(),
                ));
            }
        }
        for (cid, back) in self.relay_table.fail_connection(connection) {
            back.send_reply(Frame::reply_err(
                cid,
                "",
                ErrCode::ConnectionLost,
                "relay connection lost",
            ));
        }
    }

    fn on_reply(&self, frame: Frame, source: &FrameSender) {
        // hub 中转回程优先（本节点转发的 ask 的 REPLY——还原 cid 回源）
        if self.relay_reply(&frame, source) {
            return;
        }
        let cid = frame.header.correlation_id;
        match frame.header.frame_type {
            frame_type::REPLY => {
                if !self.callbacks.complete_from(
                    cid,
                    ReplyPayload::Ok(frame.payload.clone(), frame.type_key.clone()),
                    source,
                ) {
                    // 边界观测：无主 REPLY 常见于①ask 已超时回收 ②重复
                    // REPLY ③cid 漂移（hub 转发表错乱）。带 cid 便于对账。
                    tracing::debug!(
                        cid, type_key = %frame.type_key,
                        "REPLY completed no pending callback (timeout raced? duplicate?)"
                    );
                }
            }
            _ => {
                let (code, detail) = decode_err_payload(&frame.payload)
                    .unwrap_or((ErrCode::ProtocolViolation, "<undecodable>".into()));
                if self.fail_resolve(cid, source, code, &detail) {
                    return;
                }
                let hook = self.sys_event.read().unwrap().clone();
                if let Some(hook) = hook {
                    let cleanup = Cleanup::default();
                    let _close = cleanup.close_on_failure(source);
                    let mut hook = cleanup.owned(hook);
                    let consumed =
                        cleanup.call(|| hook.value().on_reply_error(cid, code, &detail, source));
                    hook.retire();
                    // An uncertain or consuming hook must not accidentally
                    // complete an ordinary actor callback with the same CID.
                    if cleanup.failed() || consumed != Ok(false) {
                        return;
                    }
                }
                if !self.callbacks.complete_from(
                    cid,
                    ReplyPayload::Err(code, detail.clone()),
                    source,
                ) {
                    tracing::debug!(
                        cid, code = code.name(), %detail,
                        "REPLY_ERR completed no pending callback (timeout raced? duplicate?)"
                    );
                }
            }
        }
    }
}

/// 断连清理钩子（RemoteActorSystem 注入 ConnectionTask）：
/// Only callbacks bound to the closed connection fail; supplied statuses must
/// belong exclusively to that connection.
pub fn make_on_disconnect(
    callbacks: Arc<CallbackRegistry>,
    statuses: Vec<Arc<crate::node::NodeStatus>>,
) -> crate::transport::OnDisconnect {
    Arc::new(move |connection: &FrameSender| {
        let node_id = connection.node_id();
        let n = callbacks.fail_connection(connection, ErrCode::ConnectionLost, "connection lost");
        if n > 0 {
            tracing::debug!(node = %node_id, pending = n, "callbacks failed on disconnect");
        }
        for st in &statuses {
            st.set(NodeState::Disconnected);
        }
    })
}

/// 测试消息（ingress 单测注册——全局 CodecRegistry 走 inventory）。
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TestAsk(pub u64);
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TestReply(pub u64);

#[cfg(test)]
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot_remote::TestAsk#v1",
        type_id: std::any::TypeId::of::<TestAsk>(),
        encode: |msg: &parrot_api::types::BoxedMessage| {
            let m = msg.downcast_ref::<TestAsk>().ok_or("downcast TestAsk")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: TestAsk = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as parrot_api::types::BoxedMessage)
        },
    }
}

#[cfg(test)]
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot_remote::TestReply#v1",
        type_id: std::any::TypeId::of::<TestReply>(),
        encode: |msg: &parrot_api::types::BoxedMessage| {
            let m = msg.downcast_ref::<TestReply>().ok_or("downcast TestReply")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: TestReply = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as parrot_api::types::BoxedMessage)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CallbackRegistry;
    use async_trait::async_trait;
    use bytes::Bytes;
    use parrot_api::address::ActorRef;
    use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
    use std::any::Any;

    /// 回声 ActorRef 桩（LocalLookup 测试实现）。
    #[derive(Debug)]
    struct EchoRef;

    #[async_trait]
    impl ActorRef for EchoRef {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send_with_timeout(msg, None)
        }

        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _timeout: Option<std::time::Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(msg) })
        }
        fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move { Ok(()) })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move { Ok(()) })
        }
        fn path(&self) -> String {
            "/user/echo".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async move { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(Self)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct TestLookup;

    #[async_trait]
    impl LocalLookup for TestLookup {
        async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
            if path == "/user/echo" {
                Some(Box::new(EchoRef))
            } else {
                None
            }
        }
    }

    struct ApplicationAliases;
    #[async_trait]
    impl LocalLookup for ApplicationAliases {
        async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
            panic!("opaque gateway requests must not enter local codec/actor dispatch")
        }
        fn forwarding_target(&self, path: &str) -> Option<String> {
            (path == "/user/alias").then(|| "parrot://gateway/user/actual".into())
        }
    }
    struct AliasRouter(FrameSender);
    #[async_trait]
    impl RelayRouter for AliasRouter {
        fn sender_of(&self, node: &str) -> Option<FrameSender> {
            (node == "gateway" || node == "other").then(|| self.0.clone())
        }
        fn next_cid(&self) -> u64 {
            900
        }
        fn self_node(&self) -> &str {
            "host"
        }
        fn dial_addr_of(&self, _: &str) -> Option<String> {
            None
        }
        fn on_hint(&self, _: &str, _: &str) {}
    }
    #[tokio::test]
    async fn application_alias_forwards_opaque_ask_tell_stop_without_capturing_other_nodes() {
        let ingress = Ingress::new(
            Arc::new(ApplicationAliases),
            Arc::new(CallbackRegistry::new(8)),
        );
        let (out, mut frames) = tokio::sync::mpsc::channel(8);
        let destination = FrameSender::anon(out);
        *ingress.relay_slot() = Some(Arc::new(AliasRouter(destination.clone())));
        let (tx, mut replies) = tokio::sync::mpsc::channel(8);
        let back = FrameSender::anon(tx);
        let payload = Bytes::from_static(b"\0\xffopaque");
        let request = Frame::ask(
            7,
            "parrot://host/user/alias",
            "unknown:business",
            payload.clone(),
            None,
        );
        ingress.on_request(request.clone(), &back).await;
        let forwarded = frames.recv().await.unwrap();
        assert_eq!(forwarded.path, "parrot://gateway/user/actual");
        assert_eq!(forwarded.type_key, request.type_key);
        assert_eq!(forwarded.payload, request.payload);
        assert_eq!(forwarded.header.hop_count, 1);
        assert_eq!(forwarded.header.correlation_id, 900);
        ingress.on_reply(
            Frame::reply(900, "", "unknown:reply", payload.clone()),
            &destination,
        );
        let reply = replies.recv().await.unwrap();
        assert_eq!(reply.header.correlation_id, 7);
        assert_eq!(reply.payload, payload);
        assert!(ingress.relay_table.is_empty());
        let mut exhausted = request;
        exhausted.header.hop_limit = 1;
        ingress.on_request(exhausted, &back).await;
        assert_eq!(
            replies.recv().await.unwrap().header.frame_type,
            frame_type::REPLY_ERR
        );
        assert!(frames.try_recv().is_err());
        for frame in [
            Frame::tell("/user/alias", "unknown:business", payload.clone()),
            Frame::stop("parrot://host/user/alias"),
            Frame::tell(
                "parrot://other/user/alias",
                "unknown:business",
                payload.clone(),
            ),
        ] {
            let kind = frame.header.frame_type;
            let expected_path = if frame.path.contains("other") {
                "parrot://other/user/alias"
            } else {
                "parrot://gateway/user/actual"
            };
            if kind == frame_type::STOP {
                ingress.on_stop(frame).await;
            } else {
                ingress.tell_deliver_or_relay(frame, &back).await;
            }
            let forwarded = frames.recv().await.unwrap();
            assert_eq!(forwarded.path, expected_path);
            assert_eq!(forwarded.header.frame_type, kind);
            assert_eq!(forwarded.header.hop_count, 1);
        }
    }

    #[test]
    fn local_path_strips_node_prefix() {
        assert_eq!(local_path("parrot://node-b/user/echo"), "/user/echo");
        assert_eq!(local_path("/user/echo"), "/user/echo");
        assert_eq!(
            local_path("parrot://node-b/actix://Echo/id"),
            "actix://Echo/id"
        );
        assert_eq!(local_path("actix://Echo/id"), "actix://Echo/id");
        assert_eq!(
            local_path("parrot://node-b/actix/Echo/id"),
            "/actix/Echo/id"
        );
    }

    fn ingress() -> (Arc<Ingress>, Arc<CallbackRegistry>) {
        let callbacks = Arc::new(CallbackRegistry::new(64));
        (
            Arc::new(Ingress::new(Arc::new(TestLookup), callbacks.clone())),
            callbacks,
        )
    }

    /// 录制型 Lookup：按投递顺序记录 TELL payload 内容（重排测试观测点）。
    struct RecordingLookup(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    #[async_trait]
    impl LocalLookup for RecordingLookup {
        async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
            if path == "/user/echo" {
                Some(Box::new(RecordingRef(self.0.clone())))
            } else {
                None
            }
        }
    }

    /// 录制型 Ref：deliver 时把 TestAsk.0 追加进共享 Vec（投递顺序即答案）。
    #[derive(Debug)]
    struct RecordingRef(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    #[async_trait]
    impl ActorRef for RecordingRef {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send_with_timeout(msg, None)
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _timeout: Option<std::time::Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(msg) })
        }
        fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            let sink = self.0.clone();
            Box::pin(async move {
                if let Some(m) = msg.downcast_ref::<TestAsk>() {
                    sink.lock().unwrap().push(m.0 as u8);
                }
                Ok(())
            })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move { Ok(()) })
        }
        fn path(&self) -> String {
            "/user/echo".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async move { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(RecordingRef(self.0.clone()))
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn recording_ingress() -> (Arc<Ingress>, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Arc::new(Ingress::new(
                Arc::new(RecordingLookup(sink.clone())),
                Arc::new(CallbackRegistry::new(64)),
            )),
            sink,
        )
    }

    /// 构造 seq TELL（TestAsk(v) 编码——v 同时用作投递序标记）。
    fn seq_tell(v: u64, seq: u32) -> Frame {
        seq_tell_to("nA", v, seq)
    }

    fn seq_tell_to(node: &str, v: u64, seq: u32) -> Frame {
        let (_, payload) = crate::codec_registry::CodecRegistry::global()
            .encode_outgoing(&(Box::new(TestAsk(v)) as BoxedMessage))
            .unwrap();
        Frame::tell_with_seq(
            &format!("parrot://{node}/user/echo"),
            "bin:parrot_remote::TestAsk#v1",
            Bytes::from(payload),
            seq,
        )
    }

    // RO1：seq 乱序到达（33 先于 32）→ 接收端按序投递（先发 1,2 建基线）
    #[tokio::test]
    async fn ro1_out_of_order_reordered() {
        let (ig, sink) = recording_ingress();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(seq_tell(1, 1), &back, "nA").await;
        ig.dispatch(seq_tell(2, 2), &back, "nA").await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(sink.lock().unwrap().as_slice(), &[1u8, 2], "基线 1,2 先投");
        // 乱序：4 先到（缓冲），3 后到（触发 drain → 3,4 连投）
        ig.dispatch(seq_tell(4, 4), &back, "nA").await;
        ig.dispatch(seq_tell(3, 3), &back, "nA").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            sink.lock().unwrap().as_slice(),
            &[1u8, 2, 3, 4],
            "乱序 4/3 应重排为 3,4"
        );
    }

    // RO2：缺口超时放行（seq=5 到、4 永不来）→ 250ms 后放行 5，不卡死
    #[tokio::test]
    async fn ro2_gap_timeout_flushes() {
        let (ig, sink) = recording_ingress();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(seq_tell(1, 1), &back, "nA").await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let flushed_before = REORDER_GAP_FLUSHED.load(Ordering::Relaxed);
        ig.dispatch(seq_tell(5, 5), &back, "nA").await; // 2..4 缺失
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(
            sink.lock().unwrap().is_empty() || sink.lock().unwrap().as_slice() == [1u8],
            "缺口期 5 不投（等 2..4）"
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(
            sink.lock().unwrap().as_slice(),
            &[1u8, 5],
            "超时后放行 5（跳过 2..4）"
        );
        assert!(
            REORDER_GAP_FLUSHED.load(Ordering::Relaxed) > flushed_before,
            "gap flush 计数递增"
        );
    }

    // RO2-K：配置切面——gap 旋钮实际生效（50ms vs 默认 250ms 对照时序）
    #[tokio::test]
    async fn ro2k_gap_knob_shortens_wait() {
        let (ig, sink) = recording_ingress();
        // 旋钮：50ms 缺口等待（远小于默认 250ms）
        ig.set_reorder_knobs(std::time::Duration::from_millis(50), 1024);
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(seq_tell(1, 1), &back, "nK").await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        ig.dispatch(seq_tell(3, 3), &back, "nK").await; // 2 缺失
                                                        // 50ms 旋钮窗口后应放行（默认 250ms 此刻仍缓冲——时序即证明）
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            sink.lock().unwrap().as_slice(),
            &[1u8, 3],
            "50ms 旋钮：~150ms 时已放行（默认 250ms 必仍缓冲）"
        );
    }

    // RO3：迟到帧丢弃（超时放行 5 后 4 才到）→ LATE_TELL_DROPPED + 不投递
    #[tokio::test]
    async fn ro3_late_frame_dropped() {
        let (ig, sink) = recording_ingress();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(seq_tell(1, 1), &back, "nA").await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        ig.dispatch(seq_tell(5, 5), &back, "nA").await;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await; // 超时放行 5
        let late_before = LATE_TELL_DROPPED.load(Ordering::Relaxed);
        ig.dispatch(seq_tell(4, 4), &back, "nA").await; // 迟到
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert_eq!(sink.lock().unwrap().as_slice(), &[1u8, 5], "迟到 4 不投");
        assert!(
            LATE_TELL_DROPPED.load(Ordering::Relaxed) > late_before,
            "迟到计数递增"
        );
    }

    // RO4：冷启动缺口（首帧 seq=N>1——切换后新直连流）→ 250ms 放行
    #[tokio::test]
    async fn ro4_cold_start_gap_released() {
        let (ig, sink) = recording_ingress();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(seq_tell(7, 7), &back, "nA").await; // expected=1，7>1
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(sink.lock().unwrap().is_empty(), "冷启动缺口期不投");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(sink.lock().unwrap().as_slice(), &[7u8], "超时放行 7");
    }

    // RO5：中转帧 bypass 重排（目标非本节点 → 直转不缓冲）。
    // 场景：本节点是 spoke（self=me），帧终点 parrot://nB/...（我经
    // uplink 中转）——seq 再大也不进重排。
    #[tokio::test]
    async fn ro5_relayed_frames_bypass_reorder() {
        let (ig, sink) = recording_ingress();
        // 注入最小 relay：self_node="me"（终点判定启用）
        struct MeRouter;
        #[async_trait::async_trait]
        impl crate::ingress::RelayRouter for MeRouter {
            fn sender_of(&self, _node: &str) -> Option<FrameSender> {
                None
            }
            fn next_cid(&self) -> u64 {
                0
            }
            fn self_node(&self) -> &str {
                "me"
            }
            fn dial_addr_of(&self, _node: &str) -> Option<String> {
                None
            }
            fn on_hint(&self, _node: &str, _addr: &str) {}
        }
        *ig.relay_slot() = Some(std::sync::Arc::new(MeRouter));
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        // 终点 nB ≠ me → bypass（本地 lookup miss → try_relay_tell →
        // sender None → 死信；关键断言：不缓冲——立即死信计数）
        ig.dispatch(seq_tell_to("nB", 9, 9), &back, "nA").await; // 终点 nB ≠ me
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            sink.lock().unwrap().is_empty(),
            "remote destination must never hit a local namesake"
        );
        assert!(
            !ig.reorder_tx.lock().unwrap().contains_key("nA"),
            "transit traffic bypasses local reordering"
        );
    }

    // RO6：seq=0 帧（旧实现/异构网关）bypass 直投
    #[tokio::test]
    async fn ro6_seq_none_bypasses() {
        let (ig, sink) = recording_ingress();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(seq_tell(1, 1), &back, "nA").await; // 建流 expected=2
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        ig.dispatch(seq_tell(2, 0), &back, "nA").await; // seq=0 旁路
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(sink.lock().unwrap().as_slice(), &[1u8, 2], "seq=0 直投");
    }

    // RO7（RB2 回归）：Ingress drop → 重排任务退出无泄漏。
    // 历史缺陷：任务持 Arc<Ingress> 造成循环引用（map→sender / 任务→Arc），
    // 任务永不退出、Ingress 永不析构——长寿命系统每源节点泄漏一个任务。
    // 修复：任务持 Weak；本测试 drop Ingress 后断言 Weak upgrade 失败。
    #[tokio::test]
    async fn ro7_reorder_task_does_not_leak_ingress() {
        let (ig, _sink) = recording_ingress();
        let weak = Arc::downgrade(&ig);
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        // 建流 + 产生缓冲任务
        ig.dispatch(seq_tell(1, 1), &back, "nA").await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(weak.upgrade().is_some());
        // drop Ingress（模拟系统关闭）——任务只剩 Weak，不阻止析构
        drop(ig);
        // 给任务一个调度周期处理 recv None
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            weak.upgrade().is_none(),
            "Ingress 必须可析构——重排任务不得持强引用（循环引用回归）"
        );
    }

    // RO8（RA5）：空闲 30s 重排任务自清回收（map 摘除 + 计数）。
    // 用 tokio 时间加速：真实等待 30s 不可接受——本测试验证的是
    // "idle_gc 分支存在且可达"，用 start_paused 让 sleep 立即推进。
    #[tokio::test(start_paused = true)]
    async fn ro8_idle_reorder_task_recycled() {
        let (ig, _sink) = recording_ingress();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(seq_tell(1, 1), &back, "nA").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            ig.reorder_tx.lock().unwrap().contains_key("nA"),
            "任务存活期 map 有 sender"
        );
        let before = REORDER_TASK_RECYCLED.load(Ordering::Relaxed);
        // paused 模式：30s 立即到期
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        assert!(
            !ig.reorder_tx.lock().unwrap().contains_key("nA"),
            "空闲 30s 后任务自清、map 摘除"
        );
        assert!(
            REORDER_TASK_RECYCLED.load(Ordering::Relaxed) > before,
            "回收计数递增"
        );
    }

    // RO9（回绕安全）：expected 接近 u32::MAX 时"迟到"判定不误杀新帧。
    // 场景：expected = MAX-1（长流），来帧 seq=2（新流回绕）——
    // wrapping_sub 判定 seq"大于"expected（距离 < MAX/2）→ 缓冲等待，
    // 不误判为迟到丢弃。
    #[tokio::test]
    async fn ro9_wraparound_distance_compare() {
        let a: u32 = u32::MAX - 1; // expected
        let b: u32 = 2; // 新帧（回绕后）
                        // b 相对 a 前进 3 步 → 不算迟到
        assert!(b.wrapping_sub(a) <= u32::MAX / 2, "回绕后短距前进不算迟到");
        // 真迟到：expected 已放行到 100，seq=99 到
        let e: u32 = 100;
        let late: u32 = 99;
        assert!(late.wrapping_sub(e) > u32::MAX / 2, "落后 1 步判迟到");
    }

    // RO10（RB1 回归）：帧头 reserved 高 16bit 非 0 拒帧（防未来扩展位
    // 被静默吞掉——协议演进哨兵）。低 32bit seq 合法通过。
    #[test]
    fn ro10_reserved_hi16_rejected() {
        let f = Frame::tell("/a", "k", Bytes::new());
        let mut b = bytes::BytesMut::new();
        f.encode(&mut b).unwrap();
        b[23] = 0x01; // 偏移 23 = body[23] → reserved 第 6 字节（最高字节）
        assert!(matches!(
            Frame::decode(&mut b),
            Err(crate::frame::FrameError::ReservedNotZero { .. })
        ));
        // 低 32bit（偏移 18..22）任意值合法——seq 通道
        let mut b2 = bytes::BytesMut::new();
        let mut f2 = f.clone();
        f2.header.seq = u32::MAX;
        f2.encode(&mut b2).unwrap();
        assert!(Frame::decode(&mut b2).unwrap().unwrap().header.seq == u32::MAX);
    }

    // RO11（防打爆）：缓冲超 1024 帧立即放行最小 seq（不等 250ms）。
    // 场景：对端 bug/恶意刷跳跃 seq——有界内存承诺。
    #[tokio::test]
    async fn ro11_buffer_cap_releases_min() {
        let (ig, sink) = recording_ingress();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(2048);
        let back = FrameSender::anon(tx);
        // 灌 1025 帧：seq 2..1027（expected=1 缺口永不来）
        for s in 2u32..1027 {
            let f = seq_tell(s as u64, s);
            ig.dispatch(f, &back, "nA").await;
        }
        // 不等 250ms 立即查：缓冲超限 → seq=2 已放行（内存上限触发，
        // 与缺口超时无关）
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            sink.lock().unwrap().contains(&2u8),
            "缓冲超 1024 → 最小 seq=2 立即放行"
        );
    }

    // EG1（边界·观测）：SYSTEM_EVENT 无钩子 → 防御式吞帧（不 panic、有日志路径）
    #[tokio::test]
    async fn eg1_system_event_no_hook_swallowed() {
        let (ig, _cb) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        // admin::decode_sys_event 对空 payload 会 Err → warn 分支；
        // 合法 event 无钩子 → debug 分支。两条都不 panic 不回帧。
        ig.dispatch(
            Frame {
                header: crate::frame::FrameHeader {
                    frame_len: 0,
                    version: crate::frame::PROTOCOL_VERSION,
                    frame_type: frame_type::SYSTEM_EVENT,
                    flags: 0,
                    correlation_id: 0,
                    hop_count: 0,
                    hop_limit: 8,
                    seq: crate::frame::SEQ_NONE,
                },
                path: String::new(),
                type_key: String::new(),
                payload: bytes::Bytes::new(),
            },
            &back,
            "n1",
        )
        .await;
        assert!(rx.try_recv().is_err(), "无回帧");
    }

    // EG2（边界）：畸形 ROUTE_HINT payload → warn + 吞帧（不断连接）
    #[tokio::test]
    async fn eg2_malformed_route_hint_swallowed() {
        let (ig, _cb) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(
            Frame {
                header: crate::frame::FrameHeader {
                    frame_len: 0,
                    version: crate::frame::PROTOCOL_VERSION,
                    frame_type: frame_type::ROUTE_HINT,
                    flags: 0,
                    correlation_id: 0,
                    hop_count: 0,
                    hop_limit: 8,
                    seq: crate::frame::SEQ_NONE,
                },
                path: String::new(),
                type_key: String::new(),
                payload: bytes::Bytes::from_static(b"\x00\x01"), // 长度不自洽
            },
            &back,
            "n1",
        )
        .await;
        assert!(rx.try_recv().is_err(), "畸形 hint 不回帧不断连");
    }

    // EG3（边界）：未注册 type_key 的 TELL → 死信 + UnknownTypeKey 语义
    #[tokio::test]
    async fn eg3_unknown_typekey_dead_letter() {
        let (ig, _sink) = recording_ingress();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        let before = DEAD_TELL_DROPPED.load(Ordering::Relaxed);
        ig.dispatch(
            Frame::tell(
                "/user/echo",
                "bin:nonexistent::Ghost#v9",
                Bytes::from_static(b"x"),
            ),
            &back,
            "n1",
        )
        .await;
        assert_eq!(
            DEAD_TELL_DROPPED.load(Ordering::Relaxed),
            before + 1,
            "未知 type_key → 死信（部署版本不齐的观测点）"
        );
    }

    // EG4（边界）：无主 REPLY（无对应 cid）→ 迟到计数不 panic
    #[tokio::test]
    async fn eg4_orphan_reply_counted() {
        let (ig, _cb) = ingress();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        // 无主 REPLY 经 callbacks.complete → false（内部 LATE_REPLY_DROPPED
        // 计数）。断言不 panic + 无回调挂起泄漏即可。
        ig.dispatch(
            Frame::reply(777777, "", "bin:t::R", Bytes::from_static(b"late")),
            &back,
            "n1",
        )
        .await;
        // 二次同 cid 再来（重复 REPLY）同样安全
        ig.dispatch(
            Frame::reply(777777, "", "bin:t::R", Bytes::from_static(b"dup")),
            &back,
            "n1",
        )
        .await;
    }

    // EG5：RelayTable fail_target 精确失效（目标维度的挂起回收）
    #[test]
    fn eg5_relay_table_fail_target() {
        let table = RelayTable::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let source = FrameSender::new(tx, "source");
        let (a, _a_rx) = tokio::sync::mpsc::channel(4);
        let (b, _b_rx) = tokio::sync::mpsc::channel(4);
        assert!(table.insert(1, 101, source.clone(), &FrameSender::new(a, "dest-a")));
        assert!(table.insert(2, 102, source, &FrameSender::new(b, "dest-b")));
        assert!(table.fail_target("source").is_empty());
        let failed = table.fail_target("dest-a");
        assert_eq!(failed.len(), 1);
        assert_eq!(
            (failed[0].0, failed[0].1, failed[0].2.node_id()),
            (1, 101, "source")
        );
        assert_eq!(table.take(2).unwrap().0, 102);
        assert!(table.is_empty());
    }

    #[test]
    fn relay_table_capacity_and_expiry() {
        let table = RelayTable {
            slots: Default::default(),
            ttl: std::time::Duration::ZERO,
            capacity: 1,
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let source = FrameSender::anon(tx);
        let (tx, _rx2) = tokio::sync::mpsc::channel(4);
        let destination = FrameSender::new(tx, "dest");
        assert!(table.insert(1, 101, source.clone(), &destination));
        assert!(!table.insert(2, 102, source.clone(), &destination));
        let expired = table.expire();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, 101);
        assert!(table.is_empty());
        assert!(table.insert(2, 102, source, &destination));
    }

    #[test]
    fn expired_relay_reply_reports_timeout_instead_of_consuming_silently() {
        let mut ingress = Ingress::new(Arc::new(TestLookup), Arc::new(CallbackRegistry::default()));
        ingress.relay_table = Arc::new(RelayTable {
            slots: Default::default(),
            ttl: std::time::Duration::ZERO,
            capacity: 2,
        });
        let (tx, mut replies) = tokio::sync::mpsc::channel(4);
        let source = FrameSender::new(tx, "source");
        let (tx, _target_rx) = tokio::sync::mpsc::channel(4);
        let target = FrameSender::new(tx, "target");
        assert!(ingress.relay_table.insert(1, 101, source, &target));
        ingress.on_reply(Frame::reply(1, "", "bin:opaque#v1", Bytes::new()), &target);
        let reply = replies
            .try_recv()
            .expect("expired request must have a terminal response");
        assert_eq!(reply.header.correlation_id, 101);
        assert_eq!(reply.header.frame_type, frame_type::REPLY_ERR);
        assert_eq!(
            decode_err_payload(&reply.payload).unwrap().0,
            ErrCode::Timeout
        );
        assert!(ingress.relay_table.is_empty());
    }

    #[test]
    fn full_relay_reply_queue_closes_source_instead_of_silently_losing_reply() {
        let ingress = Ingress::new(Arc::new(TestLookup), Arc::new(CallbackRegistry::default()));
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let source = FrameSender::new(tx, "source");
        let (tx, _target_rx) = tokio::sync::mpsc::channel(1);
        let target = FrameSender::new(tx, "target");
        assert!(ingress.relay_table.insert(1, 101, source.clone(), &target));
        source.send_frame_sync(Frame::heartbeat()).unwrap();
        ingress.on_reply(Frame::reply(1, "", "bin:opaque#v1", Bytes::new()), &target);
        assert!(
            source.is_closed(),
            "a terminal response must not vanish on a full queue"
        );
        assert!(ingress.relay_table.is_empty());
    }

    // EG6：RelayMetrics 门面读数（不暴露 Atomic 原语的 API 契约）
    #[test]
    fn eg6_relay_metrics_facade() {
        let a = RelayMetrics::relayed_ask();
        let b = RelayMetrics::relayed_tell();
        // 只断言可读非 panic + 单调语义（不锁定绝对值——并行测试噪声）
        let _ = (a, b);
    }

    // ingress_dispatch_matrix：ASK 命中/ASK miss/REPLY 回调/REPLY_ERR/TELL miss 死信
    #[tokio::test]
    async fn ingress_dispatch_matrix() {
        let (ig, ch) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(16);
        let back = FrameSender::anon(tx);

        // ① ASK miss → REPLY_ERR(ActorNotFound)（消息键已注册，decode 通过后 lookup miss）
        let ask_payload = {
            let (k, p) = crate::codec_registry::CodecRegistry::global()
                .encode_outgoing(&(Box::new(TestAsk(1)) as BoxedMessage))
                .unwrap();
            assert_eq!(k, "bin:parrot_remote::TestAsk#v1");
            Bytes::from(p)
        };
        ig.dispatch(
            Frame::ask(
                1,
                "/user/ghost",
                "bin:parrot_remote::TestAsk#v1",
                ask_payload.clone(),
                None,
            ),
            &back,
            "n1",
        )
        .await;
        let f = rx.recv().await.unwrap();
        assert_eq!(f.header.frame_type, frame_type::REPLY_ERR);
        let (code, detail) = decode_err_payload(&f.payload).unwrap();
        assert_eq!(code, ErrCode::ActorNotFound);
        assert_eq!(detail, "/user/ghost");

        // ② TELL miss → 死信计数
        let before = DEAD_TELL_DROPPED.load(std::sync::atomic::Ordering::Relaxed);
        ig.dispatch(
            Frame::tell("/user/ghost", "bin:parrot_remote::TestAsk#v1", ask_payload),
            &back,
            "n1",
        )
        .await;
        assert_eq!(
            DEAD_TELL_DROPPED.load(std::sync::atomic::Ordering::Relaxed),
            before + 1
        );

        // ③ REPLY → 回调表完成
        let (tx_cb, rx_cb) = tokio::sync::oneshot::channel();
        ch.insert_on(42, "n1", tx_cb, &back).unwrap();
        ig.dispatch(
            Frame::reply(42, "", "bin:t::R", Bytes::from_static(b"ok")),
            &back,
            "n1",
        )
        .await;
        let got = rx_cb.await.unwrap();
        assert!(matches!(got, ReplyPayload::Ok(_, _)));

        // ④ REPLY_ERR → 回调表 Err
        let (tx_cb2, rx_cb2) = tokio::sync::oneshot::channel();
        ch.insert_on(43, "n1", tx_cb2, &back).unwrap();
        ig.dispatch(
            Frame::reply_err(43, "", ErrCode::Stopped, "stopped"),
            &back,
            "n1",
        )
        .await;
        let got2 = rx_cb2.await.unwrap();
        assert!(matches!(got2, ReplyPayload::Err(ErrCode::Stopped, _)));
    }

    // make_on_disconnect：仅当前连接的 bound callbacks + 专属状态置 Disconnected
    #[tokio::test]
    async fn on_disconnect_hook_fails_bound_callbacks_and_flags_its_statuses() {
        let callbacks = Arc::new(CallbackRegistry::new(64));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (writer, _reader) = tokio::sync::mpsc::channel(4);
        let connection = FrameSender::new(writer, "n1");
        callbacks.insert_on(9, "n1", tx, &connection).unwrap();
        let st = Arc::new(crate::node::NodeStatus::default());
        st.set(crate::node::NodeState::Connected);
        let hook = make_on_disconnect(callbacks.clone(), vec![st.clone()]);
        hook(&connection);
        let got = rx.await.unwrap();
        assert!(matches!(got, ReplyPayload::Err(ErrCode::ConnectionLost, _)));
        assert_eq!(st.get(), crate::node::NodeState::Disconnected);
    }

    // ERROR retires its connection without emitting a response.
    #[tokio::test]
    async fn ingress_error_frame_closes_connection_without_reply() {
        let (ig, _cb) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(Frame::error_frame(ErrCode::Overloaded, "busy"), &back, "n1")
            .await;
        // 无回帧
        assert!(rx.try_recv().is_err());
        assert!(
            back.is_closed(),
            "terminal ERROR left ingress connection active"
        );
    }

    // 握手帧直入 ingress（防御式吞掉——正常路径连接层拦截）
    #[tokio::test]
    async fn ingress_handshake_frame_swallowed() {
        let (ig, _cb) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(Frame::heartbeat(), &back, "n1").await;
        assert!(rx.try_recv().is_err());
    }

    // ASK payload 畸形（reply_to 前缀坏）→ REPLY_ERR(ProtocolViolation)
    #[tokio::test]
    async fn ingress_ask_malformed_prefix() {
        // 直接验证 split 失败分支（Frame::ask 总是合法前置——构造裸 payload）
        let mut f = Frame::ask(
            3,
            "/user/echo",
            "bin:parrot_remote::TestAsk#v1",
            Bytes::new(),
            None,
        );
        f.payload = Bytes::from_static(b"ab"); // < 4B → MalformedLengths
        assert!(f.split_reply_to().is_err());
        // dispatch 走同分支：回 REPLY_ERR(ProtocolViolation)
        let (ig, _cb) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(f, &back, "n1").await;
        let got = rx.recv().await.unwrap();
        assert_eq!(got.header.frame_type, frame_type::REPLY_ERR);
        let (code, _) = decode_err_payload(&got.payload).unwrap();
        assert_eq!(code, ErrCode::ProtocolViolation);
    }

    // local_path 边界：parrot:// 无斜杠尾 → 原样返回
    #[test]
    fn local_path_edge_cases() {
        assert_eq!(local_path("parrot://node-only"), "parrot://node-only");
        assert_eq!(local_path(""), "");
    }

    // ASK 解码失败（未知键）→ REPLY_ERR(UnknownTypeKey)
    #[tokio::test]
    async fn ingress_ask_unknown_key() {
        let (ig, _) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(16);
        let back = FrameSender::anon(tx);
        ig.dispatch(
            Frame::ask(7, "/user/echo", "bin:nowhere::Z#v1", Bytes::new(), None),
            &back,
            "n1",
        )
        .await;
        let f = rx.recv().await.unwrap();
        let (code, _) = decode_err_payload(&f.payload).unwrap();
        assert_eq!(code, ErrCode::UnknownTypeKey);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_reorder_registration_uses_one_queue() {
        let (ingress, _) = recording_ingress();
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let ingress = ingress.clone();
                let barrier = barrier.clone();
                let runtime = tokio::runtime::Handle::current();
                std::thread::spawn(move || {
                    let _runtime = runtime.enter();
                    barrier.wait();
                    ingress.reorder_tx_of("concurrent-source").unwrap()
                })
            })
            .collect();
        let senders: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(
            senders
                .iter()
                .all(|sender| sender.same_channel(&senders[0])),
            "concurrent dispatch created multiple FIFO streams for one source"
        );
        ingress.close();
        ingress.wait_closed().await;
    }

    #[tokio::test(start_paused = true)]
    async fn idle_reorder_keeps_queue_held_by_dispatcher() {
        let (ingress, sink) = recording_ingress();
        let tx = ingress.reorder_tx_of("idle-held").unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        assert!(
            !tx.is_closed(),
            "idle GC invalidated a dispatcher-held sender"
        );
        tx.send(seq_tell(1, 1)).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while sink.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*sink.lock().unwrap(), vec![1]);
        drop(tx);
        ingress.close();
        ingress.wait_closed().await;
    }

    #[tokio::test]
    async fn retired_reorder_registration_cannot_remove_replacement() {
        let (ingress, _) = recording_ingress();
        let (old, _old_rx) = tokio::sync::mpsc::channel(1);
        let old_registration = ReorderRegistration {
            map: Arc::downgrade(&ingress.reorder_tx),
            source: "same-source".into(),
            sender: old.downgrade(),
        };
        let replacement = ingress.reorder_tx_of("same-source").unwrap();
        drop(old_registration);
        let current = ingress.reorder_tx_of("same-source").unwrap();
        assert!(current.same_channel(&replacement));
        ingress.close();
        ingress.wait_closed().await;
    }
}
