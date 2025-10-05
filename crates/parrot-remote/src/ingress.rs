//! 职责：入站帧分发——ASK/TELL/STOP → 本地 actor；REPLY → 回调表（05 §6.1）。
//!
//! I2（06）：入站 ASK 并发化——每帧 spawn 独立任务处理（慢 actor 不再
//! 阻塞同连接的后续帧；REPLY 乱序经 cid 配对天然支持）。TELL 保持串行
//! deliver（05 §6.3 背压贯通语义——对端过载时读循环挂起是有意行为）。
//! 本地解析经 LocalLookup trait 倒置（parrot-remote 不依赖 parrot crate，
//! E5.2 分层铁律）。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::codec_registry::CodecRegistry;
use crate::error::{decode_err_payload, ErrCode};
use crate::frame::{frame_type, Frame};
use crate::node::NodeState;
use crate::registry::{CallbackRegistry, ReplyPayload};
use crate::transport::FrameSender;

/// TELL 重排：迟到帧（缺口超时放行后到达）丢弃计数。
pub static LATE_TELL_DROPPED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// TELL 重排：缺口超时放行计数（丢帧不卡死语义的触发次数）。
pub static REORDER_GAP_FLUSHED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// TELL 重排：实际缓冲过的帧数（乱序真正发生才 >0——切换点观测指标）。
pub static REORDER_BUFFERED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// 本地 actor 解析出口（trait 倒置：parrot facade 在应用层注入实现）。
#[async_trait::async_trait]
pub trait LocalLookup: Send + Sync + 'static {
    /// 路径 → 本地 ActorRef（miss 返回 None → REPLY_ERR ActorNotFound）。
    async fn lookup(&self, path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>>;
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

/// 转发映射：forward_cid → (orig_cid, 回源 FrameSender, 到期时刻)。
///
/// hub 路由（07 §6）核心结构：入站 ASK 目标节点非本节点时，分配新 cid
/// 转发到目标节点；REPLY/REPLY_ERR 回来按新 cid 查表还原 orig_cid，经
/// 保存的回源 sender 送回发起方。条目带 TTL（防目标节点泄漏——超时清理
/// 由 complete/断连时的懒扫描兜底）。
pub struct RelayTable {
    slots: std::sync::Mutex<std::collections::HashMap<
        u64,
        (u64, FrameSender, std::time::Instant),
    >>,
    ttl: std::time::Duration,
}

impl RelayTable {
    pub fn new() -> Self {
        Self {
            slots: std::sync::Mutex::new(Default::default()),
            ttl: std::time::Duration::from_secs(65),
        }
    }

    /// 登记映射（forward_cid 分配方：hub 的 CallbackRegistry.next_cid）。
    pub fn insert(&self, forward_cid: u64, orig_cid: u64, back: FrameSender) {
        self.slots
            .lock()
            .unwrap()
            .insert(forward_cid, (orig_cid, back, std::time::Instant::now()));
    }

    /// 取出并移除（REPLY 回程一次性消费）。过期条目返回 None（懒清理）。
    pub fn take(&self, forward_cid: u64) -> Option<(u64, FrameSender)> {
        let mut g = self.slots.lock().unwrap();
        match g.remove(&forward_cid) {
            Some((orig, back, at)) if at.elapsed() < self.ttl => Some((orig, back)),
            _ => None,
        }
    }

    /// 目标节点断连：其作为转发目标的所有挂起条目失效（回源 REPLY_ERR）。
    /// 返回失效条目（调用方逐条发 ConnectionLost 错误帧）。
    pub fn fail_target(&self, target: &str) -> Vec<(u64, u64, FrameSender)> {
        let mut g = self.slots.lock().unwrap();
        let hit: Vec<u64> = g
            .iter()
            .filter(|(_, (_, back, _))| back.node_id() == target)
            .map(|(cid, _)| *cid)
            .collect();
        hit.iter()
            .filter_map(|cid| g.remove(cid).map(|(o, b, _)| (*cid, o, b)))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }
}

impl Default for RelayTable {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Ingress {
    pub local: Arc<dyn LocalLookup>,
    pub callbacks: Arc<CallbackRegistry>,
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
    reorder_tx: std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<Frame>>>,
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
        Self {
            local,
            callbacks,
            sys_event: std::sync::RwLock::new(None),
            relay: std::sync::RwLock::new(None),
            relay_table: Arc::new(RelayTable::new()),
            learned: std::sync::Mutex::new(Default::default()),
            reorder_tx: std::sync::Mutex::new(Default::default()),
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

/// SYSTEM_EVENT 入站钩子（返回 false = 未处理，ingress 记日志）。
#[async_trait::async_trait]
pub trait SysEventHook: Send + Sync + 'static {
    async fn on_event(&self, event: crate::admin::SysEvent, back: &FrameSender, from: &str);
}

/// 帧路径 → 本地查找路径：剥 parrot://{node}/ 前缀（节点寻址已在连接层完成，
/// 本地表只存 /system/... 形态——05 §3.1 一文法两形态）。
fn local_path(path: &str) -> &str {
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

impl Ingress {
    /// 处理一帧入站（来自 ConnectionTask 的 inbound 队列）。
    /// `back` 是该连接的回程发送端（REPLY 用）。
    ///
    /// I2：ASK 并发分发——spawn 独立任务（慢 actor 不阻塞同连接的后续帧；
    /// REPLY 乱序经 cid 配对天然支持）。FrameSender 是 Clone，move 安全。
    /// TELL 保持串行 deliver（05 §6.3 背压贯通——对端过载时读循环挂起
    /// 是有意语义，不并发化）。
    pub async fn dispatch(self: &Arc<Self>, frame: Frame, back: &FrameSender, from_node: &str) {
        match frame.header.frame_type {
            frame_type::ASK => {
                let back = back.clone();
                let this = self.clone();
                tokio::spawn(async move {
                    this.on_ask(frame, &back).await;
                });
            }
            frame_type::TELL => self.on_tell(frame, back, from_node).await,
            frame_type::STOP => self.on_stop(frame).await,
            frame_type::REPLY | frame_type::REPLY_ERR => self.on_reply(frame),
            frame_type::SYSTEM_EVENT => {
                // P2 管理通道：admin/gossip/receptionist 同帧不同标签
                match crate::admin::decode_sys_event(&frame.payload) {
                    Ok(event) => {
                        let hook = self.sys_event.read().unwrap().clone();
                        if let Some(hook) = hook {
                            hook.on_event(event, back, from_node).await;
                        } else {
                            tracing::debug!("SYSTEM_EVENT ignored (no hook installed)");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "malformed SYSTEM_EVENT dropped");
                    }
                }
            }
            frame_type::ERROR => {
                let (code, detail) = decode_err_payload(&frame.payload)
                    .unwrap_or((ErrCode::ProtocolViolation, "<undecodable>".into()));
                tracing::warn!(node = %from_node, code = code.name(), %detail, "remote ERROR frame");
            }
            frame_type::ROUTE_HINT => {
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
            // 心跳/握手由 ConnectionTask 处理；集群帧在连接层已断连——防御式吞掉
            _ => {
                tracing::debug!(
                    ft = frame.header.frame_type,
                    "ingress: frame handled at conn layer"
                );
            }
        }
    }

    async fn on_ask(&self, frame: Frame, back: &FrameSender) {
        let cid = frame.header.correlation_id;
        // 剥 reply_to 前缀（DEV_01 §3.1 约定）
        let (reply_to, payload) = match frame.split_reply_to() {
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
        // 解码
        let decoded = match CodecRegistry::global().decode_incoming(&frame.type_key, &payload) {
            Ok(m) => m,
            Err(code) => {
                let _ = back
                    .send(Frame::reply_err(
                        cid,
                        &frame.path,
                        code,
                        &format!("type_key={}", frame.type_key),
                    ))
                    .await;
                return;
            }
        };
        // 本地解析（facade 注入）；miss 且目标是其他 parrot 节点 → hub 转发
        let local_ref = match self.local.lookup(local_path(&frame.path)).await {
            Some(r) => r,
            None => {
                // hub 路由（07 §6）：parrot://{other}/... 由本节点中转。
                // RouteUnreachable/ConnectionLost 的错误帧 try_relay_ask 已
                // 自行回源；这里只兜底 ActorNotFound（常规 miss 语义）。
                if self.try_relay_ask(&frame, back, cid).await == Some(ErrCode::ActorNotFound)
                {
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
        };
        // 远端不二次超时（07 §7 双超时竞态规避）：send() 无界。
        // panic 隔离：本地引擎 panic 不能杀分发循环（连接存活优先）
        let send_fut = local_ref.send(decoded);
        let outcome = std::panic::AssertUnwindSafe(send_fut);
        let outcome = futures::FutureExt::catch_unwind(outcome).await;
        let outcome: Result<parrot_api::types::BoxedMessage, parrot_api::errors::ActorError> =
            match outcome {
                Ok(r) => r,
                Err(p) => Err(parrot_api::errors::ActorError::Panic(
                    p.downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "local actor panicked".into()),
                )),
            };
        match outcome {
            Ok(reply) => {
                // 回复类型编码（回复也必须 RemoteMessage）
                let (key, payload) = match CodecRegistry::global().encode_outgoing(&reply) {
                    Ok(v) => v,
                    Err(code) => {
                        let _ = back
                            .send(Frame::reply_err(
                                cid,
                                &frame.path,
                                code,
                                "reply type not remotable",
                            ))
                            .await;
                        return;
                    }
                };
                // REPLY.path = reply_to 回程路径（cid 配对走回调表）
                let path = reply_to.unwrap_or_default();
                let _ = back
                    .send(Frame::reply(cid, &path, &key, bytes::Bytes::from(payload)))
                    .await;
            }
            Err(e) => {
                let (code, detail) = ErrCode::from_actor_error(&e);
                let _ = back
                    .send(Frame::reply_err(cid, &frame.path, code, &detail))
                    .await;
            }
        }
    }

    /// hub 转发 ASK：目标是其他节点且本节点有路由能力 → 转发 + 映射登记。
    /// 返回 Some(code) = 未转发（code 是应回源的错误码；ActorNotFound =
    /// 常规 miss 语义）；None = 已转发（等 REPLY 自动回源）。
    async fn try_relay_ask(
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
        // 转发：新 cid + hop_count+1（防环；≥hop_limit 由对端 Frame 校验拒收）
        let forward_cid = router.next_cid();
        let mut fwd = frame.clone();
        fwd.header.correlation_id = forward_cid;
        fwd.header.hop_count = fwd.header.hop_count.saturating_add(1);
        if sender.send(fwd).await.is_err() {
            let _ = back
                .send(Frame::reply_err(
                    orig_cid,
                    &frame.path,
                    ErrCode::ConnectionLost,
                    "relay link closed",
                ))
                .await;
            return Some(ErrCode::ConnectionLost);
        }
        // 映射登记（REPLY 回来自动还原回源）
        self.relay_table.insert(forward_cid, orig_cid, back.clone());
        RELAYED_ASK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 方案 A：目标可直拨 → 向源注入 ROUTE_HINT（源学直连，后续帧不再
        // 经 hub——try_send 队列满仅丢弃 hint，学习退化为继续中转，无害）
        if let Some(addr) = router.dial_addr_of(target) {
            let _ = back.send_frame(Frame::route_hint(target, &addr));
        }
        tracing::debug!(%target, forward_cid, orig_cid, "relayed ask");
        None
    }

    /// REPLY 回源：先查转发映射（本节点作为 hub 中转的回程），命中则
    /// 还原 orig_cid 送回源连接；未命中走本地回调表（本节点发起的 ask）。
    fn relay_reply(&self, frame: &Frame) -> bool {
        let cid = frame.header.correlation_id;
        if let Some((orig_cid, back)) = self.relay_table.take(cid) {
            let mut out = frame.clone();
            out.header.correlation_id = orig_cid;
            back.send_frame(out);
            tracing::debug!(cid, orig_cid, "relayed reply");
            return true;
        }
        false
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
            if let Some(router) = self
                .relay
                .read()
                .ok()
                .and_then(|g| g.clone())
            {
                router.on_hint(node, addr);
            }
        }
    }

    /// TELL 入站：端到端重排网关。
    ///
    /// 三类路径：
    /// 1. **终点是本节点的直连 seq 帧**：per-from 重排任务——seq 流来自
    ///    唯一源（直连对端），流身份成立，可安全重排
    /// 2. **中转帧**（目标节点非本节点，含来自 uplink hub 的和 hub 收到
    ///    的跨 spoke 帧）：seq 流多目标/多源交错不可归因 → bypass 直转
    /// 3. **seq==0 帧**：旧实现/异构网关 → bypass 直投（兼容语义）
    ///
    /// 重排任务冷启动：首帧 seq=N>1（切换后直连新流）→ 缓冲等 250ms——
    /// 在途 uplink 前缀帧走 bypass 路径先投（时序恰好正确），超时放行 N。
    /// 背压：bounded channel 满则 dispatch 挂起 → 读循环挂起（RC8 贯通）。
    async fn on_tell(self: &Arc<Self>, frame: Frame, back: &FrameSender, from_node: &str) {
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
        if seq == crate::frame::SEQ_NONE || !destined_here {
            self.tell_deliver_or_relay(frame, back).await;
            return;
        }
        // 直连 seq 帧 → per-from 重排任务
        let tx = self.reorder_tx_of(from_node).await;
        if let Err(e) = tx.send(frame).await {
            // 重排任务已消亡（不该发生——仅 actor 系统关闭时）→ 直投兜底
            self.tell_deliver_or_relay(e.0, back).await;
        }
    }

    /// 取/建 per-from 重排任务 sender。
    async fn reorder_tx_of(self: &Arc<Self>, from: &str) -> tokio::sync::mpsc::Sender<Frame> {
        if let Some(tx) = self
            .reorder_tx
            .lock()
            .ok()
            .and_then(|g| g.get(from).cloned())
        {
            return tx;
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<Frame>(256);
        let from_owned = from.to_string();
        let this = self.clone();
        tokio::spawn(async move {
            this.reorder_loop(from_owned, rx).await;
        });
        if let Ok(mut g) = self.reorder_tx.lock() {
            g.insert(from.to_string(), tx.clone());
        }
        tx
    }

    /// per 源节点重排循环：expected/BTreeMap 状态机 + recv 超时冲刷。
    ///
    /// 冷启动缺口（expected=1，首帧 seq=N>1）与运行中缺口共用同一条
    /// 250ms 超时路径：超时放行最小 seq（跳缺口），expected 跳至其+1；
    /// 缓冲余帧若与放行帧连续则一并投递。丢帧绝不永久阻塞。
    async fn reorder_loop(self: &Arc<Self>, from: String, mut rx: tokio::sync::mpsc::Receiver<Frame>) {
        let mut expected: u32 = 1;
        let mut buf: std::collections::BTreeMap<u32, Frame> = Default::default();
        loop {
            let idle = tokio::time::Duration::from_millis(250);
            let f = tokio::select! {
                f = rx.recv() => match f {
                    Some(f) => f,
                    None => {
                        // 通道关闭（源连接长期静默拆除——防御式）：冲刷缓冲
                        if !buf.is_empty() {
                            tracing::debug!(from = %from, buffered = buf.len(), "reorder loop flush on channel close");
                            for (_, fr) in buf.into_iter() {
                                let back = FrameSender::detached();
                                self.tell_deliver_or_relay(fr, &back).await;
                            }
                        }
                        return;
                    }
                },
                _ = tokio::time::sleep(idle), if !buf.is_empty() => {
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
                    for fr in chain {
                        let back = FrameSender::detached();
                        self.tell_deliver_or_relay(fr, &back).await;
                    }
                    continue;
                }
            };
            let seq = f.header.seq;
            if seq == expected {
                let back = FrameSender::detached();
                self.tell_deliver_or_relay(f, &back).await;
                expected = expected.wrapping_add(1);
                // drain 连续后继
                while let Some(&k) = buf.keys().next() {
                    if k == expected {
                        let fr = buf.remove(&k).unwrap();
                        let back = FrameSender::detached();
                        self.tell_deliver_or_relay(fr, &back).await;
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
                if buf.len() > 1024 {
                    // 防打爆上限：放行最小 seq（跳缺口）
                    let min_seq = *buf.keys().next().unwrap();
                    let f = buf.remove(&min_seq).unwrap();
                    REORDER_GAP_FLUSHED.fetch_add(1, Ordering::Relaxed);
                    expected = min_seq.wrapping_add(1);
                    let back = FrameSender::detached();
                    self.tell_deliver_or_relay(f, &back).await;
                    while let Some(&k) = buf.keys().next() {
                        if k == expected {
                            let fr = buf.remove(&k).unwrap();
                            let back = FrameSender::detached();
                            self.tell_deliver_or_relay(fr, &back).await;
                            expected = expected.wrapping_add(1);
                        } else {
                            break;
                        }
                    }
                }
            }
        }
    }

    /// TELL 实际投递（本地命中 / hub 中转 / 死信）——原 on_tell 主体。
    async fn tell_deliver_or_relay(&self, frame: Frame, back: &FrameSender) {
        let decoded = match CodecRegistry::global().decode_incoming(&frame.type_key, &frame.payload)
        {
            Ok(m) => m,
            Err(_) => {
                DEAD_TELL_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        if let Some(r) = self.local.lookup(local_path(&frame.path)).await {
            // deliver 挂起 = 读循环挂起 = 对端背压贯通（05 §6.3，RC8）
            let _ = r.deliver(decoded).await;
        } else if self.try_relay_tell(&frame, back) {
            // hub 中转（TELL 无回程——fire-and-forget 转发）
        } else {
            DEAD_TELL_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(path = %frame.path, "TELL to unknown actor (dead letter)");
        }
    }

    /// hub 转发 TELL（无回程语义——转发成功即完事）。
    fn try_relay_tell(&self, frame: &Frame, back: &FrameSender) -> bool {
        let Some(target) = target_node(&frame.path) else {
            return false;
        };
        let Ok(g) = self.relay.read() else { return false };
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
        if sender.send_frame_sync(fwd).is_err() {
            return false;
        }
        RELAYED_TELL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 方案 A：同 ask——向源注入直连 hint
        if let Some(addr) = router.dial_addr_of(target) {
            let _ = back.send_frame(Frame::route_hint(target, &addr));
        }
        tracing::debug!(%target, "relayed tell");
        true
    }

    async fn on_stop(&self, frame: Frame) {
        if let Some(r) = self.local.lookup(local_path(&frame.path)).await {
            let _ = r.stop().await;
        }
    }

    fn on_reply(&self, frame: Frame) {
        // hub 中转回程优先（本节点转发的 ask 的 REPLY——还原 cid 回源）
        if self.relay_reply(&frame) {
            return;
        }
        let cid = frame.header.correlation_id;
        match frame.header.frame_type {
            frame_type::REPLY => {
                self.callbacks
                    .complete(cid, ReplyPayload::Ok(frame.payload, frame.type_key));
            }
            _ => {
                let (code, detail) = decode_err_payload(&frame.payload)
                    .unwrap_or((ErrCode::ProtocolViolation, "<undecodable>".into()));
                self.callbacks
                    .complete(cid, ReplyPayload::Err(code, detail));
            }
        }
    }
}

/// 断连清理钩子（RemoteActorSystem 注入 ConnectionTask）：
/// callbacks.fail_node(ConnectionLost)（单链路隔离）+ NodeTable → Disconnected。
pub fn make_on_disconnect(
    callbacks: Arc<CallbackRegistry>,
    statuses: Vec<Arc<crate::node::NodeStatus>>,
) -> Arc<dyn Fn(&str) + Send + Sync> {
    Arc::new(move |node_id: &str| {
        let n = callbacks.fail_node(node_id, ErrCode::ConnectionLost, "connection lost");
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

    #[test]
    fn local_path_strips_node_prefix() {
        assert_eq!(local_path("parrot://node-b/user/echo"), "/user/echo");
        assert_eq!(local_path("/user/echo"), "/user/echo");
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
            fn sender_of(&self, _node: &str) -> Option<FrameSender> { None }
            fn next_cid(&self) -> u64 { 0 }
            fn self_node(&self) -> &str { "me" }
            fn dial_addr_of(&self, _node: &str) -> Option<String> { None }
            fn on_hint(&self, _node: &str, _addr: &str) {}
        }
        *ig.relay_slot() = Some(std::sync::Arc::new(MeRouter));
        let (tx, _rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        // 终点 nB ≠ me → bypass（本地 lookup miss → try_relay_tell →
        // sender None → 死信；关键断言：不缓冲——立即死信计数）
        ig.dispatch(seq_tell_to("nB", 9, 9), &back, "nA").await; // 终点 nB ≠ me
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        // bypass 证明：本地命中立即投递（无 250ms 重排等待）——sink 已 9
        assert_eq!(sink.lock().unwrap().as_slice(), &[9u8], "中转帧 bypass 立即投递");
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
        ch.insert(42, "n1", tx_cb).unwrap();
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
        ch.insert(43, "n1", tx_cb2).unwrap();
        ig.dispatch(
            Frame::reply_err(43, "", ErrCode::Stopped, "stopped"),
            &back,
            "n1",
        )
        .await;
        let got2 = rx_cb2.await.unwrap();
        assert!(matches!(got2, ReplyPayload::Err(ErrCode::Stopped, _)));
    }

    // make_on_disconnect：fail_all + 状态置 Disconnected
    #[tokio::test]
    async fn on_disconnect_hook_fails_all_and_flags_nodes() {
        let callbacks = Arc::new(CallbackRegistry::new(64));
        let (tx, rx) = tokio::sync::oneshot::channel();
        callbacks.insert(9, "n1", tx).unwrap();
        let st = Arc::new(crate::node::NodeStatus::default());
        st.set(crate::node::NodeState::Connected);
        let hook = make_on_disconnect(callbacks.clone(), vec![st.clone()]);
        hook("n1");
        let got = rx.await.unwrap();
        assert!(matches!(got, ReplyPayload::Err(ErrCode::ConnectionLost, _)));
        assert_eq!(st.get(), crate::node::NodeState::Disconnected);
    }

    // ERROR 帧入站 → 仅日志（不 panic、不回帧）
    #[tokio::test]
    async fn ingress_error_frame_swallowed() {
        let (ig, _cb) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender::anon(tx);
        ig.dispatch(Frame::error_frame(ErrCode::Overloaded, "busy"), &back, "n1")
            .await;
        // 无回帧
        assert!(rx.try_recv().is_err());
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
}
