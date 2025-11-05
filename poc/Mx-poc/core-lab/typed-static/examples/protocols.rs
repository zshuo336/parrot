//! Different request/reply types, one actor, one mailbox, shared state.
use typed_static::{typed_actor, SimpleEngine, TypedAskRef, TypedReceive};
struct Add(u64);
struct Get;
struct Describe;
struct Counter(u64);
impl TypedReceive<Add> for Counter {
    type Reply = u64;
    fn receive_typed(&mut self, m: Add) -> Result<u64, String> {
        self.0 += m.0;
        Ok(self.0)
    }
}
impl TypedReceive<Get> for Counter {
    type Reply = u64;
    fn receive_typed(&mut self, _: Get) -> Result<u64, String> {
        Ok(self.0)
    }
}
impl TypedReceive<Describe> for Counter {
    type Reply = String;
    fn receive_typed(&mut self, _: Describe) -> Result<String, String> {
        Ok(format!("counter={}", self.0))
    }
}
typed_actor! {Request,Reply,CounterActor{Add(Add)=>u64,Get(Get)=>u64,Describe(Describe)=>String}}
fn main() {
    let engine = SimpleEngine::new();
    let actor = engine.actor(CounterActor(Counter(0)), "counter").unwrap();
    let add = actor.for_message::<Add>();
    let get = actor.for_message::<Get>();
    let describe = actor.for_message::<Describe>();
    add.tell(Add(40)).unwrap();
    assert_eq!(get.ask(Get).unwrap(), 40);
    assert_eq!(add.ask(Add(2)).unwrap(), 42);
    assert_eq!(describe.ask(Describe).unwrap(), "counter=42");
    println!(
        "Add/Get/Describe share one actor: {}",
        describe.ask(Describe).unwrap()
    );
    assert_eq!(add.generation(), get.generation());
    engine.shutdown().unwrap();
    assert_eq!(engine.worker_counts(), (1, 1));
    assert!(get.ask(Get).is_err());
    println!("one worker joined; all protocol references now reject work");
}
