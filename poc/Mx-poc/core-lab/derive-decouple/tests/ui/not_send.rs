extern crate api_neutral as parrot_api;
use derive_decouple::ParrotActorNeutral;
#[derive(ParrotActorNeutral)]
struct Local(std::rc::Rc<u64>);
impl parrot_api::ActorBehaviour for Local{fn receive(&mut self,msg:parrot_api::BoxedMessage)->parrot_api::BoxedResult{Ok(msg)}}
fn main(){}
