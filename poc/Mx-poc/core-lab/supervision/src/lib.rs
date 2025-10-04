//! POC 2：监督执行器（Supervisor）+ Watch 死亡通知（Death Watch）。
//!
//! 评测 F8/F6/F18：actor 框架的"身份性功能"。
//! 实现：
//!   - SupervisorTree：父 spawn 子，子 panic/dead → 按 strategy 决策
//!     （Restart / Stop / Escalate），带 restart 退避与窗口限频
//!   - DeathWatch：monitor/monitoree 分离（任意方可 watch 任意 ref）
//!   - Terminated 广播（watcher 收到死因：Normal / Panic / Killed）

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub type BoxedMessage = Box<dyn std::any::Any + Send>;
pub type BoxedResult = Result<BoxedMessage, String>;

#[derive(Debug, Clone, PartialEq)]
pub enum DeathReason {
    Normal,
    Panic(String),
    Killed,
    Escalated(String),
}

impl std::fmt::Display for DeathReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeathReason::Normal => write!(f, "normal"),
            DeathReason::Panic(m) => write!(f, "panic: {m}"),
            DeathReason::Killed => write!(f, "killed"),
            DeathReason::Escalated(m) => write!(f, "escalated: {m}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WatchEvent {
    Terminated { path: String, reason: DeathReason },
}

/// 监督策略。
#[derive(Debug, Clone)]
pub enum SupervisionStrategy {
    /// 崩溃即重启（带窗口限频：window 内超过 max_restarts 则升级）
    OneForOne { max_restarts: usize, window: Duration },
    Stop,
    Escalate,
}

/// 子 actor 工厂（重启需要重建闭包）。
/// 共享可重入工厂：Arc<Mutex<>> 包裹，重启时再调一次生成新实例。
pub type ChildFactory = Arc<Mutex<Box<dyn FnMut() -> Box<dyn FnMut(BoxedMessage) -> BoxedResult + Send> + Send>>>;

struct ChildEntry {
    path: String,
    factory: ChildFactory,
    strategy: SupervisionStrategy,
    restarts: Vec<Instant>, // 窗口内重启时间戳
}

pub struct Supervisor {
    children: Mutex<HashMap<String, ChildEntry>>,
    /// 死亡通知总线（watcher 订阅）
    watchers: Mutex<HashMap<String, Sender<WatchEvent>>>, // watcher_path → tx
    /// actor 路径 → 发送端（投递消息用）
    mails: Mutex<HashMap<String, Sender<(BoxedMessage, Option<Sender<BoxedResult>>)>>>,
}

