use derive_decouple::ParrotActorNeutral;
#[derive(Default,ParrotActorNeutral)]
#[neutral(crate="renamed_api",lifecycle,binding="counter")]
struct Counter(u64);
impl renamed_api::ActorBehaviour for Counter{fn receive(&mut self,_:renamed_api::BoxedMessage)->renamed_api::BoxedResult{Ok(Box::new(self.0))}}
impl renamed_api::Lifecycle for Counter{fn pre_start(&mut self)->Result<(),String>{self.0=42;Ok(())}fn post_stop(&mut self){self.0=0;}}
fn main(){let registry=renamed_api::BindingRegistry::new();let token=Counter::register_binding(&registry).unwrap();assert_eq!(registry.list(),vec!["counter"]);drop(token);assert!(registry.list().is_empty());let mut actor=Counter::default();renamed_api::ErasedActor::pre_start(&mut actor).unwrap();assert_eq!(*renamed_api::ErasedActor::receive(&mut actor,Box::new(())).unwrap().downcast::<u64>().unwrap(),42);renamed_api::ErasedActor::post_stop(&mut actor);assert_eq!(actor.0,0);}
