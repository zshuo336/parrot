//! Static messages stay typed; protocol ports below share ONE actor state.
use typed_static::*;
struct Add(u64);
struct Get;
struct Describe;
struct Calc {
    n: u64,
}
impl TypedReceive<Add> for Calc {
    type Reply = u64;
    fn receive_typed(&mut self, msg: Add) -> Result<u64, String> {
        self.n += msg.0;
        Ok(self.n)
    }
}
impl TypedReceive<Get> for Calc {
    type Reply = u64;
    fn receive_typed(&mut self, _: Get) -> Result<u64, String> {
        Ok(self.n)
    }
}
impl TypedReceive<Describe> for Calc {
    type Reply = String;
    fn receive_typed(&mut self, _: Describe) -> Result<String, String> {
        Ok(format!("counter={}", self.n))
    }
}
typed_actor! { Request, Reply, CalcActor { Add(Add) => u64, Get(Get) => u64, Describe(Describe) => String } }
#[test]
fn static_protocols_share_one_actor_without_any_erasure() {
    let engine = SimpleEngine::new();
    let actor = engine.actor(CalcActor(Calc { n: 0 }), "calc").unwrap();
    let add = actor.for_message::<Add>();
    let get = actor.for_message::<Get>();
    let describe = actor.for_message::<Describe>();
    assert_eq!(add.generation(), get.generation());
    assert_eq!(add.path(), get.path());
    assert_eq!(add.ask(Add(41)).unwrap(), 41);
    assert_eq!(add.ask(Add(41)).unwrap(), 82);
    assert_eq!(get.ask(Get).unwrap(), 82);
    add.tell(Add(1)).unwrap();
    assert_eq!(get.ask(Get).unwrap(), 83); // same mailbox FIFO is the barrier
    assert_eq!(describe.ask(Describe).unwrap(), "counter=83");
    drop(actor);
    assert_eq!(get.ask(Get).unwrap(), 83); // ports retain the same owner
    engine.shutdown().unwrap();
    assert_eq!(engine.worker_counts(), (1, 1));
    let rejected = add.tell(Add(7)).unwrap_err();
    assert_eq!(rejected.message.0, 7);
}
#[test]
fn dual_track_bridge_shares_state_identity_and_retirement() {
    let engine = SimpleEngine::new();
    let reference = engine.actor(Calc { n: 0 }, "calc").unwrap();
    let dynamic = reference.clone().into_dyn(
        |m| {
            m.downcast::<Add>()
                .map(|a| *a)
                .map_err(|_| "type mismatch".into())
        },
        |r| Box::new(r),
    );
    assert_eq!(reference.ask(Add(41)).unwrap(), 41);
    assert_eq!(
        *dynamic
            .send_dyn(Box::new(Add(1)))
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    assert!(dynamic.send_dyn(Box::new(Get)).is_err());
    assert_eq!(reference.ask(Add(0)).unwrap(), 42);
    assert_eq!(dynamic.generation(), reference.generation());
    assert_eq!(dynamic.path(), reference.path());
    dynamic.stop();
    dynamic
        .wait_stopped(std::time::Duration::from_secs(3))
        .unwrap();
    assert!(reference.ask(Add(1)).is_err());
    assert_eq!(engine.alive_count(), 0);
    engine.shutdown().unwrap();
    assert_eq!(engine.worker_counts(), (1, 1));
}
#[test]
fn dyn_track_baseline_uses_the_owned_engine() {
    let engine = SimpleEngine::new();
    let reference = engine
        .spawn_dyn(
            Box::new(|msg| {
                msg.downcast::<u64>()
                    .map(|n| Box::new(*n + 1) as BoxedMessage)
                    .map_err(|_| "unsupported".into())
            }),
            "legacy",
        )
        .unwrap();
    assert_eq!(
        *reference
            .send_dyn(Box::new(41u64))
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    engine.shutdown().unwrap();
    assert!(reference.send_dyn(Box::new(41u64)).is_err());
    assert_eq!(engine.worker_counts(), (1, 1));
}