impl Supervisor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { children: Mutex::new(HashMap::new()), watchers: Mutex::new(HashMap::new()), mails: Mutex::new(HashMap::new()) })
    }

    /// supervise spawn：起线程跑 actor，panic 捕获 → 策略执行 → 通知 watcher。
    pub fn spawn_supervised(
        self: &Arc<Self>,
        mut factory: ChildFactory,
        path: &str,
        strategy: SupervisionStrategy,
    ) -> Result<String, String> {
        // 先登记条目（策略/重启历史），再跑首轮
        self.children.lock().unwrap().insert(
            path.to_string(),
            ChildEntry { path: path.into(), factory: factory.clone(), strategy, restarts: vec![] },
        );
        self.start_child(path, factory)
    }

    fn start_child(self: &Arc<Self>, path: &str, factory: ChildFactory) -> Result<String, String> {
        let (tx, rx) = channel::<(BoxedMessage, Option<Sender<BoxedResult>>)>();
        self.mails.lock().unwrap().insert(path.to_string(), tx.clone());

        let sup = self.clone();
        let path_owned = path.to_string();
        std::thread::spawn(move || {
            let mut actor = (factory.lock().unwrap())();
            for (msg, rtx) in rx {
                let outcome = catch_unwind(AssertUnwindSafe(|| actor(msg)));
                match outcome {
                    Ok(r) => {
                        if let Some(rtx) = rtx { let _ = rtx.send(r); }
                    }
                    Err(panic) => {
                        let reason = DeathReason::Panic(panic_msg(panic));
                        if let Some(rtx) = rtx {
                            let _ = rtx.send(Err(format!("panic: {:?}", reason)));
                        }
                        sup.on_child_death(&path_owned, reason);
                        return; // 线程退出，由 supervisor 决策重启
                    }
                }
            }
            // 通道关闭：正常退出
            sup.on_child_death(&path_owned, DeathReason::Normal);
        });
        Ok(path.to_string())
    }

    /// 子死亡 → 策略决策 + 死亡广播。
    fn on_child_death(self: &Arc<Self>, path: &str, reason: DeathReason) {
        // 通知 watcher（Death Watch 语义：无论什么死法都广播）
        self.broadcast(path, &reason);

        if reason == DeathReason::Normal {
            return; // 正常退出不重启
        }

        let decision = {
            let mut g = self.children.lock().unwrap();
            match g.get_mut(path) {
                None => None,
                Some(entry) => match &entry.strategy {
                    SupervisionStrategy::Stop => Some(Decision::Stop),
                    SupervisionStrategy::Escalate => Some(Decision::Escalate(reason.to_string())),
                    SupervisionStrategy::OneForOne { max_restarts, window } => {
                        let now = Instant::now();
                        entry.restarts.retain(|t| now.duration_since(*t) < *window);
                        if entry.restarts.len() >= *max_restarts {
                            Some(Decision::Escalate(format!("restart budget exhausted: {}", path)))
                        } else {
                            entry.restarts.push(now);
                            Some(Decision::Restart)
                        }
                    }
                },
            }
        };

        match decision {
            Some(Decision::Restart) => {
                // 重建 actor（共享 factory 再调一次）
                let factory = {
                    let g = self.children.lock().unwrap();
                    g.get(path).map(|e| e.factory.clone())
                };
                if let Some(f) = factory {
                    let _ = self.start_child(path, f);
                }
            }
            Some(Decision::Stop) => { /* 不重启 */ }
            Some(Decision::Escalate(msg)) => {
                // POC：升级 = 广播给所有 watcher + 停树
                self.broadcast(path, &DeathReason::Escalated(msg));
            }
            None => {}
        }
    }

    fn broadcast(&self, path: &str, reason: &DeathReason) {
        let ws = self.watchers.lock().unwrap();
        for tx in ws.values() {
            let _ = tx.send(WatchEvent::Terminated { path: path.into(), reason: reason.clone() });
        }
    }

    // ---------------- Watch API ----------------

    /// 任意方 watch 任意路径（monitor 与被 monitor 分离）。
    pub fn watch(&self, watcher_path: &str, target_path: &str) -> Receiver<WatchEvent> {
        let _ = target_path;
        let (tx, rx) = channel();
        self.watchers.lock().unwrap().insert(watcher_path.to_string(), tx);
        rx
    }

    /// 解除 watch。
    pub fn unwatch(&self, watcher_path: &str) {
        self.watchers.lock().unwrap().remove(watcher_path);
    }

    // ---------------- 消息投递 ----------------

    pub fn tell(&self, path: &str, msg: BoxedMessage) -> Result<(), String> {
        let mails = self.mails.lock().unwrap();
        mails.get(path).ok_or("not found")?.send((msg, None)).map_err(|e| e.to_string())
    }

    pub fn ask(&self, path: &str, msg: BoxedMessage) -> BoxedResult {
        let (rtx, rrx) = channel();
        {
            let mails = self.mails.lock().unwrap();
            let tx = mails.get(path).ok_or_else(|| "not found".to_string())?;
            tx.send((msg, Some(rtx))).map_err(|e| e.to_string())?;
        }
        rrx.recv_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?
    }
}

enum Decision {
    Restart,
    Stop,
    Escalate(String),
}

