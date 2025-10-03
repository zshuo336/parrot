//! POC 1 测试：derive 解耦验证。
//!
//! 断言：
//! 1. 宏生成代码只引用 `::parrot_api::`（规范层），零引擎符号
//! 2. 同一 actor 类型可在两个形态迥异的引擎上运行（框架的框架）

// 符号绑定：把规范层 crate 暴露为 parrot_api（正式版里 parrot-api 就是独立 crate）
extern crate api_neutral as parrot_api;
use parrot_api::{ActorRefErased, BoxedMessage, ErasedActor, EngineRuntime};

// 引入被测宏
use derive_decouple::ParrotActorNeutral;

// ---- 两个形态迥异的假想引擎（证明零符号耦合） ----

struct MockNativeEngine {
    _p: (),
}

type MailTx = std::sync::mpsc::Sender<(BoxedMessage, Option<std::sync::mpsc::Sender<Result<BoxedMessage, String>>>)>;

impl MockNativeEngine {
    fn new() -> Self { Self { _p: () } }
}

impl EngineRuntime for MockNativeEngine {
    fn name(&self) -> &'static str { "mock-native" }
    fn spawn_erased(&self, mut actor: Box<dyn ErasedActor>, path: &str) -> Result<parrot_api::BoxedActorRef, String> {
        let (tx, rx) = std::sync::mpsc::channel::<(BoxedMessage, Option<std::sync::mpsc::Sender<Result<BoxedMessage, String>>>)>();
        std::thread::spawn(move || {
            while let Ok((msg, reply)) = rx.recv() {
                let result = actor.receive(msg);
                if let Some(r) = reply {
                    let _ = r.send(result);
                }
            }
        });
        Ok(std::sync::Arc::new(NativeRef { tx, path: path.to_string() }))
    }
}

struct NativeRef { tx: MailTx, path: String }
impl ActorRefErased for NativeRef {
    fn tell(&self, msg: BoxedMessage) { let _ = self.tx.send((msg, None)); }
    fn ask(&self, msg: BoxedMessage) -> Result<BoxedMessage, String> {
        let (rtx, rrx) = std::sync::mpsc::channel();
        self.tx.send((msg, Some(rtx))).map_err(|e| e.to_string())?;
        rrx.recv().map_err(|e| e.to_string())?
    }
    fn path(&self) -> String { self.path.clone() }
}

/// 第二引擎：故意不同形状（如"协程式"批量 drain）。
struct MockFiberEngine { delegate: MockNativeEngine }
impl MockFiberEngine {
    fn new() -> Self { Self { delegate: MockNativeEngine::new() } }
}
impl EngineRuntime for MockFiberEngine {
    fn name(&self) -> &'static str { "mock-fiber" }
    fn spawn_erased(&self, actor: Box<dyn ErasedActor>, path: &str) -> Result<parrot_api::BoxedActorRef, String> {
        // 形状不同（内部 delegate），对外同一规范接口
        self.delegate.spawn_erased(actor, &format!("fiber:{}", path))
    }
}

// ---- 业务 actor：宏生成 spawn_on / ErasedActor，零引擎 import ----

#[derive(ParrotActorNeutral)]
struct Counter { n: u64 }

impl parrot_api::ActorBehaviour for Counter {
    fn receive(&mut self, msg: BoxedMessage) -> Result<BoxedMessage, String> {
        if let Some(inc) = msg.downcast_ref::<u64>() {
            self.n += inc;
            Ok(Box::new(self.n))
        } else {
            Err("unsupported".into())
        }
    }
}

#[derive(ParrotActorNeutral)]
struct DerivedCounter { n: u64 }

impl parrot_api::ActorBehaviour for DerivedCounter {
    fn receive(&mut self, msg: BoxedMessage) -> Result<BoxedMessage, String> {
        if let Some(inc) = msg.downcast_ref::<u64>() {
            self.n += inc;
            Ok(Box::new(self.n))
        } else {
            Err("unsupported".into())
        }
    }
}

#[test]
fn poc1_derive_is_engine_neutral() {
    let native = MockNativeEngine::new();
    let fiber = MockFiberEngine::new();

    // 手写桥
    let r1 = native.spawn_erased(Box::new(Counter { n: 0 }), "/c1").unwrap();
    assert_eq!(*r1.ask(Box::new(1u64)).unwrap().downcast::<u64>().unwrap(), 1);

    // 宏生成的中立 spawn_on：同一类型跑两个引擎
    let d1 = DerivedCounter { n: 0 }.spawn_on(&native, "/d1").unwrap();
    let d2 = DerivedCounter { n: 100 }.spawn_on(&fiber, "/d2").unwrap();

    assert_eq!(*d1.ask(Box::new(1u64)).unwrap().downcast::<u64>().unwrap(), 1);
    assert_eq!(*d2.ask(Box::new(1u64)).unwrap().downcast::<u64>().unwrap(), 101);
    assert_eq!(d2.path(), "fiber:/d2", "第二引擎保持自身形状");
}
