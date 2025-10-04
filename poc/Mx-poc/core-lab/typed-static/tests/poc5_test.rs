//! POC 5 测试：静态轨 ask/tell 零 Any、双轨并存、桥接装箱边界。

use typed_static::*;

struct Add(u64);
struct Get;

/// 静态 actor：TypedReceive<Add> 与 TypedReceive<Get> 双实现。
struct Calc { n: u64 }

impl TypedReceive<Add> for Calc {
    type Reply = u64;
    fn receive_typed(&mut self, msg: Add) -> Result<u64, String> {
        self.n += msg.0;
        Ok(self.n)
    }
}

impl TypedReceive<Get> for Calc {
    type Reply = u64;
    fn receive_typed(&mut self, _msg: Get) -> Result<u64, String> {
        Ok(self.n)
    }
}

#[test]
fn static_track_zero_any() {
    let engine = SimpleEngine::new();

    // 静态 spawn（宏展开后的形态：engine.actor(...)）
    let add_ref: TypedActorRef<Add, u64> = engine.actor(Calc { n: 0 }, "/calc").unwrap();
    let get_ref: TypedActorRef<Get, u64> = {
        // 同一 actor 需要第二个消息类型的 ref：POC 用第二个 spawn 演示双轨并存；
        // 真实设计见文档（一个 actor 多协议 ref 由宏分派）。
        let r: TypedActorRef<Get, u64> = engine.actor(Calc { n: 0 }, "/calc2").unwrap();
        r
    };

    // 静态 ask：消息直接 mpsc，零 Any 装箱零 downcast
    assert_eq!(add_ref.ask(Add(41)).unwrap(), 41, "0+41=41（第二次 ask 才是 82）");
    assert_eq!(add_ref.ask(Add(41)).unwrap(), 82);
    assert_eq!(get_ref.ask(Get).unwrap(), 0, "第二个 spawn 是独立实例");

    // 静态 tell
    add_ref.tell(Add(1));
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(add_ref.ask(Add(0)).unwrap(), 83, "tell 后再 ask 应看到累积"); // Add(0) 不改变值
}

#[test]
fn dual_track_bridge() {
    let engine = SimpleEngine::new();
    let add_ref: TypedActorRef<Add, u64> = engine.actor(Calc { n: 0 }, "/calc3").unwrap();

    // 桥接到动态轨：装箱只发生在 encode/decode 边界（各一次）
    let dyn_ref = add_ref.into_dyn(
        |m| m.downcast::<Add>().map(|a| *a).map_err(|_| "type mismatch".to_string()),
        |r: u64| Box::new(r) as BoxedMessage,
    );
    let r = dyn_ref.send_dyn(Box::new(Add(1))).unwrap();
    assert_eq!(*r.downcast::<u64>().unwrap(), 1);
}

#[test]
fn dyn_track_baseline() {
    let engine = SimpleEngine::new();
    let r = engine
        .spawn_dyn(
            Box::new(|msg: BoxedMessage| {
                msg.downcast_ref::<u64>()
                    .map(|n| Ok(Box::new(n + 1) as BoxedMessage))
                    .unwrap_or(Err("unsupported".into()))
            }),
            "/legacy",
        )
        .unwrap();
    let out = r.send_dyn(Box::new(41u64)).unwrap();
    assert_eq!(*out.downcast::<u64>().unwrap(), 42);
}