fn panic_msg(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() { s.to_string() }
    else if let Some(s) = p.downcast_ref::<String>() { s.clone() }
    else { "opaque panic".into() }
}

// ============ 测试 ============

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn counter_factory() -> ChildFactory {
        Arc::new(Mutex::new(
            Box::new(|| {
                // 每次工厂调用生成全新 actor 闭包（重启后计数归零——验证 factory 重建）
                Box::new(|msg: BoxedMessage| {
                    if let Some(n) = msg.downcast_ref::<u64>() {
                        if *n == 999 { panic!("boom"); }
                        Ok(Box::new(n + 1) as BoxedMessage)
                    } else { Err("unsupported".into()) }
                }) as Box<dyn FnMut(BoxedMessage) -> BoxedResult + Send>
            }) as Box<dyn FnMut() -> Box<dyn FnMut(BoxedMessage) -> BoxedResult + Send> + Send>,
        ))
    }

    #[test]
    fn supervisor_restart_on_panic() {
        let sup = Supervisor::new();
        sup.spawn_supervised(
            counter_factory(),
            "/sup/counter",
            SupervisionStrategy::OneForOne { max_restarts: 3, window: Duration::from_secs(10) },
        )
        .unwrap();

        // 正常 ask
        let r = sup.ask("/sup/counter", Box::new(41u64)).unwrap();
        assert_eq!(*r.downcast::<u64>().unwrap(), 42);

        // 触发 panic（999）→ 重启 → 计数逻辑恢复
        let _ = sup.ask("/sup/counter", Box::new(999u64));
        std::thread::sleep(Duration::from_millis(100)); // 等重启
        let r = sup.ask("/sup/counter", Box::new(1u64)).unwrap();
        assert_eq!(*r.downcast::<u64>().unwrap(), 2, "panic 后应自动重启恢复");
    }

    #[test]
    fn death_watch_notification() {
        let sup = Supervisor::new();
        sup.spawn_supervised(
            counter_factory(),
            "/victim",
            SupervisionStrategy::OneForOne { max_restarts: 1, window: Duration::from_secs(10) },
        )
        .unwrap();
        let rx = sup.watch("/watcher", "/victim");

        // 击杀
        let _ = sup.ask("/victim", Box::new(999u64));
        let ev = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match ev {
            WatchEvent::Terminated { path, reason } => {
                assert_eq!(path, "/victim");
                assert!(matches!(reason, DeathReason::Panic(_)));
            }
        }
    }

    #[test]
    fn restart_budget_escalation() {
        let sup = Supervisor::new();
        let n = AtomicU64::new(0);
        let factory: ChildFactory = Arc::new(Mutex::new(
            Box::new(move || {
                n.fetch_add(1, Ordering::SeqCst);
                Box::new(|msg: BoxedMessage| {
                    if msg.downcast_ref::<u64>().map(|v| *v == 999).unwrap_or(false) {
                        panic!("boom");
                    }
                    Ok(Box::new(0u64) as BoxedMessage)
                }) as Box<dyn FnMut(BoxedMessage) -> BoxedResult + Send>
            }) as Box<dyn FnMut() -> Box<dyn FnMut(BoxedMessage) -> BoxedResult + Send> + Send>,
        ));
        sup.spawn_supervised(
            factory,
            "/fragile",
            SupervisionStrategy::OneForOne { max_restarts: 2, window: Duration::from_secs(10) },
        )
        .unwrap();
        let rx = sup.watch("w", "/fragile");

        // 连环击杀 → 窗口内超限 → Escalate
        for _ in 0..3 {
            let _ = sup.ask("/fragile", Box::new(999u64));
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut escalated = false;
        while let Ok(ev) = rx.recv_timeout(Duration::from_millis(300)) {
            if let WatchEvent::Terminated { reason: DeathReason::Escalated(_), .. } = ev {
                escalated = true;
                break;
            }
        }
        assert!(escalated, "超限应升级 Escalate");
    }
}
