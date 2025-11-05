extern crate api_neutral as parrot_api;
use derive_decouple::ParrotActorNeutral;
#[derive(ParrotActorNeutral)]
#[neutral(lifecycle)]
struct Missing;
impl parrot_api::ActorBehaviour for Missing{fn receive(&mut self,msg:parrot_api::BoxedMessage)->parrot_api::BoxedResult{Ok(msg)}}
fn main(){}
