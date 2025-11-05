extern crate api_neutral as parrot_api;
use derive_decouple::ParrotActorNeutral;
#[derive(ParrotActorNeutral)]
struct Counter;
impl parrot_api::ActorBehaviour for Counter {
    fn receive(&mut self,msg:parrot_api::BoxedMessage)->parrot_api::BoxedResult{Ok(msg)}
}
fn spawn(engine:&dyn parrot_api::EngineRuntime) {let _=Counter.spawn_on(engine,"counter");}
fn main(){let _=spawn;}
