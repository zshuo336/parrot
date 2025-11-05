use derive_decouple::ParrotActorNeutral;
#[derive(ParrotActorNeutral)]
#[neutral(crate = "renamed_api")]
struct Counter;
impl renamed_api::ActorBehaviour for Counter {
    fn receive(&mut self,msg:renamed_api::BoxedMessage)->renamed_api::BoxedResult{Ok(msg)}
}
fn main(){let value=renamed_api::ErasedActor::receive(&mut Counter,Box::new(42u64)).unwrap();assert_eq!(*value.downcast::<u64>().unwrap(),42);}
