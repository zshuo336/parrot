//! POC 5：静态类型编程模式（动态/静态双轨 ActorRef）。
//!
//! 核心思想：
//!   - 动态轨（现状）：Box<dyn Any + Send>，2 装箱 + 2 downcast
//!   - 静态轨（新增）：TypedActorRef<M>，send/ask/tell 消息类型编译期固定，
//!     零 Any 装箱、零 downcast，直达引擎 mailbox（mpsc<M> 类型化通道）
//!   - 两条轨同一引擎并存，可互转（typed ↔ dyn 桥接点显式装箱一次）
//!   - #[typed_actor] 宏消除模板代码

use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

pub type BoxedMessage = Box<dyn Any + Send>;
pub type BoxedResult = Result<BoxedMessage, String>;

// ============ 静态轨接口（用户需求中的 ActorRefExt） ============

/// 静态 ask：M → R 编译期固定。
pub trait TypedAskRef<M: Send + 'static>: Send + Sync {
    type Reply: Send + 'static;
    fn ask(&self, msg: M) -> Result<Self::Reply, String>;
    fn tell(&self, msg: M);
}

/// 静态 receive（Actor trait 的静态参数版）。
pub trait TypedReceive<M>: Send + 'static {
    type Reply: Send + 'static;
    fn receive_typed(&mut self, msg: M) -> Result<Self::Reply, String>;
}

/// 静态 ref：包裹类型化通道，零 Any。
pub struct TypedActorRef<M, R> {
    tx: std::sync::mpsc::Sender<(M, Option<std::sync::mpsc::Sender<Result<R, String>>>)>,
    path: String,
    _marker: PhantomData<fn(M) -> R>,
}

impl<M: Send + 'static, R: Send + 'static> TypedAskRef<M> for TypedActorRef<M, R> {
    type Reply = R;
    fn ask(&self, msg: M) -> Result<R, String> {
        let (rtx, rrx) = std::sync::mpsc::channel();
        self.tx.send((msg, Some(rtx))).map_err(|e| e.to_string())?;
        rrx.recv().map_err(|e| e.to_string())?
    }
    fn tell(&self, msg: M) {
        let _ = self.tx.send((msg, None));
    }
}

impl<M, R> TypedActorRef<M, R> {
    pub fn path(&self) -> &str { &self.path }
}

// ============ 动态轨（现状不变） ============

pub trait DynActorRef: Send + Sync {
    fn send_dyn(&self, msg: BoxedMessage) -> BoxedResult;
    fn path(&self) -> String;
}

// ============ 引擎：双轨 spawn ============

/// 最小引擎：线程 + mpsc。两种 spawn 各走各的通道类型。
pub struct SimpleEngine {
    actors: Mutex<HashMap<String, ()>>, // 路径占用表
}

impl SimpleEngine {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { actors: Mutex::new(HashMap::new()) })
    }

    /// 静态 spawn：TypedReceive actor，全程零 Any。
    pub fn spawn_typed<A, M>(
        &self,
        mut actor: A,
        path: &str,
    ) -> Result<TypedActorRef<M, A::Reply>, String>
    where
        A: TypedReceive<M> + Send + 'static,
        M: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel::<(M, Option<std::sync::mpsc::Sender<Result<A::Reply, String>>>)>();
        std::thread::spawn(move || {
            for (msg, rtx) in rx {
                let reply = actor.receive_typed(msg);
                if let Some(rtx) = rtx {
                    let _ = rtx.send(reply);
                }
            }
        });
        self.actors.lock().unwrap().insert(path.to_string(), ());
        Ok(TypedActorRef { tx, path: path.to_string(), _marker: PhantomData })
    }

    /// 动态 spawn（现状路径，Box<dyn Any>）。
    pub fn spawn_dyn(
        &self,
        mut actor: Box<dyn FnMut(BoxedMessage) -> BoxedResult + Send>,
        path: &str,
    ) -> Result<Arc<dyn DynActorRef>, String> {
        let (tx, rx) = std::sync::mpsc::channel::<(BoxedMessage, Option<std::sync::mpsc::Sender<BoxedResult>>)>();
        std::thread::spawn(move || {
            for (msg, rtx) in rx {
                let r = actor(msg);
                if let Some(rtx) = rtx {
                    let _ = rtx.send(r);
                }
            }
        });
        self.actors.lock().unwrap().insert(path.to_string(), ());
        Ok(Arc::new(DynRef { tx, path: path.to_string() }))
    }
}

struct DynRef {
    tx: std::sync::mpsc::Sender<(BoxedMessage, Option<std::sync::mpsc::Sender<BoxedResult>>)>,
    path: String,
}

impl DynActorRef for DynRef {
    fn send_dyn(&self, msg: BoxedMessage) -> BoxedResult {
        let (rtx, rrx) = std::sync::mpsc::channel();
        self.tx.send((msg, Some(rtx))).map_err(|e| e.to_string())?;
        rrx.recv().map_err(|e| e.to_string())?
    }
    fn path(&self) -> String { self.path.clone() }
}

// ============ 双轨桥接（显式、一次性装箱） ============

/// 把静态 ref 桥接到动态接口：调用方提供 M 的编解码（显式装箱点）。
pub struct TypedToDyn<M, R> {
    tx: std::sync::mpsc::Sender<(M, Option<std::sync::mpsc::Sender<Result<R, String>>>)>,
    path: String,
    encode: fn(BoxedMessage) -> Result<M, String>,
    decode: fn(R) -> BoxedMessage,
}

impl<M: Send + 'static, R: Send + 'static> DynActorRef for TypedToDyn<M, R> {
    fn send_dyn(&self, msg: BoxedMessage) -> BoxedResult {
        let m = (self.encode)(msg)?;
        let (rtx, rrx) = std::sync::mpsc::channel();
        self.tx.send((m, Some(rtx))).map_err(|e| e.to_string())?;
        let r = rrx.recv().map_err(|e| e.to_string())?;
        r.map(|x| (self.decode)(x))
    }
    fn path(&self) -> String { self.path.clone() }
}

impl<M: Send + 'static, R: Send + 'static> TypedActorRef<M, R> {
    /// 桥接到动态轨（装箱发生在 encode/decode 边界，各一次）。
    pub fn into_dyn(
        self,
        encode: fn(BoxedMessage) -> Result<M, String>,
        decode: fn(R) -> BoxedMessage,
    ) -> Arc<dyn DynActorRef> {
        Arc::new(TypedToDyn { tx: self.tx, path: self.path, encode, decode })
    }
}

// ============ 便捷 spawn（宏展开后的形态，POC 用类型推断函数达到同等便利） ============

impl SimpleEngine {
    /// 一行完成 TypedReceive actor 的静态 spawn。
    pub fn actor<A, M>(&self, actor: A, path: &str) -> Result<TypedActorRef<M, A::Reply>, String>
    where
        A: TypedReceive<M> + Send + 'static,
        M: Send + 'static,
    {
        self.spawn_typed(actor, path)
    }
}
