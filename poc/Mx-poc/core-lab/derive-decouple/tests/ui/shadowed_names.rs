extern crate api_neutral as parrot_api;
use derive_decouple::ParrotActorNeutral;
struct Box;
type Result = u8;
#[derive(ParrotActorNeutral)]
struct Generic<__ParrotEngine: Send + 'static>(__ParrotEngine);
impl<T:Send+'static> parrot_api::ActorBehaviour for Generic<T>{fn receive(&mut self,msg:parrot_api::BoxedMessage)->parrot_api::BoxedResult{Ok(msg)}}
fn forward(engine:&dyn parrot_api::EngineRuntime){let _=Generic(42u64).spawn_on(engine,"a");}
fn main(){let _:Result=1;let _=Box;let _=forward;}
